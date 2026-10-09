use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use aporto_protocol::*;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{CoreLimits, ExecutionResult};

mod instances;
mod items;

pub(crate) type PreparedTurn = (Thread, Turn, Vec<Value>, Option<String>);

pub(crate) struct Store {
    connection: Mutex<Connection>,
    // Keep ownership of the process-wide state lock until every worker is gone.
    _lock: File,
    limits: CoreLimits,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn rpc(code: i32, message: &str) -> anyhow::Error {
    RpcError::new(code, message).into()
}

fn private(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, directory);
    }
    Ok(())
}

fn status(value: &str) -> rusqlite::Result<TurnStatus> {
    match value {
        "queued" => Ok(TurnStatus::Queued),
        "running" => Ok(TurnStatus::Running),
        "cancelling" => Ok(TurnStatus::Cancelling),
        "completed" => Ok(TurnStatus::Completed),
        "failed" => Ok(TurnStatus::Failed),
        "interrupted" => Ok(TurnStatus::Interrupted),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn turn_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        input: row.get(2)?,
        status: status(&row.get::<_, String>(3)?)?,
        output: row.get(4)?,
        error: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn thread_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: row.get(0)?,
        title: row.get(1)?,
        agent_id: row.get(2)?,
        bundle_digest: row.get(3)?,
        sandbox_id: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        last_sequence: row.get(7)?,
        release_id: row.get(8)?,
        runtime_instance_id: row.get(9)?,
        runtime_image: row.get(10)?,
        runtime_provider: row.get(11)?,
        workdir: row.get(12)?,
    })
}

const THREAD_FIELDS: &str = "t.id,t.title,t.agent_id,t.bundle_digest,r.sandbox_id,t.created_at,t.updated_at,t.last_sequence,t.release_id,t.runtime_instance_id,r.image,r.provider,t.workdir";
const THREAD_FROM: &str = "threads t JOIN runtime_instances r ON r.id=t.runtime_instance_id";
const TURN_FIELDS: &str = "id,thread_id,input,status,output,error,created_at,updated_at";

fn get_thread(db: &Connection, id: &str) -> Result<Thread> {
    db.query_row(
        &format!("SELECT {THREAD_FIELDS} FROM {THREAD_FROM} WHERE t.id=?1"),
        [id],
        thread_row,
    )
    .optional()?
    .ok_or_else(|| rpc(NOT_FOUND, "thread not found"))
}

fn get_turn(db: &Connection, id: &str) -> Result<Turn> {
    db.query_row(
        &format!("SELECT {TURN_FIELDS} FROM turns WHERE id=?1"),
        [id],
        turn_row,
    )
    .optional()?
    .ok_or_else(|| rpc(NOT_FOUND, "turn not found"))
}

fn event(
    tx: &Transaction<'_>,
    thread_id: &str,
    turn_id: Option<&str>,
    kind: &str,
    data: Value,
) -> Result<Event> {
    let timestamp = now();
    let encoded = serde_json::to_string(&data)?;
    tx.execute(
        "INSERT INTO events(thread_id,turn_id,kind,data_json,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![thread_id, turn_id, kind, encoded, timestamp],
    )?;
    let sequence = tx.last_insert_rowid() as u64;
    tx.execute(
        "UPDATE threads SET last_sequence=?1,updated_at=?2 WHERE id=?3",
        params![sequence, timestamp, thread_id],
    )?;
    Ok(Event {
        sequence,
        thread_id: thread_id.into(),
        turn_id: turn_id.map(str::to_string),
        kind: kind.into(),
        data,
        created_at: timestamp,
    })
}

