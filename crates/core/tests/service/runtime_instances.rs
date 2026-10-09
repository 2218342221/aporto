use super::*;

async fn instances(core: &CoreService) -> RuntimeInstanceListResult {
    call(core, "runtime/instances", json!({"agent_id":"test"})).await
}
async fn reuse(core: &CoreService, source: &Thread, workdir: Option<&str>) -> Thread {
    call(core,"thread/start",json!({"agent_id":"test","runtime":{"mode":"reuse","instance_id":source.runtime_instance_id},"workdir":workdir})).await
}

#[tokio::test]
async fn selections_are_validated_and_new_instances_are_lazy() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(&directory, executor.clone(), CoreLimits::default()).await;
    let default = thread(&core).await;
    assert_eq!(default.runtime_provider, "docker");
    assert_eq!(default.runtime_image, "fixture:latest");
    assert_eq!(default.workdir, "/workspace");
    assert!(default.sandbox_id.is_none());
    assert!(instances(&core).await.instances.is_empty());
    let selected: Thread = call(&core,"thread/start",json!({"agent_id":"test","runtime":{"mode":"new","image":"fixture:alternate"},"workdir":"/work/project/"})).await;
    assert_eq!(selected.runtime_image, "fixture:alternate");
    assert_eq!(selected.workdir, "/work/project");
    assert_ne!(selected.runtime_instance_id, default.runtime_instance_id);
    for params in [
        json!({"agent_id":"test","runtime":{"mode":"new","image":"untrusted:latest"}}),
        json!({"agent_id":"test","workdir":"relative"}),
        json!({"agent_id":"test","workdir":"/work/../etc"}),
        json!({"agent_id":"test","runtime":{"mode":"reuse","instance_id":default.runtime_instance_id,"image":"fixture:alternate"}}),
    ] {
        assert_eq!(
            response(&core, "thread/start", params)
                .await
                .error
                .unwrap()
                .code,
            INVALID_PARAMS
        );
    }
    for instance in [&default.runtime_instance_id, "not-owned"] {
        assert_eq!(
            response(
                &core,
                "thread/start",
                json!({"agent_id":"test","runtime":{"mode":"reuse","instance_id":instance}})
            )
            .await
            .error
            .unwrap()
            .code,
            NOT_FOUND
        );
    }
    let turn = start(&core, &selected, "seed", "seed").await;
    assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Completed);
    let observed = executor.contexts.lock().unwrap()[0].clone();
    assert_eq!(observed.runtime_instance_id, selected.runtime_instance_id);
    assert_eq!(observed.runtime_image, selected.runtime_image);
    assert_eq!(observed.runtime_provider, selected.runtime_provider);
    assert_eq!(observed.workdir, selected.workdir);
    let page = instances(&core).await;
    assert_eq!(page.instances.len(), 1);
    assert_eq!(page.instances[0].id, selected.runtime_instance_id);
    assert_eq!(page.instances[0].workdir, selected.workdir);
    assert!(!page.instances[0].busy);
    assert!(!core.failure_token().is_cancelled());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn reused_instances_share_identity_but_keep_history_and_workdirs_independent() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(&directory, executor.clone(), CoreLimits::default()).await;
    let original = thread(&core).await;
    let first = start(&core, &original, "seed", "seed").await;
    terminal(&core, &first).await;
    let original = read(&core, &original.id).await.thread;
    let sibling = reuse(&core, &original, Some("/work/second")).await;
    let third = reuse(&core, &original, None).await;
    assert_ne!(original.id, sibling.id);
    assert_eq!(sibling.runtime_instance_id, original.runtime_instance_id);
    assert_eq!(sibling.sandbox_id, original.sandbox_id);
    assert_eq!(sibling.release_id, original.release_id);
    assert_eq!(sibling.workdir, "/work/second");
    assert_eq!(third.workdir, original.workdir);
    assert!(read(&core, &sibling.id).await.turns.is_empty());
    let own = start(&core, &sibling, "independent", "one").await;
    terminal(&core, &own).await;
    let next = start(&core, &original, "continued", "two").await;
    terminal(&core, &next).await;
    let contexts = executor.contexts.lock().unwrap().clone();
    assert!(contexts[1].history.is_empty());
    assert_eq!(contexts[1].sandbox_id, original.sandbox_id);
    assert_eq!(contexts[1].workdir, "/work/second");
    assert_eq!(contexts[2].history.len(), 2);
    assert_eq!(contexts[2].history[0]["content"], "seed");
    assert!(
        contexts[2]
            .history
            .iter()
            .all(|item| item["content"] != "independent")
    );
    let listed: ThreadListResult = call(&core, "thread/list", json!({})).await;
    assert!(
        listed
            .threads
            .iter()
            .all(|thread| thread.sandbox_id == original.sandbox_id)
    );
    core.shutdown().await.unwrap();
    drop(core);
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let restored = read(&core, &sibling.id).await;
    assert_eq!(
        restored.thread.runtime_instance_id,
        original.runtime_instance_id
    );
    assert_eq!(restored.thread.sandbox_id, original.sandbox_id);
    assert_eq!(restored.turns.len(), 1);
    assert_eq!(instances(&core).await.instances.len(), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn busy_shared_instances_are_serialized_without_blocking_independent_workers() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(
        &directory,
        executor.clone(),
        CoreLimits {
            max_concurrent: 2,
            ..Default::default()
        },
    )
    .await;
    let original = thread(&core).await;
    terminal(&core, &start(&core, &original, "seed", "seed").await).await;
    let sibling = reuse(&core, &original, None).await;
    let independent = thread(&core).await;
    let active = start(&core, &original, "block", "active").await;
    wait_calls(&executor, 2).await;
    let queued = start(&core, &sibling, "second", "queued").await;
    // Attaching another thread while busy is supported and does not execute.
    let attached = reuse(&core, &original, None).await;
    assert_eq!(attached.runtime_instance_id, original.runtime_instance_id);
    let parallel = start(&core, &independent, "block", "parallel").await;
    wait_calls(&executor, 3).await;
    assert_eq!(
        read(&core, &sibling.id).await.turns[0].status,
        TurnStatus::Queued
    );
    assert_eq!(
        read(&core, &independent.id).await.turns[0].status,
        TurnStatus::Running
    );
    assert!(
        instances(&core)
            .await
            .instances
            .iter()
            .find(|item| item.id == original.runtime_instance_id)
            .unwrap()
            .busy
    );
    executor.permits.add_permits(2);
    assert_eq!(terminal(&core, &active).await.status, TurnStatus::Completed);
    assert_eq!(
        terminal(&core, &parallel).await.status,
        TurnStatus::Completed
    );
    assert_eq!(terminal(&core, &queued).await.status, TurnStatus::Completed);
    let contexts = executor.contexts.lock().unwrap().clone();
    assert_eq!(contexts[3].input, "second");
    assert_eq!(contexts[3].sandbox_id, contexts[1].sandbox_id);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 4);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_cancellation_and_running_cleanup_keep_instance_admission_correct() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(
        &directory,
        executor.clone(),
        CoreLimits {
            max_concurrent: 3,
            ..Default::default()
        },
    )
    .await;
    let original = thread(&core).await;
    terminal(&core, &start(&core, &original, "seed", "seed").await).await;
    let sibling = reuse(&core, &original, None).await;
    let third = reuse(&core, &original, None).await;
    let active = start(&core, &original, "cancel-then-cleanup", "active").await;
    wait_calls(&executor, 2).await;
    let removed = start(&core, &sibling, "must-not-run", "queued").await;
    let waiting = start(&core, &third, "after-cleanup", "waiting").await;
    let interrupted: Turn = call(
        &core,
        "turn/interrupt",
        json!({"thread_id":sibling.id,"turn_id":removed.id}),
    )
    .await;
    assert_eq!(interrupted.status, TurnStatus::Interrupted);
    let cancelling: Turn = call(
        &core,
        "turn/interrupt",
        json!({"thread_id":original.id,"turn_id":active.id}),
    )
    .await;
    assert_eq!(cancelling.status, TurnStatus::Cancelling);
    // The fake executor waits for explicit cleanup completion after cancellation.
    assert_eq!(
        read(&core, &third.id).await.turns[0].status,
        TurnStatus::Queued
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert!(instances(&core).await.instances[0].busy);
    executor.permits.add_permits(1);
    assert_eq!(
        terminal(&core, &active).await.status,
        TurnStatus::Interrupted
    );
    assert_eq!(
        terminal(&core, &waiting).await.status,
        TurnStatus::Completed
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 3);
    assert!(
        executor
            .contexts
            .lock()
            .unwrap()
            .iter()
            .all(|context| context.input != "must-not-run")
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn instance_pages_and_archived_release_reuse_preserve_original_configuration() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let mut created = Vec::new();
    for i in 0..3 {
        let thread = thread(&core).await;
        terminal(
            &core,
            &start(&core, &thread, "seed", &format!("seed-{i}")).await,
        )
        .await;
        created.push(thread);
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page: RuntimeInstanceListResult = call(
            &core,
            "runtime/instances",
            json!({"agent_id":"test","limit":1,"cursor":cursor}),
        )
        .await;
        assert_eq!(page.instances.len(), 1);
        seen.push(page.instances[0].id.clone());
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3);
    for limit in [0, 101] {
        assert_eq!(
            response(
                &core,
                "runtime/instances",
                json!({"agent_id":"test","limit":limit})
            )
            .await
            .error
            .unwrap()
            .code,
            INVALID_PARAMS
        );
    }
    assert_eq!(
        response(
            &core,
            "runtime/instances",
            json!({"agent_id":"test","cursor":"missing"})
        )
        .await
        .error
        .unwrap()
        .code,
        NOT_FOUND
    );
    core.shutdown().await.unwrap();
    drop(core);
    let mut replacement = FakeExecutor::new();
    let mutable = Arc::get_mut(&mut replacement).unwrap();
    mutable.archive = Some(created[0].release_id.clone());
    mutable.digest = format!("sha256:{}", "b".repeat(64));
    let core = initialized(&directory, replacement.clone(), CoreLimits::default()).await;
    let reused = reuse(&core, &created[0], None).await;
    assert_eq!(reused.release_id, created[0].release_id);
    assert_ne!(reused.release_id, thread(&core).await.release_id);
    terminal(
        &core,
        &start(&core, &reused, "old-release", "old-release").await,
    )
    .await;
    assert_eq!(
        replacement.contexts.lock().unwrap()[0].release_id,
        created[0].release_id
    );
    assert_eq!(instances(&core).await.instances.len(), 3);
    core.shutdown().await.unwrap();
    drop(core);
    let mut unavailable = FakeExecutor::new();
    Arc::get_mut(&mut unavailable).unwrap().digest = format!("sha256:{}", "b".repeat(64));
    let core = initialized(&directory, unavailable, CoreLimits::default()).await;
    assert!(instances(&core).await.instances.is_empty());
    assert_eq!(response(&core,"thread/start",json!({"agent_id":"test","runtime":{"mode":"reuse","instance_id":created[0].runtime_instance_id}})).await.error.unwrap().code,CONFLICT);
    assert!(!core.failure_token().is_cancelled());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_threads_keep_their_physical_container_and_collate_shared_identity() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let original = thread(&core).await;
    terminal(&core, &start(&core, &original, "seed", "seed").await).await;
    let original = read(&core, &original.id).await.thread;
    let sibling = reuse(&core, &original, None).await;
    core.shutdown().await.unwrap();
    drop(core);
    let db = rusqlite::Connection::open(directory.path().join("state.sqlite")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF;
        UPDATE threads SET sandbox_id=(SELECT sandbox_id FROM runtime_instances r WHERE r.id=threads.runtime_instance_id);
        DROP INDEX threads_instance;
        DROP INDEX one_running_turn_per_instance;
        ALTER TABLE turns DROP COLUMN runtime_instance_id;
        ALTER TABLE threads DROP COLUMN runtime_instance_id;
        ALTER TABLE threads DROP COLUMN workdir;
        DROP TABLE runtime_instances;
        PRAGMA user_version=2;").unwrap();
    drop(db);
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let restored = read(&core, &original.id).await;
    let attached = read(&core, &sibling.id).await;
    assert_eq!(restored.thread.sandbox_id, original.sandbox_id);
    assert_eq!(restored.thread.runtime_image, original.runtime_image);
    assert_eq!(restored.turns.len(), 1);
    assert_eq!(
        restored.thread.runtime_instance_id,
        attached.thread.runtime_instance_id
    );
    assert_eq!(instances(&core).await.instances.len(), 1);
    let instance_id = restored.thread.runtime_instance_id;
    core.shutdown().await.unwrap();
    drop(core);
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    assert_eq!(
        read(&core, &original.id).await.thread.runtime_instance_id,
        instance_id
    );
    core.shutdown().await.unwrap();
}
