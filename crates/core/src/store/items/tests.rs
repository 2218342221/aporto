use super::*;
use crate::{ExecutionResult, store::Store};

fn setup(limits: CoreLimits) -> (tempfile::TempDir, Store, Thread, Turn) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), limits).unwrap();
    let thread = store
        .start_thread(
            &AgentSummary {
                id: "agent".into(),
                name: "Agent".into(),
                model: "fixture".into(),
                bundle_digest: "bundle".into(),
                release_id: "release".into(),
                runtime: RuntimeOptions {
                    provider: "docker".into(),
                    images: vec!["fixture:latest".into()],
                    default_image: "fixture:latest".into(),
                    default_workdir: "/workspace".into(),
                },
            },
            "Transcript",
        )
        .unwrap();
    let turn = store
        .start_turn(
            &TurnStartParams {
                thread_id: thread.id.clone(),
                input: "work".into(),
                idempotency_key: "one".into(),
            },
            "hash",
        )
        .unwrap();
    store.begin(&turn.id).unwrap();
    (directory, store, thread, turn)
}
fn item(id: &str, kind: ItemKind) -> ItemContent {
    ItemContent {
        id: id.into(),
        kind,
        status: ItemStatus::InProgress,
        phase: None,
        name: None,
        text: Some("hello".into()),
        input: None,
        output: None,
        error: None,
        elapsed_ms: None,
        truncated: false,
    }
}
fn emit_item(
    store: &Store,
    thread: &Thread,
    turn: &Turn,
    kind: &str,
    item: &ItemContent,
) -> Result<()> {
    store.emit(&thread.id, &turn.id, kind, json!({"item":item}))
}
fn page(store: &Store, thread: &Thread, turn: &Turn, after: u64, limit: u32) -> ItemListResult {
    store
        .items(ItemListParams {
            thread_id: thread.id.clone(),
            turn_id: turn.id.clone(),
            after: Some(after),
            limit: Some(limit),
        })
        .unwrap()
}
fn events(store: &Store, thread: &Thread) -> Vec<Event> {
    store
        .events(EventListParams {
            thread_id: thread.id.clone(),
            after: None,
            limit: Some(500),
        })
        .unwrap()
        .events
}
fn success(store: &Store, turn: &Turn) {
    store
        .finish(
            &turn.id,
            Some(&ExecutionResult {
                answer: "done".into(),
                history: vec![json!({"role":"assistant","content":"done"})],
            }),
            None,
        )
        .unwrap();
}

#[test]
fn cumulative_projection_replays_identically_and_ordinal_survives_updates_and_restore() {
    let (directory, store, thread, turn) = setup(CoreLimits::default());
    let mut first = item("message", ItemKind::AssistantMessage);
    let mut second = item("call", ItemKind::ToolCall);
    emit_item(&store, &thread, &turn, "item.started", &first).unwrap();
    let first_page = page(&store, &thread, &turn, 0, 1);
    emit_item(&store, &thread, &turn, "item.started", &second).unwrap();
    first.text = Some("hello world".into());
    emit_item(&store, &thread, &turn, "item.updated", &first).unwrap();
    second.status = ItemStatus::Completed;
    second.output = Some("result".into());
    emit_item(&store, &thread, &turn, "item.completed", &second).unwrap();
    let continuation = page(&store, &thread, &turn, first_page.next_cursor, 1);
    assert_eq!(continuation.items[0].content.id, "call");
    assert!(!continuation.has_more);
    success(&store, &turn);
    let before = page(&store, &thread, &turn, 0, 100).items;
    assert_eq!(before[0].ordinal, first_page.next_cursor);
    assert!(before[0].sequence > before[1].ordinal);
    assert_eq!(before[0].content.status, ItemStatus::Completed);
    let mut replay = std::collections::BTreeMap::new();
    for event in events(&store, &thread)
        .into_iter()
        .filter(|event| event.kind.starts_with("item."))
    {
        let snapshot: TurnItem = serde_json::from_value(event.data["item"].clone()).unwrap();
        assert_eq!(event.sequence, snapshot.sequence);
        assert_eq!(event.turn_id.as_deref(), Some(snapshot.turn_id.as_str()));
        assert!(snapshot.ordinal > 0 && snapshot.ordinal <= snapshot.sequence);
        replay.insert(snapshot.ordinal, snapshot);
    }
    assert_eq!(before, replay.into_values().collect::<Vec<_>>());
    drop(store);
    let restored = Store::open(directory.path(), CoreLimits::default()).unwrap();
    assert_eq!(before, page(&restored, &thread, &turn, 0, 100).items);
    let empty = page(&restored, &thread, &turn, u32::MAX as u64, 100);
    assert!(empty.items.is_empty() && !empty.has_more);
    assert_eq!(empty.next_cursor, u32::MAX as u64);
}

