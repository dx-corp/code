//! Script orchestration, never execution authority. Hosts admit, journal and
//! execute every request, and commit scratch state only with known success.
use rquickjs::{Context, Function, Promise, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex, mpsc as sync_mpsc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod discovery;
mod helpers;
mod models;
mod output;
mod prelude;
mod store;
#[cfg(test)]
mod tests;
mod vm;

pub use discovery::{declaration_description, describe_tool, namespace, render_type};
pub use models::{
    ModelAlias, ModelBinding, ModelCall, ModelOperation, ModelSelector, admitted_models,
    resolve_model_call,
};
pub use output::{OutputBlock, image_block};
pub use store::{Store, StoreWrites, validate_store};

pub const TOOL_NAME: &str = "codemode";
pub const DESCRIPTION: &str = "Compose tools in a sandboxed JavaScript async function. Use tools.<name>(args), Promise.allSettled for independent reads, and text(value), image(imageBlock) or return for selected output. searchTools(query), describeTool(name), describeNamespace(name) and ALL_TOOLS inspect only the admitted catalog. store(key,value)/load(key) keep bounded untrusted JSON scratch state after known successful execution. models.getAvailable(), models.classify(selector,args), models.generateImages(selector,args) use only admitted ordinary model tools and their unchanged schemas. Nested calls retain policy and receipts; conversational confirmations use direct calls. No filesystem, network, process, modules or timers. Maximum 64 calls, 64 KiB text output; hard deadline 60 seconds. Effects already executed are not undone; reconcile unknown outcomes before retrying.";

pub fn schema() -> Value {
    serde_json::json!({"type":"object", "properties": {
        "code":{"type":"string","minLength":1,"maxLength":65536}
    },"required":["code"],"additionalProperties":false})
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub model_operation: Option<ModelOperation>,
    #[serde(default)]
    pub model_binding: Option<ModelBinding>,
}

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

/// Diagnostics are non-authoritative; durable owner receipts establish effects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Proposed,
    Running,
    Ok,
    Error,
    Interrupted,
    NotExecuted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSummary {
    pub index: usize,
    pub name: String,
    pub status: CallStatus,
}

#[derive(Debug)]
pub struct Report {
    /// Text-only compatibility projection. Image bytes never appear here.
    pub output: Vec<String>,
    pub blocks: Vec<OutputBlock>,
    pub error: Option<String>,
    pub store_writes: StoreWrites,
    pub calls: Vec<CallSummary>,
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
            if !self.calls.is_empty() {
                output.push_str("\nNested calls (completed effects are not undone): ");
                output.push_str(
                    &self
                        .calls
                        .iter()
                        .map(|call| format!("{} ({:?})", call.name, call.status))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
        }
        output
    }
}

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
        Self::start_with_store(code, tools, cancel, deadline, Store::new())
    }
    pub fn start_with_store(
        code: String,
        tools: Vec<Tool>,
        cancel: &CancellationToken,
        deadline: Duration,
        store: Store,
    ) -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let stop = cancel.child_token();
        let worker_stop = stop.clone();
        std::thread::spawn(move || {
            let report = vm::run(
                code,
                tools,
                &events_tx,
                &worker_stop,
                deadline.min(Duration::from_secs(60)),
                store,
            );
            let _ = events_tx.send(Event::Done(report));
        });
        Self { events, stop }
    }
    pub async fn next(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}
