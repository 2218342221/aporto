//! Bounded Responses SSE decoding and public assistant-message snapshots.
//!
//! Tool calls and private reasoning are retained only in the final Responses
//! value. The caller validates that value before dispatching any tool.
use super::{MAX_WIRE_BYTES, RunObserver};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_UPDATES_PER_MESSAGE: usize = 32;
const MAX_UPDATES_PER_TURN: usize = 300;
const MAX_MESSAGES_PER_TURN: usize = 256;
const MAX_OUTPUT_ITEMS: usize = 1024;
const UPDATE_INTERVAL: Duration = Duration::from_millis(50);

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn assistant_text(item: &Value) -> Result<String> {
    let parts = item
        .get("content")
        .and_then(Value::as_array)
        .context("invalid assistant message content")?;
    ensure!(
        parts.len() <= MAX_OUTPUT_ITEMS,
        "too many assistant content parts"
    );
    let mut texts = Vec::new();
    for part in parts {
        let value = match part["type"].as_str() {
            Some("output_text") => part.get("text"),
            Some("refusal") => part.get("refusal"),
            _ => None,
        };
        if let Some(value) = value {
            texts.push(value.as_str().context("invalid assistant text")?);
        }
    }
    Ok(texts.join("\n"))
}

fn message_phase(item: &Value, has_calls: bool) -> Result<&'static str> {
    match item.get("phase") {
        Some(Value::String(phase)) if phase == "commentary" => Ok("commentary"),
        Some(Value::String(phase)) if phase == "final_answer" => Ok("final_answer"),
        None | Some(Value::Null) => Ok(if has_calls {
            "commentary"
        } else {
            "final_answer"
        }),
        _ => bail!("invalid assistant message phase"),
    }
}

struct Message {
    parts: BTreeMap<usize, String>,
    stored_bytes: usize,
    truncated: bool,
    phase: &'static str,
    emitted: bool,
    last_sent: String,
    last_sent_at: Instant,
    updates: usize,
}

impl Message {
    fn new() -> Self {
        Self {
            parts: BTreeMap::new(),
            stored_bytes: 0,
            truncated: false,
            phase: "commentary",
            emitted: false,
            last_sent: String::new(),
            last_sent_at: Instant::now(),
            updates: 0,
        }
    }

    fn append(&mut self, index: usize, delta: &str) {
        let kept = prefix(delta, MAX_TEXT_BYTES.saturating_sub(self.stored_bytes));
        if !kept.is_empty() {
            self.parts.entry(index).or_default().push_str(kept);
            self.stored_bytes += kept.len();
        }
        self.truncated |= kept.len() != delta.len();
    }

    fn replace(&mut self, text: &str) {
        self.parts.clear();
        let kept = prefix(text, MAX_TEXT_BYTES);
        self.parts.insert(0, kept.to_owned());
        self.stored_bytes = kept.len();
        self.truncated = kept.len() != text.len();
    }

    fn text(&self) -> String {
        let text = self
            .parts
            .values()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        prefix(&text, MAX_TEXT_BYTES).to_owned()
    }

    fn item(&self, round: u32, index: usize, status: &str, text: &str) -> Value {
        let truncated = self.truncated
            || self
                .stored_bytes
                .saturating_add(self.parts.len().saturating_sub(1))
                > MAX_TEXT_BYTES;
        json!({"item":{
            "id":format!("message:{round}:{index}"), "kind":"assistant_message",
            "status":status, "phase":self.phase, "name":null, "text":text,
            "input":null, "output":null, "error":null, "elapsed_ms":null,
            "truncated":truncated
        }})
    }
}

/// A single user turn can contain several model rounds. Keeping the update budget
/// here prevents a chatty provider from exhausting the persistent event quota.
pub(super) struct AssistantTranscript {
    observer: Arc<dyn RunObserver>,
    messages: BTreeMap<(u32, usize), Message>,
    updates: usize,
}

pub(super) struct RoundMessages {
    pub(super) final_answer: String,
    pub(super) has_commentary: bool,
}

impl AssistantTranscript {
    pub(super) fn new(observer: Arc<dyn RunObserver>) -> Self {
        Self {
            observer,
            messages: BTreeMap::new(),
            updates: 0,
        }
    }

    fn message(&mut self, round: u32, index: usize) -> Result<&mut Message> {
        ensure!(index < MAX_OUTPUT_ITEMS, "too many Responses output items");
        ensure!(
            self.messages.contains_key(&(round, index))
                || self.messages.len() < MAX_MESSAGES_PER_TURN,
            "too many assistant messages in one turn"
        );
        Ok(self
            .messages
            .entry((round, index))
            .or_insert_with(Message::new))
    }

