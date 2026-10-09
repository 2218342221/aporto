use super::*;
use crate::{CoreLimits, ExecutionResult};

fn agent(id: &str) -> AgentSummary {
    AgentSummary {
        id: id.into(),
        name: id.into(),
        model: "test".into(),
        bundle_digest: "bundle".into(),
        release_id: "release".into(),
        runtime: RuntimeOptions {
            provider: "docker".into(),
            images: vec!["fixture:latest".into()],
            default_image: "fixture:latest".into(),
            default_workdir: "/workspace".into(),
        },
    }
}
fn start(store: &Store, thread: &Thread, key: &str) -> Turn {
    store
        .start_turn(
            &TurnStartParams {
                thread_id: thread.id.clone(),
                input: "work".into(),
                idempotency_key: key.into(),
            },
            key,
        )
        .unwrap()
}
fn events(store: &Store, thread: &Thread) -> usize {
    store
        .events(EventListParams {
            thread_id: thread.id.clone(),
            after: None,
            limit: None,
        })
        .unwrap()
        .events
        .len()
}

#[test]
fn runtime_ready_cannot_rebind_or_duplicate_a_materialized_sandbox() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), CoreLimits::default()).unwrap();
    let agent = agent("agent");
    let original = store.start_thread(&agent, "Original").unwrap();
    let turn = start(&store, &original, "first");
    store.begin(&turn.id).unwrap();
    store
        .emit(
            &original.id,
            &turn.id,
            "runtime.ready",
            json!({"sandbox_id":"owned-sandbox"}),
        )
        .unwrap();
    let before = events(&store, &original);
    assert!(
        store
            .emit(
                &original.id,
                &turn.id,
                "runtime.ready",
                json!({"sandbox_id":"different-sandbox"})
            )
            .is_err()
    );
    assert_eq!(events(&store, &original), before);
    assert_eq!(
        store.thread(&original.id).unwrap().sandbox_id.as_deref(),
        Some("owned-sandbox")
    );
    let separate = store.start_thread(&agent, "Separate").unwrap();
    let other = start(&store, &separate, "second");
    store.begin(&other.id).unwrap();
    let before = events(&store, &separate);
    assert!(
        store
            .emit(
                &separate.id,
                &other.id,
                "runtime.ready",
                json!({"sandbox_id":"owned-sandbox"})
            )
            .is_err()
    );
    assert_eq!(events(&store, &separate), before);
    assert!(store.thread(&separate.id).unwrap().sandbox_id.is_none());
    let foreign = self::agent("other-agent");
    let error = store
        .start_thread_selected(
            &foreign,
            "Wrong agent",
            Some(ThreadRuntimeParams::Reuse {
                instance_id: original.runtime_instance_id,
            }),
            None,
            |_, _, _| Some(foreign.runtime.clone()),
        )
        .unwrap_err();
    assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, NOT_FOUND);
}
#[test]
fn sqlite_prevents_shared_dispatch_and_restart_retains_one_instance() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), CoreLimits::default()).unwrap();
    let agent = agent("agent");
    let original = store.start_thread(&agent, "Original").unwrap();
    let first = start(&store, &original, "seed");
    store.begin(&first.id).unwrap();
    store
        .emit(
            &original.id,
            &first.id,
            "runtime.ready",
            json!({"sandbox_id":"owned-sandbox"}),
        )
        .unwrap();
    store
        .finish(
            &first.id,
            Some(&ExecutionResult {
                answer: "seed".into(),
                history: vec![],
            }),
            None,
        )
        .unwrap();
    let sibling = store
        .start_thread_selected(
            &agent,
            "Sibling",
            Some(ThreadRuntimeParams::Reuse {
                instance_id: original.runtime_instance_id.clone(),
            }),
            None,
            |_, _, _| Some(agent.runtime.clone()),
        )
        .unwrap();
    let active = start(&store, &original, "active");
    store.begin(&active.id).unwrap();
    let queued = start(&store, &sibling, "queued");
    assert!(store.begin(&queued.id).is_err());
    {
        let db = store.connection.lock().unwrap();
        // The database constraint also blocks an admission path that bypasses begin().
        assert!(
            db.execute(
                "UPDATE turns SET status='running' WHERE id=?1",
                [&queued.id]
            )
            .is_err()
        );
    }
    drop(store);
    let store = Store::open(directory.path(), CoreLimits::default()).unwrap();
    for (thread, turn_id) in [(&original, &active.id), (&sibling, &queued.id)] {
        let restored = store
            .read_thread(ThreadReadParams {
                thread_id: thread.id.clone(),
                limit: None,
                before: None,
            })
            .unwrap();
        assert_eq!(restored.thread.sandbox_id.as_deref(), Some("owned-sandbox"));
        assert_eq!(
            restored.thread.runtime_instance_id,
            original.runtime_instance_id
        );
        assert_eq!(
            restored
                .turns
                .iter()
                .find(|turn| &turn.id == turn_id)
                .unwrap()
                .status,
            TurnStatus::Interrupted
        );
    }
    let page = store
        .instances(
            RuntimeInstanceListParams {
                agent_id: agent.id.clone(),
                cursor: None,
                limit: None,
            },
            |_, _, _| Some(agent.runtime.clone()),
        )
        .unwrap();
    assert_eq!(page.instances.len(), 1);
    assert!(!page.instances[0].busy);
}