#[test]
fn identity_terminal_and_size_validation_leave_both_event_and_projection_unchanged() {
    let (_directory, store, thread, turn) = setup(CoreLimits::default());
    let original = item("call", ItemKind::ToolCall);
    let mut invalid = original.clone();
    invalid.text = Some("x".repeat(MAX_FIELD_BYTES + 1));
    assert!(emit_item(&store, &thread, &turn, "item.started", &invalid).is_err());
    invalid = original.clone();
    invalid.text = Some("\0".repeat(MAX_FIELD_BYTES));
    invalid.output = invalid.text.clone();
    assert!(emit_item(&store, &thread, &turn, "item.started", &invalid).is_err());
    invalid = original.clone();
    invalid.id = "a".repeat(129);
    assert!(emit_item(&store, &thread, &turn, "item.started", &invalid).is_err());
    invalid = original.clone();
    invalid.phase = Some(ItemPhase::FinalAnswer);
    assert!(emit_item(&store, &thread, &turn, "item.started", &invalid).is_err());
    assert!(emit_item(&store, &thread, &turn, "item.updated", &original).is_err());
    let mut forged = json!({"item":original});
    forged["item"]["turn_id"] = json!(turn.id);
    assert!(
        store
            .emit(&thread.id, &turn.id, "item.started", forged)
            .is_err()
    );
    assert_eq!(events(&store, &thread).len(), 3); // thread, queued, started
    assert!(page(&store, &thread, &turn, 0, 100).items.is_empty());
    emit_item(&store, &thread, &turn, "item.started", &original).unwrap();
    let mut changed = original.clone();
    changed.kind = ItemKind::PtcCall;
    assert!(emit_item(&store, &thread, &turn, "item.updated", &changed).is_err());
    changed = original.clone();
    changed.status = ItemStatus::Completed;
    emit_item(&store, &thread, &turn, "item.completed", &changed).unwrap();
    let count = events(&store, &thread).len();
    emit_item(&store, &thread, &turn, "item.completed", &changed).unwrap();
    assert_eq!(events(&store, &thread).len(), count);
    assert!(emit_item(&store, &thread, &turn, "item.updated", &original).is_err());
    changed.text = Some("changed after completion".into());
    assert!(emit_item(&store, &thread, &turn, "item.completed", &changed).is_err());
    success(&store, &turn);
    let persisted = page(&store, &thread, &turn, 0, 100).items[0]
        .content
        .clone();
    emit_item(&store, &thread, &turn, "item.completed", &persisted).unwrap();
    assert!(
        emit_item(
            &store,
            &thread,
            &turn,
            "item.started",
            &item("late", ItemKind::ToolCall)
        )
        .is_err()
    );
    assert!(
        store
            .items(ItemListParams {
                thread_id: "other".into(),
                turn_id: turn.id.clone(),
                after: None,
                limit: None
            })
            .is_err()
    );
    let error = store
        .items(ItemListParams {
            thread_id: thread.id.clone(),
            turn_id: turn.id.clone(),
            after: Some(u64::MAX),
            limit: None,
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().unwrap().code,
        INVALID_PARAMS
    );
}

#[test]
fn projection_write_failure_rolls_back_event_sequence_and_thread_cursor() {
    let (_directory, store, thread, turn) = setup(CoreLimits::default());
    let before = events(&store, &thread);
    store.connection.lock().unwrap().execute_batch("CREATE TRIGGER refuse_items BEFORE INSERT ON turn_items BEGIN SELECT RAISE(ABORT,'injected item write failure'); END;").unwrap();
    assert!(
        emit_item(
            &store,
            &thread,
            &turn,
            "item.started",
            &item("atomic", ItemKind::ToolCall)
        )
        .is_err()
    );
    let after = events(&store, &thread);
    assert_eq!(before.len(), after.len());
    assert_eq!(
        store.thread(&thread.id).unwrap().last_sequence,
        before.last().unwrap().sequence
    );
    assert!(page(&store, &thread, &turn, 0, 100).items.is_empty());
    store
        .connection
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_items")
        .unwrap();
    emit_item(
        &store,
        &thread,
        &turn,
        "item.started",
        &item("atomic", ItemKind::ToolCall),
    )
    .unwrap();
    assert_eq!(
        page(&store, &thread, &turn, 0, 100).items[0].sequence,
        before.last().unwrap().sequence + 1
    );
}

#[test]
fn terminalization_failure_rolls_back_all_items_and_the_turn() {
    let (_directory, store, thread, turn) = setup(CoreLimits::default());
    for id in ["first", "second"] {
        emit_item(
            &store,
            &thread,
            &turn,
            "item.started",
            &item(id, ItemKind::ToolCall),
        )
        .unwrap();
    }
    let before = page(&store, &thread, &turn, 0, 100).items;
    let event_count = events(&store, &thread).len();
    store.connection.lock().unwrap().execute_batch("CREATE TRIGGER refuse_second BEFORE INSERT ON turn_items WHEN NEW.id='second' BEGIN SELECT RAISE(ABORT,'injected terminal write failure'); END;").unwrap();
    assert!(
        store
            .finish(&turn.id, None, Some("user_interrupted"))
            .is_err()
    );
    assert_eq!(page(&store, &thread, &turn, 0, 100).items, before);
    assert_eq!(events(&store, &thread).len(), event_count);
    assert_eq!(
        super::super::get_turn(&store.connection.lock().unwrap(), &turn.id)
            .unwrap()
            .status,
        TurnStatus::Running
    );
    store
        .connection
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_second")
        .unwrap();
    store
        .finish(&turn.id, None, Some("user_interrupted"))
        .unwrap();
    assert!(
        page(&store, &thread, &turn, 0, 100)
            .items
            .iter()
            .all(|item| item.content.status == ItemStatus::Interrupted)
    );
}

#[test]
fn lifecycle_finalizes_open_items_even_after_progress_quota_is_full() {
    for terminal in [
        TurnStatus::Completed,
        TurnStatus::Failed,
        TurnStatus::Interrupted,
    ] {
        let limits = CoreLimits {
            max_events_per_turn: 16,
            ..CoreLimits::default()
        };
        let (_directory, store, thread, turn) = setup(limits);
        for (id, kind) in [
            ("assistant", ItemKind::AssistantMessage),
            ("ptc", ItemKind::PtcCall),
            ("tool", ItemKind::ToolCall),
        ] {
            emit_item(&store, &thread, &turn, "item.started", &item(id, kind)).unwrap();
        }
        assert!(
            store
                .emit(&thread.id, &turn.id, "tool.progress", json!({}))
                .is_err()
        );
        assert!(
            emit_item(
                &store,
                &thread,
                &turn,
                "item.started",
                &item("overflow", ItemKind::ToolCall)
            )
            .is_err()
        );
        match terminal {
            TurnStatus::Completed => success(&store, &turn),
            TurnStatus::Failed => store.finish(&turn.id, None, None).unwrap(),
            _ => store
                .finish(&turn.id, None, Some("user_interrupted"))
                .unwrap(),
        }
        for item in page(&store, &thread, &turn, 0, 100).items {
            let expected = match terminal {
                TurnStatus::Completed if item.content.kind == ItemKind::AssistantMessage => {
                    ItemStatus::Completed
                }
                TurnStatus::Failed => ItemStatus::Failed,
                _ => ItemStatus::Interrupted,
            };
            assert_eq!(item.content.status, expected);
            assert_eq!(item.content.text.as_deref(), Some("hello"));
        }
        let recorded = events(&store, &thread);
        assert!(recorded.len() <= 17); // per-turn budget excludes thread creation.
        assert!(recorded.last().unwrap().kind.starts_with("turn."));
        assert_eq!(
            recorded
                .iter()
                .filter(|event| event.kind == "item.completed")
                .count(),
            3
        );
    }
}

#[test]
fn restart_finalization_is_durable_and_idempotent() {
    let (directory, store, thread, turn) = setup(CoreLimits::default());
    emit_item(
        &store,
        &thread,
        &turn,
        "item.started",
        &item("pending", ItemKind::PtcCall),
    )
    .unwrap();
    store
        .interrupt(
            &TurnInterruptParams {
                thread_id: thread.id.clone(),
                turn_id: turn.id.clone(),
            },
            false,
            "user_interrupted",
        )
        .unwrap();
    drop(store);
    let store = Store::open(directory.path(), CoreLimits::default()).unwrap();
    let completed = page(&store, &thread, &turn, 0, 100).items;
    assert_eq!(completed[0].content.status, ItemStatus::Interrupted);
    assert_eq!(
        completed[0].content.error.as_deref(),
        Some("core_restarted")
    );
    let count = events(&store, &thread).len();
    drop(store);
    let store = Store::open(directory.path(), CoreLimits::default()).unwrap();
    assert_eq!(page(&store, &thread, &turn, 0, 100).items, completed);
    assert_eq!(events(&store, &thread).len(), count);
}

#[test]
fn authoritative_completion_can_resolve_phase_and_replace_a_truncated_snapshot() {
    let (_directory, store, thread, turn) = setup(CoreLimits::default());
    let mut content = item("message", ItemKind::AssistantMessage);
    content.phase = Some(ItemPhase::Commentary);
    content.truncated = true;
    emit_item(&store, &thread, &turn, "item.started", &content).unwrap();
    content.phase = Some(ItemPhase::FinalAnswer);
    content.status = ItemStatus::Completed;
    content.text = Some("canonical answer".into());
    content.truncated = false;
    emit_item(&store, &thread, &turn, "item.completed", &content).unwrap();
    assert_eq!(
        page(&store, &thread, &turn, 0, 100).items[0].content,
        content
    );
    content.phase = Some(ItemPhase::Commentary);
    assert!(emit_item(&store, &thread, &turn, "item.completed", &content).is_err());
}

#[test]
fn item_pages_are_bounded_by_bytes_and_count_and_never_stall() {
    let (_directory, store, thread, turn) = setup(CoreLimits {
        max_events_per_turn: 5000,
        ..CoreLimits::default()
    });
    for index in 0..10 {
        let mut content = item(&index.to_string(), ItemKind::AssistantMessage);
        content.status = ItemStatus::Completed;
        content.text = Some("x".repeat(MAX_FIELD_BYTES));
        content.input = content.text.clone();
        content.output = content.text.clone();
        content.error = content.text.clone();
        emit_item(&store, &thread, &turn, "item.completed", &content).unwrap();
    }
    let first = page(&store, &thread, &turn, 0, 100);
    assert!(first.has_more && !first.items.is_empty());
    assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_FRAME_BYTES / 2);
    let rest = page(&store, &thread, &turn, first.next_cursor, 100);
    assert_eq!(first.items.len() + rest.items.len(), 10);
    assert!(!rest.has_more);
    for index in 10..MAX_ITEMS {
        let mut content = item(&index.to_string(), ItemKind::AssistantMessage);
        content.status = ItemStatus::Completed;
        emit_item(&store, &thread, &turn, "item.completed", &content).unwrap();
    }
    assert!(
        emit_item(
            &store,
            &thread,
            &turn,
            "item.started",
            &item("overflow", ItemKind::ToolCall)
        )
        .is_err()
    );
    let mut cursor = 0;
    let mut count = 0;
    loop {
        let next = page(&store, &thread, &turn, cursor, 100);
        count += next.items.len();
        assert!(next.items.len() <= 100);
        assert!(next.next_cursor > cursor);
        cursor = next.next_cursor;
        if !next.has_more {
            break;
        }
    }
    assert_eq!(count, MAX_ITEMS);
}

#[test]
fn adding_projection_keeps_existing_state_and_does_not_invent_historical_items() {
    let (directory, store, thread, turn) = setup(CoreLimits::default());
    success(&store, &turn);
    store
        .connection
        .lock()
        .unwrap()
        .execute_batch("DROP TABLE turn_items; PRAGMA user_version=1;")
        .unwrap();
    drop(store);
    let store = Store::open(directory.path(), CoreLimits::default()).unwrap();
    assert_eq!(store.thread(&thread.id).unwrap().id, thread.id);
    assert!(page(&store, &thread, &turn, 0, 100).items.is_empty());
    assert_eq!(
        super::super::get_turn(&store.connection.lock().unwrap(), &turn.id)
            .unwrap()
            .status,
        TurnStatus::Completed
    );
}