    async fn publish(&mut self, round: u32, index: usize) -> Result<()> {
        let message = self
            .messages
            .get_mut(&(round, index))
            .expect("message registered");
        let text = message.text();
        if text.trim().is_empty() || text == message.last_sent {
            return Ok(());
        }
        let kind = if !message.emitted {
            "item.started"
        } else {
            if message.updates >= MAX_UPDATES_PER_MESSAGE
                || self.updates >= MAX_UPDATES_PER_TURN
                || (message.last_sent_at.elapsed() < UPDATE_INTERVAL
                    && text.len().abs_diff(message.last_sent.len()) < 1024)
            {
                return Ok(());
            }
            message.updates += 1;
            self.updates += 1;
            "item.updated"
        };
        self.observer
            .emit(kind, message.item(round, index, "in_progress", &text))
            .await?;
        message.emitted = true;
        message.last_sent = text;
        message.last_sent_at = Instant::now();
        Ok(())
    }

    async fn delta(&mut self, round: u32, index: usize, part: usize, delta: &str) -> Result<()> {
        ensure!(part < MAX_OUTPUT_ITEMS, "too many assistant content parts");
        self.message(round, index)?.append(part, delta);
        self.publish(round, index).await
    }

    async fn snapshot(&mut self, round: u32, index: usize, item: &Value) -> Result<()> {
        if item["type"] != "message" {
            return Ok(());
        }
        ensure!(
            item.get("role").is_none_or(|role| role == "assistant"),
            "invalid assistant message role"
        );
        let text = assistant_text(item)?;
        let message = self.message(round, index)?;
        // The completed response is authoritative, including any correction to
        // streamed text. Replacing the same item avoids duplicate final messages.
        message.replace(&text);
        if item.get("phase").and_then(Value::as_str).is_some() {
            message.phase = message_phase(item, true)?;
        }
        self.publish(round, index).await
    }

    pub(super) async fn complete_round(
        &mut self,
        round: u32,
        output: &[Value],
    ) -> Result<RoundMessages> {
        ensure!(
            output.len() <= MAX_OUTPUT_ITEMS,
            "too many Responses output items"
        );
        for ((message_round, index), _) in self.messages.range((round, 0)..=(round, usize::MAX)) {
            ensure!(
                *message_round == round
                    && output
                        .get(*index)
                        .is_some_and(|item| item["type"] == "message"),
                "streamed assistant message missing from completed response"
            );
        }
        let has_calls = output.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("custom_tool_call" | "function_call")
            )
        });
        let mut answer = Vec::new();
        let mut has_commentary = false;
        // Validate every public message before writing completion events.
        let messages = output
            .iter()
            .enumerate()
            .filter(|(_, item)| item["type"] == "message")
            .map(|(index, item)| {
                ensure!(
                    item.get("role").is_none_or(|role| role == "assistant"),
                    "invalid assistant message role"
                );
                Ok((
                    index,
                    assistant_text(item)?,
                    message_phase(item, has_calls)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        for (index, text, phase) in messages {
            if phase == "final_answer" && !text.trim().is_empty() {
                answer.push(text.clone());
            }
            has_commentary |= phase == "commentary" && !text.trim().is_empty();
            let observer = self.observer.clone();
            let message = self.message(round, index)?;
            message.replace(&text);
            message.phase = phase;
            if text.trim().is_empty() && !message.emitted {
                continue;
            }
            let public = message.text();
            if !message.emitted {
                observer
                    .emit(
                        "item.started",
                        message.item(round, index, "in_progress", &public),
                    )
                    .await?;
                message.emitted = true;
            }
            observer
                .emit(
                    "item.completed",
                    message.item(round, index, "completed", &public),
                )
                .await?;
            message.last_sent = public;
        }
        Ok(RoundMessages {
            final_answer: answer.join("\n"),
            has_commentary,
        })
    }
}

/// Split at bytes, then decode complete lines, so arbitrary UTF-8 fragmentation
/// never replaces codepoints. SSE comments and multiple data lines are supported.
#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: String,
    event: String,
    after_cr: bool,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<(String, String)>> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\n' | b'\r') {
                self.end_line(&mut events)?;
                self.after_cr = byte == b'\r';
            } else {
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    fn end_line(&mut self, events: &mut Vec<(String, String)>) -> Result<()> {
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| anyhow::anyhow!("invalid Responses stream encoding"))?;
        if line.is_empty() {
            if !self.data.is_empty() {
                self.data.pop(); // The last data-line newline is not event data.
                events.push((
                    std::mem::take(&mut self.event),
                    std::mem::take(&mut self.data),
                ));
            } else {
                self.event.clear();
            }
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                "event" => {
                    self.event.clear();
                    self.event.push_str(value);
                }
                _ => {}
            }
        }
        self.line.clear();
        Ok(())
    }
}

