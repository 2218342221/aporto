#![cfg(feature = "test-fixtures")]
//! Exercise the actual Server binary, its Core child, and open HTTP connections.
use reqwest::Client;
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, ChildStderr, Command},
};

const TOKEN: &str = "fixture-operator-token-at-least-32-bytes";
struct Server {
    child: Child,
    stderr: BufReader<ChildStderr>,
    url: String,
    http: Client,
}
impl Server {
    async fn start(mode: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_aporto-server"))
            .args([
                "--core-bin",
                env!("CARGO_BIN_EXE_aporto-server-core-fixture"),
                "--core-config",
                mode,
                "--state-dir",
                "unused",
                "--listen",
                "127.0.0.1:0",
            ])
            .env("APORTO_SERVER_TOKEN", TOKEN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), stderr.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let address = line
            .trim()
            .strip_prefix("Aporto Server listening on ")
            .unwrap_or_else(|| panic!("Server did not start: {line}"));
        Self {
            child,
            stderr,
            url: format!("http://{address}"),
            http: Client::builder().no_proxy().build().unwrap(),
        }
    }

    async fn stream(&self) -> reqwest::Response {
        let response = self
            .http
            .get(format!("{}/v1/threads/thread-1/events", self.url))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response
    }

    async fn assert_exit(mut self, success: bool) -> String {
        let status = tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .expect("Server did not exit within the HTTP drain bound")
            .unwrap();
        let mut stderr = String::new();
        self.stderr.read_to_string(&mut stderr).await.unwrap();
        assert_eq!(
            status.success(),
            success,
            "status={status}; stderr={stderr}"
        );
        stderr
    }
}

#[tokio::test]
async fn unexpected_core_exit_stops_sse_and_bounds_slow_http_drain() {
    let server = Server::start("exit-on-list").await;
    let mut stream = server.stream().await;
    let mut slow = TcpStream::connect(server.url.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    // The ordinary request deadline is 30 seconds. Core failure must still end
    // this process after the shorter, 10-second HTTP drain window.
    slow.write_all(format!("POST /v1/threads HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n").as_bytes()).await.unwrap();
    let mut interim = [0; 128];
    let received = tokio::time::timeout(Duration::from_secs(3), slow.read(&mut interim))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&interim[..received]).contains("100 Continue"));
    slow.write_all(b"{").await.unwrap();
    let _ = server
        .http
        .get(format!("{}/v1/agents", server.url))
        .bearer_auth(TOKEN)
        .send()
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await
    .expect("Core failure did not close the existing SSE stream");
    let stderr = server.assert_exit(false).await;
    assert!(stderr.contains("Core process exited unexpectedly"));
    drop(slow);
}

#[tokio::test]
async fn even_a_zero_status_core_exit_is_unexpected_without_shutdown() {
    let server = Server::start("exit-zero-on-list").await;
    let _ = server
        .http
        .get(format!("{}/v1/agents", server.url))
        .bearer_auth(TOKEN)
        .send()
        .await;
    assert!(
        server
            .assert_exit(false)
            .await
            .contains("Core process exited unexpectedly")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_closes_sse_and_shuts_down_core_successfully() {
    let server = Server::start("unused").await;
    let mut stream = server.stream().await;
    let status = Command::new("kill")
        .args(["-TERM", &server.child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await
    .expect("normal shutdown did not close SSE");
    server.assert_exit(true).await;
}
