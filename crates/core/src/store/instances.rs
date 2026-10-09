//! Durable ownership and selection of shared runtime instances.
use super::{Store, event, get_thread, now, rpc};
use crate::{TurnExecutor, normalize_runtime_options};
use anyhow::{Context, Result, ensure};
use aporto_protocol::*;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;
use uuid::Uuid;

#[derive(Clone)]
struct Instance {
    id: String,
    agent_id: String,
    release_id: String,
    bundle_digest: String,
    provider: String,
    image: String,
    sandbox_id: Option<String>,
    workdir: String,
    created_at: i64,
    updated_at: i64,
}
const FIELDS: &str =
    "id,agent_id,release_id,bundle_digest,provider,image,sandbox_id,workdir,created_at,updated_at";
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Instance> {
    Ok(Instance {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        release_id: row.get(2)?,
        bundle_digest: row.get(3)?,
        provider: row.get(4)?,
        image: row.get(5)?,
        sandbox_id: row.get(6)?,
        workdir: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}
fn find(db: &Connection, id: &str) -> Result<Instance> {
    db.query_row(
        &format!("SELECT {FIELDS} FROM runtime_instances WHERE id=?1"),
        [id],
        row,
    )
    .optional()?
    .ok_or_else(|| rpc(NOT_FOUND, "runtime instance not found"))
}
fn insert(tx: &Transaction<'_>, instance: &Instance) -> Result<()> {
    tx.execute("INSERT INTO runtime_instances(id,agent_id,release_id,bundle_digest,provider,image,sandbox_id,workdir,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![instance.id,instance.agent_id,instance.release_id,instance.bundle_digest,instance.provider,instance.image,instance.sandbox_id,instance.workdir,instance.created_at,instance.updated_at])?;
    Ok(())
}

pub(super) fn initialize(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_instances (
        id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, release_id TEXT NOT NULL,
        bundle_digest TEXT NOT NULL, provider TEXT NOT NULL, image TEXT NOT NULL,
        sandbox_id TEXT, workdir TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
        UNIQUE(provider,release_id,sandbox_id)
    );
    CREATE INDEX IF NOT EXISTS instances_agent_time ON runtime_instances(agent_id,created_at,id);")?;
    for (table, column, declaration) in [
        (
            "threads",
            "runtime_instance_id",
            "TEXT REFERENCES runtime_instances(id)",
        ),
        ("threads", "workdir", "TEXT"),
        (
            "turns",
            "runtime_instance_id",
            "TEXT REFERENCES runtime_instances(id)",
        ),
    ] {
        let exists: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name=?1)"),
            [column],
            |row| row.get(0),
        )?;
        if !exists {
            tx.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {declaration};"
            ))?;
        }
    }
    tx.execute_batch("CREATE INDEX IF NOT EXISTS threads_instance ON threads(runtime_instance_id);
        CREATE UNIQUE INDEX IF NOT EXISTS one_running_turn_per_instance ON turns(runtime_instance_id)
          WHERE status IN ('running','cancelling');
        PRAGMA user_version=3;")?;
    tx.commit()?;
    Ok(())
}

pub(super) fn ready(tx: &Transaction<'_>, thread_id: &str, sandbox_id: &str) -> Result<()> {
    let thread = get_thread(tx, thread_id)?;
    let instance = find(tx, &thread.runtime_instance_id)?;
    ensure!(
        instance
            .sandbox_id
            .as_deref()
            .is_none_or(|old| old == sandbox_id),
        "runtime instance cannot be rebound to a different sandbox"
    );
    tx.execute(
        "UPDATE runtime_instances SET sandbox_id=?1,updated_at=?2 WHERE id=?3",
        params![sandbox_id, now(), instance.id],
    )?;
    Ok(())
}

