//! Codex-style PTC, using a fresh QuickJS runtime per cell.
//!
//! Source is an async function body, not an ES module: top-level await and return
//! work, import/export do not. Only JSON crosses the native tool boundary. Each
//! runtime has a dedicated thread, an allocator limit, and a CPU interrupt hook.
//! This is not process isolation; a native engine crash would affect the host.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Exception, Function, Promise};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::types::{Limits, ToolBroker, ToolDefinition};

pub const MAX_ACTIVE_CELLS: usize = 8;
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_STORE_BYTES: usize = 1024 * 1024;
pub const MAX_STORE_KEYS: usize = 128;
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_PENDING_CALLBACKS: usize = 128;
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_ITEMS: usize = 4096;
const MAX_BUFFERED_YIELDS: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
pub struct ObserveOptions {
    pub yield_time_ms: Option<u64>,
    pub max_output_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    Running,
    Completed,
    Failed,
    Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellResult {
    pub status: CellStatus,
    pub cell_id: String,
    pub output: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub truncated: bool,
}

pub struct PtcSession {
    inner: Arc<Inner>,
}

struct Inner {
    broker: Arc<dyn ToolBroker>,
    limits: Limits,
    cells: Mutex<HashMap<String, Arc<Cell>>>,
    store: Mutex<BTreeMap<String, Value>>,
    session_id: uuid::Uuid,
    next_id: AtomicU64,
    closed: AtomicBool,
    tool_slots: Semaphore,
    tool_calls: AtomicUsize,
    tool_gate: RwLock<()>,
}

struct Cell {
    id: String,
    state: Mutex<CellState>,
    observer: tokio::sync::Mutex<()>,
    changed: Notify,
    cancel: CancellationToken,
    requested_termination: AtomicBool,
    pending_callbacks: AtomicUsize,
    callback_error: Mutex<Option<String>>,
    deadline: Instant,
    kv: Mutex<CellStore>,
}

struct CellState {
    status: CellStatus,
    output: Vec<String>,
    total_output_bytes: usize,
    total_output_items: usize,
    truncated: bool,
    yields: VecDeque<(Vec<String>, bool)>,
    error: Option<String>,
}

struct CellStore {
    values: BTreeMap<String, Value>,
    writes: BTreeMap<String, Value>,
}

impl PtcSession {
    pub fn new(broker: Arc<dyn ToolBroker>, limits: Limits) -> Self {
        let parallel = limits.max_parallel.clamp(1, 128);
        Self {
            inner: Arc::new(Inner {
                broker,
                limits,
                cells: Mutex::default(),
                store: Mutex::default(),
                session_id: uuid::Uuid::new_v4(),
                next_id: AtomicU64::new(1),
                closed: AtomicBool::new(false),
                tool_slots: Semaphore::new(parallel),
                tool_calls: AtomicUsize::new(0),
                tool_gate: RwLock::new(()),
            }),
        }
    }

