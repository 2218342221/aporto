#![cfg(feature = "test-fixtures")]
use aporto_core_client::{ClientOptions, CoreClient, CoreError, CoreProcessConfig};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

async fn spawn(options: ClientOptions) -> CoreClient {
    spawn_with_shutdown_timeout(options, Duration::from_millis(100)).await
}

async fn spawn_with_shutdown_timeout(mut options: ClientOptions, timeout: Duration) -> CoreClient {
    options.shutdown_timeout = timeout;
    CoreClient::spawn(
        CoreProcessConfig {
            binary: PathBuf::from(env!("CARGO_BIN_EXE_aporto-rpc-fixture")),
            config: "unused config ; $(no shell)".into(),
            state_dir: "unused state".into(),
            releases_dir: None,
        },
        options,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn concurrent_calls_are_correlated_and_rpc_errors_propagate() {
    let client = spawn(ClientOptions::default()).await;
    let (a, b) = tokio::join!(
        client.call::<Value>("echo", json!({"value":1})),
        client.call::<Value>("echo", json!({"value":2}))
    );
    assert_eq!(a.unwrap()["value"], 1);
    assert_eq!(b.unwrap()["value"], 2);
    assert!(matches!(
        client.call::<Value>("error", json!({})).await,
        Err(CoreError::Rpc { code: -32004, .. })
    ));
    client.shutdown().await.unwrap();
    assert!(!client.is_available());
}

#[tokio::test]
async fn request_timeout_and_pending_limit_are_bounded() {
    let client = spawn(ClientOptions {
        request_timeout: Duration::from_millis(150),
        max_pending: 1,
        ..Default::default()
    })
    .await;
    let worker = client.clone();
    let pending = tokio::spawn(async move { worker.call::<Value>("hang", json!({})).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(matches!(
        client.call::<Value>("echo", json!({})).await,
        Err(CoreError::Overloaded)
    ));
    assert!(matches!(pending.await.unwrap(), Err(CoreError::Timeout)));
    assert_eq!(
        client
            .call::<Value>("echo", json!({"still":"available"}))
            .await
            .unwrap()["still"],
        "available"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_exit_and_invalid_frames_fail_pending_calls() {
    for method in ["exit", "invalid", "oversize"] {
        let client = spawn(ClientOptions {
            max_frame_bytes: 1024,
            ..Default::default()
        })
        .await;
        let result = client.call::<Value>(method, json!({})).await;
        assert!(matches!(
            result,
            Err(CoreError::Unavailable | CoreError::Protocol(_))
        ));
        assert!(!client.is_available());
        client.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn wait_closed_wakes_when_the_child_exits_and_remains_ready() {
    let client = spawn(ClientOptions::default()).await;
    let waiting_client = client.clone();
    let waiter = tokio::spawn(async move { waiting_client.wait_closed().await });
    assert!(!waiter.is_finished());
    assert!(matches!(
        client.call::<Value>("exit", json!({})).await,
        Err(CoreError::Unavailable)
    ));
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(!client.is_available());
    tokio::time::timeout(Duration::from_secs(1), client.wait_closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelling_a_backpressured_call_does_not_corrupt_the_rpc_stream() {
    let client = spawn(ClientOptions {
        request_timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .await;
    let first = client.clone();
    let sleeping = tokio::spawn(async move { first.call::<Value>("slow", json!({})).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = client.clone();
    let cancelled = tokio::spawn(async move {
        second
            .call::<Value>("echo", json!({"large":"x".repeat(1024*1024)}))
            .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    sleeping.await.unwrap().unwrap();
    let next = client
        .call::<Value>("echo", json!({"healthy":true}))
        .await
        .unwrap();
    assert_eq!(next["healthy"], true);
    assert!(client.is_available());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_propagates_persistence_failure_after_reaping_the_child() {
    let client = spawn(ClientOptions::default()).await;
    client
        .call::<Value>("configure_shutdown", json!({"behavior":"error"}))
        .await
        .unwrap();
    assert!(matches!(
        client.shutdown().await,
        Err(CoreError::Rpc { code: -32603, .. })
    ));
    assert!(!client.is_available());
    // Every waiter receives the same completion, including callers arriving later.
    assert!(matches!(
        client.shutdown().await,
        Err(CoreError::Rpc { code: -32603, .. })
    ));
}

#[tokio::test]
async fn shutdown_uses_its_cleanup_window_instead_of_the_normal_request_timeout() {
    let client = spawn_with_shutdown_timeout(
        ClientOptions {
            request_timeout: Duration::from_millis(100),
            ..ClientOptions::default()
        },
        Duration::from_secs(1),
    )
    .await;
    client
        .call::<Value>("configure_shutdown", json!({"behavior":"slow"}))
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    assert!(!client.is_available());
}

#[tokio::test]
async fn cancelling_shutdown_waiter_does_not_abandon_unresponsive_child_cleanup() {
    let client = spawn(ClientOptions::default()).await;
    client
        .call::<Value>("configure_shutdown", json!({"behavior":"ignore"}))
        .await
        .unwrap();
    let waiter_client = client.clone();
    let waiter = tokio::spawn(async move { waiter_client.shutdown().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    waiter.abort();
    // Keep the owner alive and do not call shutdown again until the supervisor
    // has independently terminated the child at its original deadline.
    tokio::time::timeout(Duration::from_secs(2), async {
        while client.is_available() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(client.shutdown().await, Err(CoreError::Timeout)));
}