impl Store {
    /// Existing threads are bound before workers start. Never guess an image or
    /// provider when the immutable release required to recover it is unavailable.
    pub(crate) fn bind_legacy_instances(&self, executor: &dyn TurnExecutor) -> Result<()> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let legacy = {
            let mut stmt = tx.prepare("SELECT id,agent_id,release_id,bundle_digest,sandbox_id,created_at,updated_at FROM threads WHERE runtime_instance_id IS NULL ORDER BY created_at,id")?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (thread_id, agent_id, release_id, bundle_digest, sandbox_id, created_at, updated_at) in
            legacy
        {
            let options = normalize_runtime_options(executor.runtime_options(&agent_id,&release_id,&bundle_digest)
                .context("legacy thread runtime release is unavailable; restore its immutable release before opening this state directory")?)?;
            let existing = if let Some(sandbox) = &sandbox_id {
                tx.query_row(&format!("SELECT {FIELDS} FROM runtime_instances WHERE provider=?1 AND release_id=?2 AND sandbox_id=?3"),
                    params![options.provider,release_id,sandbox], row).optional()?
            } else {
                None
            };
            let instance = if let Some(existing) = existing {
                ensure!(
                    existing.agent_id == agent_id && existing.bundle_digest == bundle_digest,
                    "legacy physical instance has conflicting release ownership"
                );
                existing
            } else {
                let instance = Instance {
                    id: Uuid::new_v4().to_string(),
                    agent_id,
                    release_id,
                    bundle_digest,
                    provider: options.provider,
                    image: options.default_image,
                    sandbox_id,
                    workdir: options.default_workdir,
                    created_at,
                    updated_at,
                };
                insert(&tx, &instance)?;
                instance
            };
            tx.execute(
                "UPDATE threads SET runtime_instance_id=?1,workdir=?2 WHERE id=?3",
                params![instance.id, instance.workdir, thread_id],
            )?;
        }
        tx.execute("UPDATE turns SET runtime_instance_id=(SELECT runtime_instance_id FROM threads WHERE threads.id=turns.thread_id) WHERE runtime_instance_id IS NULL", [])?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_thread(&self, agent: &AgentSummary, title: &str) -> Result<Thread> {
        self.start_thread_selected(agent, title, None, None, |_, _, _| {
            Some(agent.runtime.clone())
        })
    }

    pub(crate) fn start_thread_selected(
        &self,
        agent: &AgentSummary,
        title: &str,
        selection: Option<ThreadRuntimeParams>,
        workdir: Option<String>,
        available: impl Fn(&str, &str, &str) -> Option<RuntimeOptions>,
    ) -> Result<Thread> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let count: usize = tx.query_row("SELECT count(*) FROM threads", [], |row| row.get(0))?;
        if count >= self.limits.max_threads {
            return Err(rpc(OVERLOADED, "thread limit reached"));
        }
        let timestamp = now();
        let (mut instance, new) =
            match selection.unwrap_or(ThreadRuntimeParams::New { image: None }) {
                ThreadRuntimeParams::New { image } => {
                    let image = image.unwrap_or_else(|| agent.runtime.default_image.clone());
                    if !agent.runtime.images.contains(&image) {
                        return Err(rpc(
                            INVALID_PARAMS,
                            "image is not configured for this agent release",
                        ));
                    }
                    (
                        Instance {
                            id: Uuid::new_v4().to_string(),
                            agent_id: agent.id.clone(),
                            release_id: agent.release_id.clone(),
                            bundle_digest: agent.bundle_digest.clone(),
                            provider: agent.runtime.provider.clone(),
                            image,
                            sandbox_id: None,
                            workdir: workdir
                                .clone()
                                .unwrap_or_else(|| agent.runtime.default_workdir.clone()),
                            created_at: timestamp,
                            updated_at: timestamp,
                        },
                        true,
                    )
                }
                ThreadRuntimeParams::Reuse { instance_id } => {
                    let instance = find(&tx, &instance_id)?;
                    if instance.agent_id != agent.id || instance.sandbox_id.is_none() {
                        return Err(rpc(
                            NOT_FOUND,
                            "materialized runtime instance not found for this agent",
                        ));
                    }
                    let options = available(
                    &instance.agent_id,
                    &instance.release_id,
                    &instance.bundle_digest,
                )
                .and_then(|value| normalize_runtime_options(value).ok())
                .ok_or_else(|| {
                    rpc(
                        CONFLICT,
                        "runtime instance release is unavailable; restore its immutable release",
                    )
                })?;
                    if options.provider != instance.provider
                        || !options.images.contains(&instance.image)
                    {
                        return Err(rpc(
                            CONFLICT,
                            "runtime instance no longer matches its immutable release",
                        ));
                    }
                    (instance, false)
                }
            };
        let workdir = workdir.unwrap_or_else(|| instance.workdir.clone());
        let workdir = aporto::workspace::validate_workdir(&workdir).map_err(|_| {
            rpc(
                INVALID_PARAMS,
                "workdir must be a supported absolute runtime path",
            )
        })?;
        if new {
            instance.workdir = workdir.clone();
            insert(&tx, &instance)?;
        }
        let id = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO threads(id,title,agent_id,bundle_digest,release_id,runtime_instance_id,workdir,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",
            params![id,title,agent.id,instance.bundle_digest,instance.release_id,instance.id,workdir,timestamp])?;
        event(
            &tx,
            &id,
            None,
            "thread.started",
            json!({"agent_id":agent.id,"bundle_digest":instance.bundle_digest,
            "release_id":instance.release_id,"runtime_instance_id":instance.id,"runtime_image":instance.image,
            "runtime_provider":instance.provider,"workdir":workdir}),
        )?;
        let thread = get_thread(&tx, &id)?;
        tx.commit()?;
        Ok(thread)
    }