    pub async fn exec(&self, source: &str, options: ObserveOptions) -> Result<CellResult> {
        if source.trim().is_empty() {
            bail!("exec source must not be empty");
        }
        if source.len() > MAX_SOURCE_BYTES {
            bail!("exec source exceeds {MAX_SOURCE_BYTES} bytes");
        }
        validate_limits(&self.inner.limits)?;
        let definitions = catalog(self.inner.broker.definitions())?;
        let id = format!(
            "cell-{}-{}",
            self.inner.session_id,
            self.inner.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let cell = Arc::new(Cell {
            id: id.clone(),
            state: Mutex::new(CellState {
                status: CellStatus::Running,
                output: Vec::new(),
                total_output_bytes: 0,
                total_output_items: 0,
                truncated: false,
                yields: VecDeque::new(),
                error: None,
            }),
            observer: tokio::sync::Mutex::new(()),
            changed: Notify::new(),
            cancel: CancellationToken::new(),
            requested_termination: AtomicBool::new(false),
            pending_callbacks: AtomicUsize::new(0),
            callback_error: Mutex::new(None),
            deadline: Instant::now() + Duration::from_millis(self.inner.limits.cell_timeout_ms),
            kv: Mutex::new(CellStore {
                values: self.inner.store.lock().unwrap().clone(),
                writes: BTreeMap::new(),
            }),
        });
        {
            let mut cells = self.inner.cells.lock().unwrap();
            if self.inner.closed.load(Ordering::Acquire) {
                bail!("PTC session is closed");
            }
            // Finished cells with unobserved output also retain a slot until wait consumes them.
            if cells.len() >= MAX_ACTIVE_CELLS {
                bail!("active cell limit ({MAX_ACTIVE_CELLS}) reached; wait for existing cells");
            }
            cells.insert(id.clone(), Arc::clone(&cell));
        }
        let inner = Arc::clone(&self.inner);
        let running_cell = Arc::clone(&cell);
        let source = source.to_owned();
        let spawn = std::thread::Builder::new()
            .name(format!("ptc-{id}"))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(run_cell(
                        Arc::clone(&inner),
                        Arc::clone(&running_cell),
                        definitions,
                        source,
                    ))
                }))
                .unwrap_or_else(|_| Err(anyhow!("PTC runtime thread panicked")));
                finish_cell(&inner, &running_cell, result);
            });
        if let Err(error) = spawn {
            self.inner.cells.lock().unwrap().remove(&id);
            return Err(error.into());
        }
        self.observe(cell, options).await
    }

    pub async fn wait(
        &self,
        id: &str,
        options: ObserveOptions,
        terminate: bool,
    ) -> Result<CellResult> {
        let cell = self
            .inner
            .cells
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown or already consumed cell: {id}"))?;
        let options = if terminate {
            request_termination(&cell);
            ObserveOptions {
                yield_time_ms: Some(self.inner.limits.cell_timeout_ms),
                ..options
            }
        } else {
            options
        };
        self.observe(cell, options).await
    }

    pub async fn close(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Release);
        let cells: Vec<_> = self.inner.cells.lock().unwrap().values().cloned().collect();
        for cell in &cells {
            request_termination(cell);
        }
        for cell in cells {
            loop {
                let changed = cell.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if cell.state.lock().unwrap().status != CellStatus::Running {
                    break;
                }
                changed.await;
            }
        }
        self.inner.cells.lock().unwrap().clear();
        Ok(())
    }

    async fn observe(&self, cell: Arc<Cell>, options: ObserveOptions) -> Result<CellResult> {
        let _observer = cell
            .observer
            .try_lock()
            .map_err(|_| anyhow!("cell already has an active observer"))?;
        let wait = options
            .yield_time_ms
            .unwrap_or(10_000)
            .min(self.inner.limits.cell_timeout_ms);
        let until = tokio::time::Instant::now() + Duration::from_millis(wait);
        let max_bytes = options
            .max_output_bytes
            .unwrap_or(self.inner.limits.max_output_bytes)
            .min(self.inner.limits.max_output_bytes);
        loop {
            let changed = cell.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let ready = {
                let state = cell.state.lock().unwrap();
                state.status != CellStatus::Running || !state.yields.is_empty()
            };
            if ready || tokio::time::Instant::now() >= until {
                break;
            }
            tokio::select! { _ = changed => {}, _ = tokio::time::sleep_until(until) => break }
        }
        let mut state = cell.state.lock().unwrap();
        let (mut output, mut truncated, status, error) =
            if let Some((output, truncated)) = state.yields.pop_front() {
                (output, truncated, CellStatus::Running, None)
            } else {
                let output = std::mem::take(&mut state.output);
                let truncated = std::mem::take(&mut state.truncated);
                (output, truncated, state.status, state.error.clone())
            };
        let mut remaining = max_bytes;
        for item in &mut output {
            if item.len() > remaining {
                truncate_utf8(item, remaining);
                truncated = true;
            }
            remaining = remaining.saturating_sub(item.len());
        }
        output.retain(|item| !item.is_empty());
        let result = CellResult {
            status,
            cell_id: cell.id.clone(),
            output,
            error,
            truncated,
        };
        drop(state);
        if result.status != CellStatus::Running {
            self.inner.cells.lock().unwrap().remove(&cell.id);
        }
        Ok(result)
    }
}

impl Drop for PtcSession {
    fn drop(&mut self) {
        self.inner.closed.store(true, Ordering::Release);
        for cell in self.inner.cells.lock().unwrap().values() {
            request_termination(cell);
        }
    }
}

fn validate_limits(limits: &Limits) -> Result<()> {
    if !(1..=3_600_000).contains(&limits.cell_timeout_ms) {
        bail!("cell_timeout_ms must be between 1 and 3600000");
    }
    if !(1..=1024).contains(&limits.memory_mb) {
        bail!("memory_mb must be between 1 and 1024");
    }
    if !(1..=16 * 1024 * 1024).contains(&limits.max_output_bytes) {
        bail!("max_output_bytes must be between 1 and 16777216");
    }
    if !(1..=4096).contains(&limits.max_tool_calls) {
        bail!("max_tool_calls must be between 1 and 4096");
    }
    if !(1..=128).contains(&limits.max_parallel) {
        bail!("max_parallel must be between 1 and 128");
    }
    Ok(())
}

