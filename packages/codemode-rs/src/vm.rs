use super::*;
use crate::helpers::install_helpers;
use crate::prelude::PRELUDE;

struct VmState {
    output: Arc<Mutex<output::OutputSink>>,
    store: Arc<Mutex<Store>>,
    summaries: Arc<Mutex<Vec<CallSummary>>>,
}

pub(crate) fn run(
    code: String,
    tools: Vec<Tool>,
    events: &mpsc::UnboundedSender<Event>,
    stop: &CancellationToken,
    deadline: Duration,
    initial: Store,
) -> Report {
    let output = Arc::new(Mutex::new(output::OutputSink::default()));
    let current = Arc::new(Mutex::new(initial.clone()));
    let summaries = Arc::new(Mutex::new(Vec::new()));
    let result = validate_store(&initial)
        .and_then(|_| admitted_models(&tools).map(|_| ()))
        .and_then(|_| {
            execute(
                code,
                tools,
                events,
                stop,
                deadline,
                VmState {
                    output: output.clone(),
                    store: current.clone(),
                    summaries: summaries.clone(),
                },
            )
        });
    let blocks = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .blocks
        .clone();
    let output = blocks
        .iter()
        .filter_map(|block| match block {
            OutputBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let store_writes = if result.is_ok() {
        StoreWrites::between(
            &initial,
            &current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    } else {
        StoreWrites::default()
    };
    let mut calls = summaries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for call in &mut calls {
        call.status = match call.status {
            CallStatus::Proposed => CallStatus::NotExecuted,
            CallStatus::Running => CallStatus::Interrupted,
            ref status => status.clone(),
        };
    }
    Report {
        output,
        blocks,
        error: result.err(),
        store_writes,
        calls,
    }
}

fn execute(
    code: String,
    mut tools: Vec<Tool>,
    events: &mpsc::UnboundedSender<Event>,
    stop: &CancellationToken,
    deadline: Duration,
    state: VmState,
) -> Result<(), String> {
    let VmState {
        output,
        store,
        summaries,
    } = state;
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
    let overflow = Arc::new(Mutex::new(None::<String>));
    for tool in &mut tools {
        if let Some(instructions) = &mut tool.namespace_instructions {
            shorten(instructions, discovery::MAX_METADATA_BYTES, "...");
        }
    }
    context.with(|ctx| -> Result<(), String> {
        let pending_calls = pending.clone();
        let requested = summaries.clone();
        let count = std::cell::Cell::new(0usize);
        let bridge = Function::new(ctx.clone(), move |name: String, args: String| -> rquickjs::Result<usize> {
            if count.get() >= 64 { return Err(rquickjs::Error::new_from_js_message("tool call", "bounded script", "maximum 64 nested calls")); }
            let args = serde_json::from_str(&args).map_err(|e| rquickjs::Error::new_from_js_message("JSON", "tool arguments", e.to_string()))?;
            if serde_json::to_vec(&args).map_or(true, |v| v.len() > 1_048_576) { return Err(rquickjs::Error::new_from_js_message("arguments", "bounded script", "maximum 1 MiB tool arguments")); }
            let index = count.get();
            count.set(index + 1);
            requested.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(CallSummary { index, name: name.clone(), status: CallStatus::Proposed });
            pending_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Call { index, name, args });
            Ok(index)
        }).map_err(|e| e.to_string())?;
        let text_output = output.clone();
        let poisoned = overflow.clone();
        let emit = Function::new(ctx.clone(), move |text: String| -> rquickjs::Result<()> {
            let mut output = text_output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = output.text(text) {
                *poisoned.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
                return Err(rquickjs::Error::new_from_js_message("output", "bounded script", error));
            }
            Ok(())
        }).map_err(|e| e.to_string())?;
        install_helpers(&ctx, &tools, output.clone(), overflow.clone(), store)?;
        // The Rust helper retains bounded on-demand guidance. Do not charge
        // the VM heap for copies repeated in every ALL_TOOLS declaration.
        for tool in &mut tools { tool.namespace_instructions = None; }
        ctx.globals().set("__host_call", bridge).map_err(|e| e.to_string())?;
        ctx.globals().set("__host_text", emit).map_err(|e| e.to_string())?;
        ctx.globals().set("__catalog", serde_json::to_string(&tools.iter().map(|tool| serde_json::json!({"name":tool.name})).collect::<Vec<_>>()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let respond: Function = ctx.eval(PRELUDE).map_err(|e| js_error(&ctx, e))?;
        let source = format!("(async function(){{{code}\n}})().then(value => {{ if (value !== undefined) text(value); }})");
        let mut options = rquickjs::context::EvalOptions::default();
        options.filename = Some("codemode.js".into());
        let promise: Promise = ctx.eval_with_options(source, options).map_err(|e| js_error(&ctx, e))?;
        let (reply, responses) = sync_mpsc::channel::<Reply>();
        let mut inflight = std::collections::BTreeSet::new();
        loop {
            if stop.is_cancelled() { return Err("cancelled".into()); }
            if Instant::now() >= expires { return Err("deadline exceeded".into()); }
            if let Some(error) = overflow.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() { return Err(error); }
            while ctx.execute_pending_job() {
                if stop.is_cancelled() || Instant::now() >= expires { return Err("cancelled or deadline exceeded".into()); }
            }
            if let Some(error) = overflow.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() { return Err(error); }
            if let Some(result) = promise.result::<rquickjs::Value>() {
                return result.map(|_| ()).map_err(|e| js_error(&ctx, e));
            }
            let calls = std::mem::take(&mut *pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
            if !calls.is_empty() {
                for call in &calls {
                    inflight.insert(call.index);
                    if let Some(summary) = summaries.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(call.index) {
                        summary.status = CallStatus::Running;
                    }
                }
                events.send(Event::Calls { calls, reply: reply.clone() }).map_err(|_| "host disconnected")?;
            }
            if inflight.is_empty() { return Err("script awaits a promise with no pending tool call".into()); }
            let settled = loop {
                if stop.is_cancelled() { return Err("cancelled".into()); }
                if Instant::now() >= expires { return Err("deadline exceeded".into()); }
                if events.is_closed() { return Err("host disconnected".into()); }
                match responses.recv_timeout(Duration::from_millis(10)) {
                    Ok(responses) => break responses,
                    Err(sync_mpsc::RecvTimeoutError::Timeout) => {},
                    Err(sync_mpsc::RecvTimeoutError::Disconnected) => return Err("host disconnected".into()),
                }
            };
            // Settle only live calls. Old-wave senders remain valid, but neither
            // duplicate nor unrequested indices may change diagnostics or JS.
            let settled = settled.into_iter().filter_map(|(index, result)| {
                if !inflight.remove(&index) { return None; }
                if let Some(summary)=summaries.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(index) {
                    summary.status=if result.is_ok() {CallStatus::Ok} else {CallStatus::Error};
                }
                Some(match result {
                    Ok(value) => serde_json::json!({"index":index,"value":value}),
                    Err(error) => serde_json::json!({"index":index,"error":error}),
                })
            }).collect::<Vec<_>>();
            respond.call::<_, ()>((serde_json::to_string(&settled).map_err(|e| e.to_string())?,)).map_err(|e| js_error(&ctx, e))?;
        }
    })
}

fn js_error(ctx: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> String {
    if error.is_exception() {
        let exception = ctx.catch();
        if let Some(object) = exception.as_object() {
            let name = object
                .get::<_, String>("name")
                .unwrap_or_else(|_| "Error".into());
            let message = object.get::<_, String>("message").unwrap_or_default();
            let stack = object.get::<_, String>("stack").unwrap_or_default();
            return format!("{name}: {message}\n{stack}").trim().to_owned();
        }
    }
    error.to_string()
}
