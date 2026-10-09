//! Transactional transcript projection. Event snapshots and item reads share one commit.
use super::{event, get_turn, rpc};
use crate::CoreLimits;
use anyhow::{Context, Result, ensure};
use aporto_protocol::*;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_ITEMS: usize = 1000;
const MAX_FIELD_BYTES: usize = 32 * 1024;
const MAX_ITEM_BYTES: usize = 200 * 1024;
// Reserved for server metadata, terminal status and a short lifecycle reason.
const ENRICHMENT_RESERVE: usize = 1024;

pub(super) fn initialize(db: &mut Connection, version: u32) -> Result<()> {
    if version < 2 {
        let tx = db.transaction()?;
        tx.execute_batch(
            "CREATE TABLE turn_items (
                turn_id TEXT NOT NULL REFERENCES turns(id),
                id TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                sequence INTEGER NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('in_progress','completed','failed','interrupted')),
                content_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(turn_id,id),
                UNIQUE(turn_id,ordinal)
            );
            CREATE INDEX turn_items_active ON turn_items(turn_id,status);
            PRAGMA user_version=2;",
        )?;
        tx.commit()?;
    }
    db.prepare(&format!(
        "SELECT {FIELDS},id,status FROM turn_items LIMIT 0"
    ))
    .context("incompatible transcript projection")?;
    Ok(())
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnItem> {
    let encoded: String = row.get(0)?;
    let content = serde_json::from_str(&encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(TurnItem {
        content,
        turn_id: row.get(1)?,
        ordinal: row.get(2)?,
        sequence: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}
const FIELDS: &str = "content_json,turn_id,ordinal,sequence,created_at,updated_at";

fn find(db: &Connection, turn: &str, id: &str) -> Result<Option<TurnItem>> {
    Ok(db
        .query_row(
            &format!("SELECT {FIELDS} FROM turn_items WHERE turn_id=?1 AND id=?2"),
            params![turn, id],
            row,
        )
        .optional()?)
}

pub(super) fn pending(db: &Connection, turn_id: &str) -> Result<usize> {
    Ok(db.query_row(
        "SELECT count(*) FROM turn_items WHERE turn_id=?1 AND status='in_progress'",
        [turn_id],
        |row| row.get(0),
    )?)
}

/// One event slot per open item, plus the pre-existing lifecycle reserve. A final
/// snapshot consumes its reserved slot even when ordinary progress is exhausted.
pub(super) fn capacity(
    db: &Connection,
    turn_id: &str,
    limits: &CoreLimits,
    pending_after: usize,
) -> Result<()> {
    let count: usize = db.query_row(
        "SELECT count(*) FROM events WHERE turn_id=?1",
        [turn_id],
        |row| row.get(0),
    )?;
    ensure!(
        count.saturating_add(9).saturating_add(pending_after) <= limits.max_events_per_turn,
        "turn event limit reached"
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemEvent {
    item: ItemContent,
}

fn validate(content: &ItemContent) -> Result<()> {
    ensure!(
        !content.id.is_empty()
            && content.id.len() <= 128
            && !content.id.chars().any(char::is_control),
        "invalid item ID"
    );
    ensure!(
        content.name.as_ref().is_none_or(|name| !name.is_empty()
            && name.len() <= 256
            && !name.chars().any(char::is_control)),
        "invalid item name"
    );
    ensure!(
        content.phase.is_none() || content.kind == ItemKind::AssistantMessage,
        "only assistant messages have a phase"
    );
    for field in [
        &content.text,
        &content.input,
        &content.output,
        &content.error,
    ] {
        ensure!(
            field
                .as_ref()
                .is_none_or(|text| text.len() <= MAX_FIELD_BYTES),
            "item field exceeds 32 KiB"
        );
    }
    ensure!(
        serde_json::to_vec(content)?.len() <= MAX_ITEM_BYTES,
        "encoded item exceeds byte limit"
    );
    Ok(())
}

pub(super) fn emit(
    tx: &Transaction<'_>,
    thread_id: &str,
    turn: &Turn,
    kind: &str,
    data: Value,
    limits: &CoreLimits,
) -> Result<()> {
    let content = serde_json::from_value::<ItemEvent>(data)
        .map_err(|_| anyhow::anyhow!("invalid item event"))?
        .item;
    validate(&content)?;
    ensure!(
        match kind {
            "item.started" | "item.updated" => content.status == ItemStatus::InProgress,
            "item.completed" => content.status.is_terminal(),
            _ => false,
        },
        "item event kind differs from status"
    );
    ensure!(turn.thread_id == thread_id, "item turn differs from thread");
    let previous = find(tx, &turn.id, &content.id)?;
    if let Some(previous) = &previous {
        ensure!(
            previous.content.kind == content.kind && previous.content.name == content.name,
            "item identity changed"
        );
        if previous.content == content {
            return Ok(());
        }
        ensure!(
            !previous.content.status.is_terminal(),
            "terminal item cannot be changed"
        );
        ensure!(kind != "item.started", "item already started");
    } else {
        ensure!(kind != "item.updated", "cannot update an unknown item");
        let count: usize = tx.query_row(
            "SELECT count(*) FROM turn_items WHERE turn_id=?1",
            [&turn.id],
            |row| row.get(0),
        )?;
        ensure!(count < MAX_ITEMS, "turn item limit reached");
    }
    ensure!(
        matches!(turn.status, TurnStatus::Running | TurnStatus::Cancelling),
        "turn is no longer active"
    );
    ensure!(
        serde_json::to_vec(&content)?.len() <= MAX_ITEM_BYTES - ENRICHMENT_RESERVE,
        "encoded item exceeds byte limit"
    );
    let pending_after = pending(tx, &turn.id)? + usize::from(!content.status.is_terminal())
        - usize::from(
            previous
                .as_ref()
                .is_some_and(|item| !item.content.status.is_terminal()),
        );
    capacity(tx, &turn.id, limits, pending_after)?;
    // Reserve metadata space under the configured observer event limit as well.
    ensure!(
        serde_json::to_vec(&json!({"item":content}))?.len() + ENRICHMENT_RESERVE
            <= limits.max_event_bytes,
        "item event exceeds byte limit"
    );
    persist(tx, thread_id, &turn.id, kind, content, previous.as_ref())?;
    Ok(())
}

fn persist(
    tx: &Transaction<'_>,
    thread_id: &str,
    turn_id: &str,
    kind: &str,
    content: ItemContent,
    previous: Option<&TurnItem>,
) -> Result<()> {
    let emitted = event(tx, thread_id, Some(turn_id), kind, Value::Null)?;
    let item = TurnItem {
        content,
        turn_id: turn_id.into(),
        ordinal: previous.map_or(emitted.sequence, |item| item.ordinal),
        sequence: emitted.sequence,
        created_at: previous.map_or(emitted.created_at, |item| item.created_at),
        updated_at: emitted.created_at,
    };
    ensure!(
        serde_json::to_vec(&item)?.len() <= MAX_ITEM_BYTES,
        "encoded item exceeds byte limit"
    );
    let data = serde_json::to_string(&json!({"item":item}))?;
    tx.execute(
        "UPDATE events SET data_json=?1 WHERE sequence=?2",
        params![data, item.sequence],
    )?;
    let status = match item.content.status {
        ItemStatus::InProgress => "in_progress",
        ItemStatus::Completed => "completed",
        ItemStatus::Failed => "failed",
        ItemStatus::Interrupted => "interrupted",
    };
    tx.execute(
        "INSERT INTO turn_items(turn_id,id,ordinal,sequence,status,content_json,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(turn_id,id) DO UPDATE SET sequence=excluded.sequence,status=excluded.status,
            content_json=excluded.content_json,updated_at=excluded.updated_at",
        params![item.turn_id,item.content.id,item.ordinal,item.sequence,status,
            serde_json::to_string(&item.content)?,item.created_at,item.updated_at],
    )?;
    Ok(())
}

/// Called only by Core lifecycle code, within the turn's terminal transaction.
/// Progress quotas cannot prevent these bounded, previously reserved final events.
pub(super) fn finalize(
    tx: &Transaction<'_>,
    thread_id: &str,
    turn_id: &str,
    status: TurnStatus,
    reason: Option<&str>,
) -> Result<()> {
    let open = {
        let mut statement = tx.prepare(&format!("SELECT {FIELDS} FROM turn_items WHERE turn_id=?1 AND status='in_progress' ORDER BY ordinal"))?;
        statement
            .query_map([turn_id], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for previous in open {
        let mut content = previous.content.clone();
        content.status = match status {
            TurnStatus::Completed if content.kind == ItemKind::AssistantMessage => {
                ItemStatus::Completed
            }
            TurnStatus::Failed => ItemStatus::Failed,
            _ => ItemStatus::Interrupted,
        };
        if content.error.is_none() && content.status != ItemStatus::Completed {
            // Lifecycle reasons are internal identifiers, bounded independently
            // from executor-controlled text so completion cannot exhaust space.
            content.error = Some(reason.unwrap_or("turn_ended").chars().take(128).collect());
        }
        persist(
            tx,
            thread_id,
            turn_id,
            "item.completed",
            content,
            Some(&previous),
        )?;
    }
    Ok(())
}

pub(super) fn list(db: &Connection, request: ItemListParams) -> Result<ItemListResult> {
    let after = request.after.unwrap_or(0);
    ensure!(
        after <= i64::MAX as u64,
        rpc(INVALID_PARAMS, "item cursor exceeds supported range")
    );
    let turn = get_turn(db, &request.turn_id)?;
    if turn.thread_id != request.thread_id {
        return Err(rpc(NOT_FOUND, "turn not found in thread"));
    }
    let limit = request.limit.unwrap_or(100).clamp(1, 100) as usize;
    let mut statement = db.prepare(&format!(
        "SELECT {FIELDS} FROM turn_items WHERE turn_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT ?3"
    ))?;
    let mut rows = statement.query(params![request.turn_id, after, limit + 1])?;
    let mut items = Vec::new();
    let mut bytes = 128; // Envelope and separators stay within the frame budget.
    let mut has_more = false;
    while let Some(next) = rows.next()? {
        let item = row(next)?;
        let size = serde_json::to_vec(&item)?.len() + 1;
        if items.len() == limit || bytes + size > MAX_FRAME_BYTES / 2 {
            has_more = true;
            break;
        }
        bytes += size;
        items.push(item);
    }
    Ok(ItemListResult {
        next_cursor: items.last().map_or(after, |item| item.ordinal),
        items,
        has_more,
    })
}

#[cfg(test)]
mod tests;