pub(crate) fn validate_tool_catalog(definitions: &[ToolDefinition]) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for definition in definitions {
        let name = &definition.name;
        if name.is_empty()
            || matches!(name.as_str(), "exec" | "wait")
            || !name.chars().enumerate().all(|(index, ch)| {
                ch == '_'
                    || ch == '$'
                    || ch.is_ascii_alphabetic()
                    || (index > 0 && ch.is_ascii_digit())
            })
        {
            bail!("invalid nested tool name: {name}");
        }
        if !names.insert(name) {
            bail!("duplicate tool name: {name}");
        }
    }
    if serde_json::to_vec(definitions)?.len() > MAX_CATALOG_BYTES {
        bail!("tool catalog exceeds {MAX_CATALOG_BYTES} bytes");
    }
    Ok(())
}

fn catalog(definitions: Vec<ToolDefinition>) -> Result<BTreeMap<String, ToolDefinition>> {
    validate_tool_catalog(&definitions)?;
    Ok(definitions
        .into_iter()
        .map(|definition| (definition.name.clone(), definition))
        .collect())
}

fn request_termination(cell: &Cell) {
    let mut state = cell.state.lock().unwrap();
    // A termination observation consumes all prior unobserved yield chunks.
    let mut output = Vec::new();
    while let Some((chunk, truncated)) = state.yields.pop_front() {
        output.extend(chunk);
        state.truncated |= truncated;
    }
    output.append(&mut state.output);
    state.output = output;
    if state.status == CellStatus::Running {
        cell.requested_termination.store(true, Ordering::Release);
        cell.cancel.cancel();
        cell.changed.notify_waiters();
    }
}

fn finish_cell(inner: &Inner, cell: &Cell, result: Result<()>) {
    cell.cancel.cancel(); // Also cancels unawaited native tool futures.
    let mut state = cell.state.lock().unwrap();
    if cell.requested_termination.load(Ordering::Acquire) {
        state.status = CellStatus::Terminated;
    } else if Instant::now() >= cell.deadline {
        state.status = CellStatus::Failed;
        state.error = Some("cell deadline exceeded".into());
    } else {
        let mut store = inner.store.lock().unwrap();
        let mut merged = store.clone();
        merged.extend(cell.kv.lock().unwrap().writes.clone());
        let commit_error = check_store(&merged).err();
        if commit_error.is_none() {
            *store = merged;
        }
        // Like Codex, normal script failure still commits writes; termination does not.
        let error = result.err().or(commit_error);
        state.status = if error.is_some() {
            CellStatus::Failed
        } else {
            CellStatus::Completed
        };
        state.error = error.map(|error| bounded_error(error.to_string()));
    }
    cell.changed.notify_waiters();
}

async fn run_cell(
    inner: Arc<Inner>,
    cell: Arc<Cell>,
    definitions: BTreeMap<String, ToolDefinition>,
    source: String,
) -> Result<()> {
    let runtime = AsyncRuntime::new()?;
    runtime
        .set_memory_limit(inner.limits.memory_mb * 1024 * 1024)
        .await;
    runtime.set_max_stack_size(256 * 1024).await;
    let interrupted = Arc::clone(&cell);
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            interrupted.cancel.is_cancelled() || Instant::now() >= interrupted.deadline
        })))
        .await;
    let context = AsyncContext::full(&runtime).await?;
    let execution_inner = Arc::clone(&inner);
    let execution_cell = Arc::clone(&cell);
    let execution = context.async_with(async move |ctx| {
        install_globals(&ctx, execution_inner, execution_cell, definitions)?;
        // Deliberately an async-function-body subset, not an ES module or Node REPL.
        let program = format!("(async function() {{\n{source}\n}})()");
        let promise = ctx
            .eval::<Promise, _>(program)
            .map_err(|error| anyhow!(js_error(&ctx, error)))?;
        promise
            .into_future::<rquickjs::Value>()
            .await
            .map_err(|error| anyhow!(js_error(&ctx, error)))?;
        Ok(())
    });
    let result = tokio::select! {
        biased;
        _ = cell.cancel.cancelled() => Err(anyhow!(cell.callback_error.lock().unwrap().clone().unwrap_or_else(|| "cell cancelled".into()))),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(cell.deadline)) => Err(anyhow!("cell deadline exceeded")),
        result = execution => result,
    };
    cell.cancel.cancel();
    result
}