    pub(crate) fn instances(
        &self,
        request: RuntimeInstanceListParams,
        available: impl Fn(&str, &str, &str) -> Option<RuntimeOptions>,
    ) -> Result<RuntimeInstanceListResult> {
        let limit = request.limit.unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return Err(rpc(INVALID_PARAMS, "instance page limit must be 1..100"));
        }
        let db = self.connection.lock().unwrap();
        let cursor = if let Some(id) = request.cursor {
            let instance = find(&db, &id)?;
            if instance.agent_id != request.agent_id || instance.sandbox_id.is_none() {
                return Err(rpc(NOT_FOUND, "instance cursor not found for this agent"));
            }
            (instance.created_at, instance.id)
        } else {
            (i64::MAX, "~".to_owned())
        };
        let mut statement = db.prepare(&format!("SELECT {FIELDS} FROM runtime_instances WHERE agent_id=?1 AND sandbox_id IS NOT NULL AND (created_at<?2 OR (created_at=?2 AND id<?3)) ORDER BY created_at DESC,id DESC"))?;
        let mut rows = statement.query(params![request.agent_id, cursor.0, cursor.1])?;
        let mut instances = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        while let Some(row) = rows.next()? {
            let instance = self::row(row)?;
            let supported = available(
                &instance.agent_id,
                &instance.release_id,
                &instance.bundle_digest,
            )
            .and_then(|options| normalize_runtime_options(options).ok())
            .is_some_and(|options| {
                options.provider == instance.provider && options.images.contains(&instance.image)
            });
            if !supported {
                continue;
            }
            let busy = db.query_row("SELECT EXISTS(SELECT 1 FROM turns u JOIN threads t ON t.id=u.thread_id WHERE t.runtime_instance_id=?1 AND u.status IN ('queued','running','cancelling'))", [&instance.id], |row| row.get(0))?;
            let public = RuntimeInstance {
                id: instance.id,
                agent_id: instance.agent_id,
                release_id: instance.release_id,
                bundle_digest: instance.bundle_digest,
                provider: instance.provider,
                image: instance.image,
                sandbox_id: instance.sandbox_id.expect("query excludes null"),
                workdir: instance.workdir,
                created_at: instance.created_at,
                updated_at: instance.updated_at,
                busy,
            };
            let size = serde_json::to_vec(&public)?.len();
            if instances.len() == limit as usize || bytes + size > MAX_FRAME_BYTES / 2 {
                more = true;
                break;
            }
            bytes += size;
            instances.push(public);
        }
        let next_cursor = more.then(|| {
            instances
                .last()
                .expect("one bounded instance fits a page")
                .id
                .clone()
        });
        Ok(RuntimeInstanceListResult {
            instances,
            next_cursor,
        })
    }
}

#[cfg(test)]
mod tests;