fn output_index(event: &Value) -> Result<usize> {
    let index = event
        .get("output_index")
        .and_then(Value::as_u64)
        .context("missing Responses stream output index")?;
    ensure!(
        index < MAX_OUTPUT_ITEMS as u64,
        "too many Responses output items"
    );
    Ok(index as usize)
}

pub(super) async fn read_response(
    response: reqwest::Response,
    transcript: &mut AssistantTranscript,
    round: u32,
) -> Result<Value> {
    let status = response.status();
    // Never read or expose HTTP error bodies: providers can echo request secrets.
    ensure!(
        status.is_success(),
        "model request failed with HTTP {status}"
    );
    let is_sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        });
    let mut body = response.bytes_stream();
    let mut total_bytes = 0usize;
    let mut json_bytes = Vec::new();
    let mut decoder = SseDecoder::default();
    let mut done_items = BTreeMap::new();
    let mut item_types = BTreeMap::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("Responses stream transport failed"))?;
        total_bytes = total_bytes.saturating_add(chunk.len());
        ensure!(
            total_bytes <= MAX_WIRE_BYTES,
            "model response exceeds 8 MiB"
        );
        if !is_sse {
            json_bytes.extend_from_slice(&chunk);
            continue;
        }
        for (event_name, data) in decoder.push(&chunk)? {
            ensure!(
                data.trim() != "[DONE]",
                "Responses stream ended before response.completed"
            );
            let event: Value = serde_json::from_str(&data)
                .map_err(|_| anyhow::anyhow!("invalid Responses stream event"))?;
            ensure!(
                event_name.is_empty()
                    || event
                        .get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == event_name),
                "Responses stream event type mismatch"
            );
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or(&event_name);
            match kind {
                "error" | "response.failed" => bail!("model returned a streaming error"),
                "response.incomplete" => {
                    bail!("model response is not completed; no tool calls were executed")
                }
                "response.output_item.added" | "response.output_item.done" => {
                    let index = output_index(&event)?;
                    let item = event.get("item").context("missing Responses stream item")?;
                    let item_type = item
                        .get("type")
                        .and_then(Value::as_str)
                        .context("missing Responses stream item type")?;
                    ensure!(
                        item_types
                            .get(&index)
                            .is_none_or(|previous| previous == item_type),
                        "Responses stream item type changed"
                    );
                    item_types.insert(index, item_type.to_owned());
                    transcript.snapshot(round, index, item).await?;
                    if kind == "response.output_item.done" {
                        done_items.insert(index, item.clone());
                    }
                }
                "response.output_text.delta" | "response.refusal.delta" => {
                    let index = output_index(&event)?;
                    ensure!(
                        item_types.get(&index).is_none_or(|kind| kind == "message"),
                        "text delta belongs to a non-message item"
                    );
                    let part = event
                        .get("content_index")
                        .and_then(Value::as_u64)
                        .context("missing Responses stream content index")?;
                    ensure!(
                        part < MAX_OUTPUT_ITEMS as u64,
                        "too many assistant content parts"
                    );
                    let delta = event
                        .get("delta")
                        .and_then(Value::as_str)
                        .context("missing Responses stream text")?;
                    transcript.delta(round, index, part as usize, delta).await?;
                }
                "response.completed" => {
                    let mut completed = event
                        .get("response")
                        .filter(|value| value.is_object())
                        .context("missing completed Responses value")?
                        .clone();
                    if completed.get("status").is_none() {
                        completed["status"] = json!("completed");
                    }
                    // Some compatible gateways send only terminal metadata, with
                    // full output carried by output_item.done events (as Codex does).
                    if completed.get("output").is_none() {
                        ensure!(
                            item_types.len() == done_items.len(),
                            "Responses output item did not complete"
                        );
                        let mut output = Vec::with_capacity(done_items.len());
                        for (index, item) in done_items {
                            ensure!(
                                index == output.len(),
                                "Responses output indexes are not contiguous"
                            );
                            output.push(item);
                        }
                        completed["output"] = json!(output);
                    }
                    let output = completed
                        .get("output")
                        .and_then(Value::as_array)
                        .context("missing Responses output")?;
                    for (index, kind) in &item_types {
                        ensure!(
                            output
                                .get(*index)
                                .and_then(|item| item["type"].as_str())
                                .is_some_and(|value| value == kind),
                            "completed Responses output differs from streamed item types"
                        );
                    }
                    return Ok(completed);
                }
                // Reasoning, reasoning summaries, encrypted payloads and tool
                // argument deltas never become public assistant transcript text.
                _ => {}
            }
        }
    }
    if is_sse {
        bail!("Responses stream ended before response.completed");
    }
    serde_json::from_slice(&json_bytes).map_err(|_| anyhow::anyhow!("invalid Responses API JSON"))
}