fn install_globals<'js>(
    ctx: &Ctx<'js>,
    inner: Arc<Inner>,
    cell: Arc<Cell>,
    definitions: BTreeMap<String, ToolDefinition>,
) -> Result<()> {
    let metadata: Vec<_> = definitions
        .iter()
        .map(|(name, definition)| {
            json!({
                "name": name,
                "description": definition.description,
            })
        })
        .collect();
    ctx.globals()
        .set("__catalog_json", serde_json::to_string(&metadata)?)?;
    let definitions = Arc::new(definitions);
    let tool_cell = Arc::clone(&cell);
    let tool_inner = Arc::clone(&inner);
    ctx.globals().set(
        "__call_json",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String, args: String| -> rquickjs::Result<Promise<'js>> {
                let cell = Arc::clone(&tool_cell);
                let inner = Arc::clone(&tool_inner);
                // Reject synchronously before creating a native future or retaining its arguments.
                let call_number = inner.tool_calls.fetch_add(1, Ordering::Relaxed);
                if call_number >= inner.limits.max_tool_calls {
                    return Err(Exception::throw_message(&ctx, "tool call budget exceeded"));
                }
                if args.len() > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("tool arguments exceed {MAX_TOOL_ARGUMENT_BYTES} bytes"),
                    ));
                }
                let definition = definitions
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| Exception::throw_message(&ctx, "unknown tool"))?;
                if cell
                    .pending_callbacks
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                        (pending < MAX_PENDING_CALLBACKS).then_some(pending + 1)
                    })
                    .is_err()
                {
                    return Err(Exception::throw_message(
                        &ctx,
                        "pending callback limit exceeded",
                    ));
                }
                let pending_guard = PendingGuard(Arc::clone(&cell));
                let (promise, resolve, _reject) = Promise::new(&ctx)?;
                let callback_ctx = ctx.clone();
                ctx.spawn(async move {
                    let _pending = pending_guard;
                    let result = async {
                        let args: Value = serde_json::from_str(&args)?;
                        let cancel = cell.cancel.child_token();
                        let result = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => Err(anyhow!("tool call cancelled")),
                            result = async {
                                let _permit = inner.tool_slots.acquire().await?;
                                if definition.parallel {
                                    let _gate = inner.tool_gate.read().await;
                                    inner.broker.call(&definition.name, args, cancel.clone()).await
                                } else {
                                    let _gate = inner.tool_gate.write().await;
                                    inner.broker.call(&definition.name, args, cancel.clone()).await
                                }
                            } => result,
                        }?;
                        if serde_json::to_vec(&result)?.len() > MAX_TOOL_RESULT_BYTES {
                            bail!("tool result exceeds {MAX_TOOL_RESULT_BYTES} bytes");
                        }
                        Ok(result)
                    }
                    .await;
                    // The stock async adapter prints to stdout if resolution fails. Fail
                    // the cell instead; stdout may be the Core JSONL protocol transport.
                    if let Err(error) = resolve.call::<_, ()>((envelope(result),)) {
                        *cell.callback_error.lock().unwrap() = Some(bounded_error(format!(
                            "tool result delivery failed: {}",
                            js_error(&callback_ctx, error)
                        )));
                        cell.cancel.cancel();
                    }
                });
                Ok(promise)
            },
        )?,
    )?;
    let output_cell = Arc::clone(&cell);
    let max_output = inner.limits.max_output_bytes;
    ctx.globals().set(
        "__text",
        Function::new(ctx.clone(), move |mut text: String| {
            let mut state = output_cell.state.lock().unwrap();
            if state.total_output_items >= MAX_OUTPUT_ITEMS {
                state.truncated = true;
                return;
            }
            state.total_output_items += 1;
            let remaining = max_output.saturating_sub(state.total_output_bytes);
            if text.len() > remaining {
                truncate_utf8(&mut text, remaining);
                state.truncated = true;
            }
            state.total_output_bytes += text.len();
            if !text.is_empty() {
                state.output.push(text);
            }
        })?,
    )?;
    let yield_cell = Arc::clone(&cell);
    ctx.globals().set(
        "__yield",
        Function::new(ctx.clone(), move || {
            let mut state = yield_cell.state.lock().unwrap();
            if state.yields.len() >= MAX_BUFFERED_YIELDS {
                state.truncated = true;
                return;
            }
            let output = std::mem::take(&mut state.output);
            let truncated = std::mem::take(&mut state.truncated);
            state.yields.push_back((output, truncated));
            yield_cell.changed.notify_waiters();
        })?,
    )?;
    let store_cell = Arc::clone(&cell);
    ctx.globals().set(
        "__store_json",
        Function::new(ctx.clone(), move |key: String, value: String| {
            let result = (|| {
                if key.len() > 256 {
                    bail!("store key exceeds 256 bytes");
                }
                if value.len() > MAX_STORE_BYTES {
                    bail!("stored value exceeds {MAX_STORE_BYTES} bytes");
                }
                let value: Value = serde_json::from_str(&value)?;
                let mut kv = store_cell.kv.lock().unwrap();
                let mut merged = kv.values.clone();
                merged.insert(key.clone(), value.clone());
                check_store(&merged)?;
                kv.values = merged;
                kv.writes.insert(key, value);
                Ok(Value::Null)
            })();
            envelope(result)
        })?,
    )?;
    ctx.globals().set(
        "__load_json",
        Function::new(ctx.clone(), move |key: String| {
            match cell.kv.lock().unwrap().values.get(&key) {
                Some(value) => json!({"found":true,"value":value}).to_string(),
                None => "{\"found\":false}".to_owned(),
            }
        })?,
    )?;
    ctx.eval::<(), _>(BOOTSTRAP)
        .map_err(|error| anyhow!(js_error(ctx, error)))?;
    Ok(())
}