impl Store {
    pub(crate) fn open(directory: &Path, limits: CoreLimits) -> Result<Self> {
        if let Ok(meta) = fs::symlink_metadata(directory) {
            ensure!(
                !meta.file_type().is_symlink(),
                "state directory cannot be a symlink"
            );
        }
        fs::create_dir_all(directory)?;
        private(directory, true)?;
        for name in [
            "state.lock",
            "state.sqlite",
            "state.sqlite-wal",
            "state.sqlite-shm",
        ] {
            if let Ok(meta) = fs::symlink_metadata(directory.join(name)) {
                ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "state entries must be regular files"
                );
            }
        }
        let lock_path = directory.join("state.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        private(&lock_path, false)?;
        lock.try_lock_exclusive()
            .context("state directory is already owned by another Core process")?;
        let database_path = directory.join("state.sqlite");
        let mut connection = Connection::open(&database_path)?;
        private(&database_path, false)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(version <= 3, "unsupported Core state schema");
        if version == 0 {
            let tx = connection.transaction()?;
            tx.execute_batch("CREATE TABLE threads (
                id TEXT PRIMARY KEY, title TEXT NOT NULL, agent_id TEXT NOT NULL, bundle_digest TEXT NOT NULL, release_id TEXT NOT NULL,
                sandbox_id TEXT, history_json TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, last_sequence INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE turns (
                id TEXT PRIMARY KEY, thread_id TEXT NOT NULL REFERENCES threads(id), input TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('queued','running','cancelling','completed','failed','interrupted')),
                output TEXT, error TEXT, idempotency_key TEXT NOT NULL, payload_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                UNIQUE(thread_id,idempotency_key)
            );
            CREATE UNIQUE INDEX one_active_turn_per_thread ON turns(thread_id)
                WHERE status IN ('queued','running','cancelling');
            CREATE INDEX turns_thread_time ON turns(thread_id,created_at,id);
            CREATE TABLE events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL REFERENCES threads(id),
                turn_id TEXT REFERENCES turns(id), kind TEXT NOT NULL, data_json TEXT NOT NULL, created_at INTEGER NOT NULL
            );
            CREATE INDEX events_thread_sequence ON events(thread_id,sequence);
            CREATE INDEX events_turn ON events(turn_id);
            PRAGMA user_version=1;")?;
            tx.commit()?;
        }
        // Reject stores lacking immutable release bindings before adding projections,
        // recovering turns or accepting any external side effects.
        connection
            .prepare("SELECT release_id FROM threads LIMIT 0")
            .context("incompatible Core state; select an empty state directory")?;
        items::initialize(&mut connection, version)?;
        instances::initialize(&mut connection)?;
        for name in ["state.sqlite-wal", "state.sqlite-shm"] {
            let path = directory.join(name);
            if path.exists() {
                private(&path, false)?;
            }
        }
        let store = Self {
            connection: Mutex::new(connection),
            _lock: lock,
            limits,
        };
        store.recover()?;
        Ok(store)
    }

    fn recover(&self) -> Result<()> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let active = {
            let mut stmt = tx.prepare(
                "SELECT id,thread_id FROM turns WHERE status IN ('queued','running','cancelling')",
            )?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (turn_id, thread_id) in active {
            items::finalize(
                &tx,
                &thread_id,
                &turn_id,
                TurnStatus::Interrupted,
                Some("core_restarted"),
            )?;
            tx.execute("UPDATE turns SET status='interrupted',error='core_restarted',updated_at=?1 WHERE id=?2", params![now(), turn_id])?;
            event(
                &tx,
                &thread_id,
                Some(&turn_id),
                "turn.interrupted",
                json!({"reason":"core_restarted"}),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn list_threads(&self, request: ThreadListParams) -> Result<ThreadListResult> {
        let limit = request.limit.unwrap_or(50).clamp(1, 100) as usize;
        let db = self.connection.lock().unwrap();
        let cursor = match request.cursor {
            Some(id) => {
                let thread = get_thread(&db, &id)?;
                (thread.created_at, thread.id)
            }
            None => (i64::MAX, "~".to_string()),
        };
        let mut stmt = db.prepare(&format!("SELECT {THREAD_FIELDS} FROM {THREAD_FROM} WHERE t.created_at<?1 OR (t.created_at=?1 AND t.id<?2) ORDER BY t.created_at DESC,t.id DESC LIMIT ?3"))?;
        let mut threads = stmt
            .query_map(params![cursor.0, cursor.1, limit + 1], thread_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = threads.len() > limit;
        threads.truncate(limit);
        let next_cursor = if more {
            threads.last().map(|thread| thread.id.clone())
        } else {
            None
        };
        Ok(ThreadListResult {
            threads,
            next_cursor,
        })
    }

    pub(crate) fn read_thread(&self, request: ThreadReadParams) -> Result<ThreadReadResult> {
        let limit = request.limit.unwrap_or(20).clamp(1, 20) as usize;
        let db = self.connection.lock().unwrap();
        let thread = get_thread(&db, &request.thread_id)?;
        let before = match request.before {
            Some(id) => db
                .query_row(
                    "SELECT rowid FROM turns WHERE id=?1 AND thread_id=?2",
                    params![id, request.thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .ok_or_else(|| rpc(NOT_FOUND, "turn page cursor not found in thread"))?,
            None => i64::MAX,
        };
        let mut stmt = db.prepare(&format!("SELECT {TURN_FIELDS} FROM turns WHERE thread_id=?1 AND rowid<?2 ORDER BY rowid DESC LIMIT ?3"))?;
        let raw = stmt
            .query_map(params![request.thread_id, before, limit + 1], turn_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut turns = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for turn in raw {
            let size = serde_json::to_vec(&turn)?.len();
            if turns.len() == limit || bytes + size > MAX_FRAME_BYTES / 2 {
                more = true;
                break;
            }
            bytes += size;
            turns.push(turn);
        }
        let next_cursor = if more {
            turns.last().map(|turn| turn.id.clone())
        } else {
            None
        };
        turns.reverse();
        Ok(ThreadReadResult {
            thread,
            turns,
            next_cursor,
        })
    }

    pub(crate) fn thread(&self, id: &str) -> Result<Thread> {
        get_thread(&self.connection.lock().unwrap(), id)
    }

    pub(crate) fn existing_turn(
        &self,
        thread_id: &str,
        key: &str,
        hash: &str,
    ) -> Result<Option<Turn>> {
        let db = self.connection.lock().unwrap();
        let record = db
            .query_row(
                "SELECT id,payload_hash FROM turns WHERE thread_id=?1 AND idempotency_key=?2",
                params![thread_id, key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        match record {
            Some((id, existing_hash)) => {
                if existing_hash != hash {
                    return Err(rpc(
                        CONFLICT,
                        "idempotency key was already used with a different payload",
                    ));
                }
                Ok(Some(get_turn(&db, &id)?))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn start_turn(&self, request: &TurnStartParams, hash: &str) -> Result<Turn> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let thread = get_thread(&tx, &request.thread_id)?;
        let active: usize = tx.query_row("SELECT count(*) FROM turns WHERE thread_id=?1 AND status IN ('queued','running','cancelling')",
            [&request.thread_id], |row| row.get(0))?;
        if active != 0 {
            return Err(rpc(CONFLICT, "thread already has an active turn"));
        }
        let id = Uuid::new_v4().to_string();
        let timestamp = now();
        tx.execute("INSERT INTO turns(id,thread_id,input,status,idempotency_key,payload_hash,created_at,updated_at,runtime_instance_id) VALUES(?1,?2,?3,'queued',?4,?5,?6,?6,?7)",
            params![id, request.thread_id, request.input, request.idempotency_key, hash, timestamp,thread.runtime_instance_id])?;
        event(
            &tx,
            &request.thread_id,
            Some(&id),
            "turn.queued",
            json!({"input":request.input}),
        )?;
        let turn = get_turn(&tx, &id)?;
        tx.commit()?;
        Ok(turn)
    }

    pub(crate) fn begin(&self, turn_id: &str) -> Result<Option<PreparedTurn>> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let turn = get_turn(&tx, turn_id)?;
        if turn.status != TurnStatus::Queued {
            return Ok(None);
        }
        let thread = get_thread(&tx, &turn.thread_id)?;
        let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM turns u JOIN threads t ON t.id=u.thread_id WHERE t.runtime_instance_id=?1 AND u.status IN ('running','cancelling'))", [&thread.runtime_instance_id], |row| row.get(0))?;
        ensure!(!busy, "runtime instance already has a running turn");
        let history: String = tx.query_row(
            "SELECT history_json FROM threads WHERE id=?1",
            [&turn.thread_id],
            |row| row.get(0),
        )?;
        let previous: Option<String> = tx.query_row("SELECT status FROM turns WHERE thread_id=?1 AND id<>?2 ORDER BY rowid DESC LIMIT 1",
            params![turn.thread_id, turn_id], |row| row.get(0)).optional()?;
        let recovery_note = previous.filter(|state| state == "failed" || state == "interrupted").map(|_| {
            "The previous turn failed or was interrupted. Some tools may already have changed the workspace or external systems. Inspect current state before retrying actions; do not assume that the last successful conversation history describes all side effects.".to_string()
        });
        tx.execute(
            "UPDATE turns SET status='running',updated_at=?1 WHERE id=?2",
            params![now(), turn_id],
        )?;
        event(
            &tx,
            &turn.thread_id,
            Some(turn_id),
            "turn.started",
            json!({}),
        )?;
        let ready = (
            get_thread(&tx, &turn.thread_id)?,
            get_turn(&tx, turn_id)?,
            serde_json::from_str(&history)?,
            recovery_note,
        );
        tx.commit()?;
        Ok(Some(ready))
    }

    pub(crate) fn interrupt(
        &self,
        request: &TurnInterruptParams,
        queued: bool,
        reason: &str,
    ) -> Result<Turn> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let turn = get_turn(&tx, &request.turn_id)?;
        if turn.thread_id != request.thread_id {
            return Err(rpc(NOT_FOUND, "turn not found in thread"));
        }
        if turn.status.is_terminal() || turn.status == TurnStatus::Cancelling {
            return Ok(turn);
        }
        let (state, kind) = if queued {
            ("interrupted", "turn.interrupted")
        } else {
            ("cancelling", "turn.cancelling")
        };
        if queued {
            items::finalize(
                &tx,
                &turn.thread_id,
                &turn.id,
                TurnStatus::Interrupted,
                Some(reason),
            )?;
        }
        tx.execute(
            "UPDATE turns SET status=?1,error=?2,updated_at=?3 WHERE id=?4",
            params![state, reason, now(), turn.id],
        )?;
        event(
            &tx,
            &turn.thread_id,
            Some(&turn.id),
            kind,
            json!({"reason":reason}),
        )?;
        let updated = get_turn(&tx, &turn.id)?;
        tx.commit()?;
        Ok(updated)
    }

    pub(crate) fn finish(
        &self,
        turn_id: &str,
        result: Option<&ExecutionResult>,
        interrupted: Option<&str>,
    ) -> Result<()> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let turn = get_turn(&tx, turn_id)?;
        if turn.status.is_terminal() {
            return Ok(());
        }
        if let Some(reason) = interrupted {
            items::finalize(
                &tx,
                &turn.thread_id,
                turn_id,
                TurnStatus::Interrupted,
                Some(reason),
            )?;
            tx.execute(
                "UPDATE turns SET status='interrupted',error=?1,updated_at=?2 WHERE id=?3",
                params![reason, now(), turn_id],
            )?;
            event(
                &tx,
                &turn.thread_id,
                Some(turn_id),
                "turn.interrupted",
                json!({"reason":reason}),
            )?;
        } else if let Some(result) = result {
            let history = serde_json::to_string(&result.history)?;
            ensure!(
                history.len() <= self.limits.max_history_bytes
                    && result.answer.len() <= self.limits.max_output_bytes,
                "executor output exceeds Core persistence limits"
            );
            items::finalize(&tx, &turn.thread_id, turn_id, TurnStatus::Completed, None)?;
            tx.execute(
                "UPDATE threads SET history_json=?1 WHERE id=?2",
                params![history, turn.thread_id],
            )?;
            tx.execute("UPDATE turns SET status='completed',output=?1,error=NULL,updated_at=?2 WHERE id=?3", params![result.answer, now(), turn_id])?;
            event(
                &tx,
                &turn.thread_id,
                Some(turn_id),
                "turn.completed",
                json!({"output":result.answer}),
            )?;
        } else {
            items::finalize(
                &tx,
                &turn.thread_id,
                turn_id,
                TurnStatus::Failed,
                Some("execution_failed"),
            )?;
            tx.execute("UPDATE turns SET status='failed',error='execution_failed',updated_at=?1 WHERE id=?2", params![now(), turn_id])?;
            event(
                &tx,
                &turn.thread_id,
                Some(turn_id),
                "turn.failed",
                json!({"reason":"execution_failed"}),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn emit(
        &self,
        thread_id: &str,
        turn_id: &str,
        kind: &str,
        data: Value,
    ) -> Result<()> {
        ensure!(
            !kind.is_empty()
                && kind.len() <= 128
                && kind
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid event kind"
        );
        ensure!(
            !matches!(
                kind,
                "thread.started"
                    | "turn.queued"
                    | "turn.started"
                    | "turn.running"
                    | "turn.cancelling"
                    | "turn.completed"
                    | "turn.failed"
                    | "turn.interrupted"
            ),
            "executor cannot emit lifecycle events"
        );
        ensure!(
            serde_json::to_vec(&data)?.len() <= self.limits.max_event_bytes,
            "event data exceeds byte limit"
        );
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let turn = get_turn(&tx, turn_id)?;
        if kind.starts_with("item.") {
            items::emit(&tx, thread_id, &turn, kind, data, &self.limits)?;
            tx.commit()?;
            return Ok(());
        }
        ensure!(
            turn.thread_id == thread_id
                && matches!(turn.status, TurnStatus::Running | TurnStatus::Cancelling),
            "turn is no longer active"
        );
        items::capacity(&tx, turn_id, &self.limits, items::pending(&tx, turn_id)?)?;
        if kind == "runtime.ready" {
            let id = data
                .get("sandbox_id")
                .and_then(Value::as_str)
                .context("runtime.ready requires sandbox_id")?;
            ensure!(
                !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
                "invalid sandbox_id"
            );
            instances::ready(&tx, thread_id, id)?;
        }
        event(&tx, thread_id, Some(turn_id), kind, data)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn items(&self, request: ItemListParams) -> Result<ItemListResult> {
        items::list(&self.connection.lock().unwrap(), request)
    }

    pub(crate) fn events(&self, request: EventListParams) -> Result<EventListResult> {
        let limit = request.limit.unwrap_or(100).clamp(1, 500) as usize;
        let after = request.after.unwrap_or(0);
        if after > i64::MAX as u64 {
            return Err(rpc(INVALID_PARAMS, "event cursor exceeds supported range"));
        }
        let db = self.connection.lock().unwrap();
        get_thread(&db, &request.thread_id)?;
        let mut stmt = db.prepare("SELECT sequence,thread_id,turn_id,kind,data_json,created_at FROM events WHERE thread_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
        let mut rows = stmt.query(params![request.thread_id, after, limit + 1])?;
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            let next = Event {
                sequence: row.get(0)?,
                thread_id: row.get(1)?,
                turn_id: row.get(2)?,
                kind: row.get(3)?,
                data: serde_json::from_str(&row.get::<_, String>(4)?)?,
                created_at: row.get(5)?,
            };
            let size = serde_json::to_vec(&next)?.len();
            if events.len() == limit || bytes + size > MAX_FRAME_BYTES / 2 {
                has_more = true;
                break;
            }
            bytes += size;
            events.push(next);
        }
        let next_cursor = events.last().map_or(after, |event| event.sequence);
        Ok(EventListResult {
            events,
            next_cursor,
            has_more,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_interrupts_all_active_states_and_preserves_last_successful_history() {
        let directory = tempfile::tempdir().unwrap();
        let limits = CoreLimits::default();
        let store = Store::open(directory.path(), limits.clone()).unwrap();
        let agent = AgentSummary {
            id: "test".into(),
            name: "Test".into(),
            model: "fixture".into(),
            bundle_digest: "sha256:fixture".into(),
            release_id: "sha256:release".into(),
            runtime: RuntimeOptions {
                provider: "docker".into(),
                images: vec!["fixture:latest".into()],
                default_image: "fixture:latest".into(),
                default_workdir: "/workspace".into(),
            },
        };
        let mut unfinished = Vec::new();
        for state in ["queued", "running", "cancelling"] {
            let thread = store.start_thread(&agent, state).unwrap();
            let success = store
                .start_turn(
                    &TurnStartParams {
                        thread_id: thread.id.clone(),
                        input: "first".into(),
                        idempotency_key: "first".into(),
                    },
                    "first-hash",
                )
                .unwrap();
            store.begin(&success.id).unwrap();
            store
                .finish(
                    &success.id,
                    Some(&ExecutionResult {
                        answer: "finished".into(),
                        history: vec![json!({"role":"assistant","content":"committed"})],
                    }),
                    None,
                )
                .unwrap();
            let turn = store
                .start_turn(
                    &TurnStartParams {
                        thread_id: thread.id.clone(),
                        input: state.into(),
                        idempotency_key: state.into(),
                    },
                    state,
                )
                .unwrap();
            if state != "queued" {
                store.begin(&turn.id).unwrap();
            }
            if state == "cancelling" {
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
            }
            unfinished.push((thread.id, turn.id));
        }
        drop(store); // Simulate process loss: no graceful terminal writes.
        let store = Store::open(directory.path(), limits).unwrap();
        for (thread_id, turn_id) in unfinished {
            let history: String = store
                .connection
                .lock()
                .unwrap()
                .query_row(
                    "SELECT history_json FROM threads WHERE id=?1",
                    [&thread_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(history.contains("committed"));
            let turn = get_turn(&store.connection.lock().unwrap(), &turn_id).unwrap();
            assert_eq!(turn.status, TurnStatus::Interrupted);
            assert_eq!(turn.error.as_deref(), Some("core_restarted"));
            let events = store
                .events(EventListParams {
                    thread_id: thread_id.clone(),
                    after: None,
                    limit: None,
                })
                .unwrap();
            assert_eq!(events.events.last().unwrap().kind, "turn.interrupted");
            assert_eq!(
                events.events.last().unwrap().data["reason"],
                "core_restarted"
            );
            let next = store
                .start_turn(
                    &TurnStartParams {
                        thread_id,
                        input: "resume".into(),
                        idempotency_key: "new".into(),
                    },
                    "new",
                )
                .unwrap();
            let (_, _, history, recovery_note) = store.begin(&next.id).unwrap().unwrap();
            assert_eq!(history.len(), 1);
            assert!(recovery_note.is_some());
        }
    }
}
