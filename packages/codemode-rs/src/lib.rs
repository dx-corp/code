//! Script orchestration, never execution authority. The host authorizes,
//! journals and executes every request using its existing tool boundary.
//! Each script has a fresh native QuickJS VM with no I/O or module loader.

use std::sync::{Arc, Mutex, mpsc as sync_mpsc};
use std::time::{Duration, Instant};

use rquickjs::{Context, Function, Promise, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests;

pub const TOOL_NAME: &str = "codemode";
pub const DESCRIPTION: &str = "Compose tools in a sandboxed JavaScript async function. Call await tools.<name>(args), run independent reads with Promise.allSettled, and emit only useful results with text(value) or return. Nested calls retain their own policy and receipts. No filesystem, network, process, modules or timers. Errors reject and calls already executed are not undone. Use ALL_TOOLS for names and schemas. Maximum 64 calls and 64 KiB output per script; hard deadline 60 seconds. Tools requiring a conversational answer or confirmation must be called directly.";

pub fn schema() -> Value {
    serde_json::json!({"type":"object", "properties": {
        "code":{"type":"string","minLength":1,"maxLength":65536}
    },"required":["code"],"additionalProperties":false})
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Requests in one JavaScript continuation. Hosts may run admitted reads
/// together; effects must retain the host's existing serial ordering.
#[derive(Debug)]
pub struct Call {
    pub index: usize,
    pub name: String,
    pub args: Value,
}

pub type Reply = Vec<(usize, Result<Value, String>)>;

pub enum Event {
    Calls {
        calls: Vec<Call>,
        reply: sync_mpsc::Sender<Reply>,
    },
    Done(Report),
}

#[derive(Debug)]
pub struct Report {
    pub output: Vec<String>,
    pub error: Option<String>,
}

impl Report {
    pub fn content(&self) -> String {
        let mut output = self.output.join("\n");
        if let Some(error) = &self.error {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("Script failed: ");
            output.push_str(error);
        }
        output
    }
}

/// Dropping the session interrupts a spinning VM and closes its host bridge.
/// Hosts must still settle and journal effects they already admitted.
pub struct Session {
    events: mpsc::UnboundedReceiver<Event>,
    stop: CancellationToken,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Session {
    pub fn start(
        code: String,
        tools: Vec<Tool>,
        cancel: &CancellationToken,
        deadline: Duration,
    ) -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let stop = cancel.child_token();
        let worker_stop = stop.clone();
        std::thread::spawn(move || {
            let output = Arc::new(Mutex::new(Vec::new()));
            let result = run(
                code,
                tools,
                &events_tx,
                &worker_stop,
                deadline.min(Duration::from_secs(60)),
                output.clone(),
            );
            let output = output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let _ = events_tx.send(Event::Done(Report {
                output,
                error: result.err(),
            }));
        });
        Self { events, stop }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}

fn run(
    code: String,
    tools: Vec<Tool>,
    events: &mpsc::UnboundedSender<Event>,
    stop: &CancellationToken,
    deadline: Duration,
    output: Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    if code.trim().is_empty() || code.len() > 65_536 {
        return Err("script must contain 1 to 65536 bytes".into());
    }
    let expires = Instant::now() + deadline;
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(32 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let interrupted = stop.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        interrupted.is_cancelled() || Instant::now() >= expires
    })));
    let context = Context::full(&runtime).map_err(|e| e.to_string())?;
    let pending = Arc::new(Mutex::new(Vec::new()));
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    context.with(|ctx| -> Result<(), String> {
        let pending_calls = pending.clone();
        let count = std::cell::Cell::new(0usize);
        let bridge = Function::new(ctx.clone(), move |name: String, args: String| -> rquickjs::Result<usize> {
            if count.get() >= 64 { return Err(rquickjs::Error::new_from_js_message("tool call", "bounded script", "maximum 64 nested calls")); }
            let args = serde_json::from_str(&args).map_err(|e| rquickjs::Error::new_from_js_message("JSON", "tool arguments", e.to_string()))?;
            let index = count.get();
            count.set(index + 1);
            pending_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Call { index, name, args });
            Ok(index)
        }).map_err(|e| e.to_string())?;
        let text_output = output.clone();
        let poisoned = overflow.clone();
        let emit = Function::new(ctx.clone(), move |text: String| -> rquickjs::Result<()> {
            let mut output = text_output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if output.len() >= 1024 || output.iter().map(String::len).sum::<usize>().saturating_add(text.len()) > 65_536 {
                poisoned.store(true, std::sync::atomic::Ordering::Relaxed);
                return Err(rquickjs::Error::new_from_js_message("output", "bounded script", "maximum 64 KiB script output"));
            }
            output.push(text);
            Ok(())
        }).map_err(|e| e.to_string())?;
        ctx.globals().set("__host_call", bridge).map_err(|e| e.to_string())?;
        ctx.globals().set("__host_text", emit).map_err(|e| e.to_string())?;
        ctx.globals().set("__catalog", serde_json::to_string(&tools).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let respond: Function = ctx.eval(PRELUDE).map_err(|e| js_error(&ctx, e))?;
        let source = format!("(async function(){{{code}\n}})().then(value => {{ if (value !== undefined) text(value); }})");
        let promise: Promise = ctx.eval(source).map_err(|e| js_error(&ctx, e))?;
        loop {
            if stop.is_cancelled() { return Err("cancelled".into()); }
            if Instant::now() >= expires { return Err("deadline exceeded".into()); }
            if overflow.load(std::sync::atomic::Ordering::Relaxed) { return Err("maximum 64 KiB script output".into()); }
            while ctx.execute_pending_job() {
                if stop.is_cancelled() || Instant::now() >= expires { return Err("cancelled or deadline exceeded".into()); }
            }
            if let Some(result) = promise.result::<rquickjs::Value>() {
                return result.map(|_| ()).map_err(|e| js_error(&ctx, e));
            }
            let calls = std::mem::take(&mut *pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
            if calls.is_empty() { return Err("script awaits a promise with no pending tool call".into()); }
            let (reply, responses) = sync_mpsc::channel();
            events.send(Event::Calls { calls, reply }).map_err(|_| "host disconnected")?;
            let responses = loop {
                if stop.is_cancelled() { return Err("cancelled".into()); }
                if Instant::now() >= expires { return Err("deadline exceeded".into()); }
                match responses.recv_timeout(Duration::from_millis(10)) {
                    Ok(responses) => break responses,
                    Err(sync_mpsc::RecvTimeoutError::Timeout) => {},
                    Err(sync_mpsc::RecvTimeoutError::Disconnected) => return Err("host disconnected".into()),
                }
            };
            let responses = responses.into_iter().map(|(index, result)| match result {
                Ok(value) => serde_json::json!({"index":index,"value":value}),
                Err(error) => serde_json::json!({"index":index,"error":error}),
            }).collect::<Vec<_>>();
            respond.call::<_, ()>((serde_json::to_string(&responses).map_err(|e| e.to_string())?,)).map_err(|e| js_error(&ctx, e))?;
        }
    })
}

fn js_error(ctx: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> String {
    if error.is_exception() {
        let exception = ctx.catch();
        if let Some(object) = exception.as_object() {
            if let Ok(value) = object.get::<_, String>("message") {
                return value;
            }
        }
    }
    error.to_string()
}

const PRELUDE: &str = r#"
(() => {
  const call = globalThis.__host_call;
  const emit = globalThis.__host_text;
  const catalog = JSON.parse(globalThis.__catalog);
  delete globalThis.__host_call;
  delete globalThis.__host_text;
  delete globalThis.__catalog;
  const pending = new Map();
  const tools = Object.create(null);
  const identifiers = new Set();
  for (const tool of catalog) {
    const identifier = tool.name.replace(/[^a-zA-Z0-9_$]/g, "_");
    if (identifiers.has(identifier)) throw new Error("ambiguous tool identifier: " + identifier);
    identifiers.add(identifier);
    const invoke = args => new Promise((resolve, reject) => {
      const index = call(tool.name, JSON.stringify(args));
      pending.set(index, { resolve, reject });
    });
    Object.defineProperty(tools, tool.name, { value: invoke });
    if (identifier !== tool.name) Object.defineProperty(tools, identifier, { value: invoke });
    tool.identifier = identifier;
    Object.freeze(tool);
  }
  const text = value => emit(typeof value === "string" ? value : (JSON.stringify(value) ?? String(value)));
  Object.defineProperty(globalThis, "tools", { value: Object.freeze(tools) });
  Object.defineProperty(globalThis, "ALL_TOOLS", { value: Object.freeze(catalog) });
  Object.defineProperty(globalThis, "text", { value: text });
  Object.defineProperty(globalThis, "console", { value: Object.freeze({
    log: (...values) => values.forEach(text),
    info: (...values) => values.forEach(text),
    warn: (...values) => values.forEach(text),
    error: (...values) => values.forEach(text)
  }) });
  return json => {
    for (const response of JSON.parse(json)) {
      const handlers = pending.get(response.index);
      pending.delete(response.index);
      if (response.error !== undefined) handlers.reject(new Error(response.error));
      else handlers.resolve(response.value);
    }
  };
})()
"#;