struct PendingGuard(Arc<Cell>);
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.pending_callbacks.fetch_sub(1, Ordering::Relaxed);
    }
}

fn envelope(result: Result<Value>) -> String {
    match result {
        Ok(value) => json!({"ok":true,"value":value}).to_string(),
        Err(error) => json!({"ok":false,"error":bounded_error(error.to_string())}).to_string(),
    }
}

fn check_store(store: &BTreeMap<String, Value>) -> Result<()> {
    if store.len() > MAX_STORE_KEYS {
        bail!("store exceeds {MAX_STORE_KEYS} keys");
    }
    if serde_json::to_vec(store)?.len() > MAX_STORE_BYTES {
        bail!("store exceeds {MAX_STORE_BYTES} bytes");
    }
    Ok(())
}

fn truncate_utf8(value: &mut String, mut bytes: usize) {
    bytes = bytes.min(value.len());
    while !value.is_char_boundary(bytes) {
        bytes -= 1;
    }
    value.truncate(bytes);
}

fn bounded_error(mut error: String) -> String {
    truncate_utf8(&mut error, 4096);
    error
}

fn js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    if error.is_exception() {
        let value = ctx.catch();
        if let Some(object) = value.as_object() {
            let message = object.get::<_, String>("message").ok();
            let stack = object.get::<_, String>("stack").ok();
            match (message, stack) {
                (Some(message), Some(stack)) if !message.is_empty() => {
                    return format!("{message}\n{stack}");
                }
                (_, Some(stack)) if !stack.is_empty() => return stack,
                (Some(message), _) if !message.is_empty() => return message,
                _ => {}
            }
        }
        if let Some(string) = value.as_string()
            && let Ok(text) = string.to_string()
        {
            return if text.is_empty() {
                "JavaScript threw an empty string".into()
            } else {
                text
            };
        }
    }
    error.to_string()
}

const BOOTSTRAP: &str = r#"
(() => {
  const call = globalThis.__call_json, output = globalThis.__text;
  const put = globalThis.__store_json, get = globalThis.__load_json, yieldNow = globalThis.__yield;
  const catalog = JSON.parse(globalThis.__catalog_json);
  for (const key of ['__call_json','__text','__store_json','__load_json','__yield','__catalog_json']) delete globalThis[key];
  const decode = raw => { const result = JSON.parse(raw); if (!result.ok) throw new Error(result.error); return result.value; };
  const tools = Object.create(null);
  for (const item of catalog) {
    tools[item.name] = async (args = {}) => {
      const json = JSON.stringify(args);
      if (json === undefined) throw new TypeError('tool arguments must be JSON serializable');
      return decode(await call(item.name, json));
    };
    Object.freeze(item);
  }
  Object.defineProperties(globalThis, {
    tools: {value: Object.freeze(tools)}, ALL_TOOLS: {value: Object.freeze(catalog)},
    text: {value: value => { const result = typeof value === 'string' ? value : JSON.stringify(value); output(result === undefined ? 'undefined' : result); }},
    store: {value: (key,value) => { if (typeof key !== 'string') throw new TypeError('store key must be a string'); const json=JSON.stringify(value); if(json===undefined)throw new TypeError('store value must be JSON serializable'); decode(put(key,json)); }},
    load: {value: key => { if(typeof key!=='string')throw new TypeError('load key must be a string'); const result=JSON.parse(get(key)); return result.found ? result.value : undefined; }},
    yield_control: {value: () => { yieldNow(); }},
  });
})();
"#;
