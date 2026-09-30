//! A read awaiting its own log write must keep progressing during peer results.
#[allow(dead_code)]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use dex_loop::{
    Budget, CancellationToken, Context, Cursor, Engine, Event, Exit, Fenced, Lexicon, Log,
    PrincipalId, ProposedCall, ThreadId, ToolName, ToolResult, ToolSpec, Tools, Verdict,
};
use serde_json::json;
use support::*;
use tokio::sync::{Mutex, Notify};

#[derive(Default)]
struct Contention {
    row: Mutex<()>,
    step_committed: Notify,
    read_holds_row: Notify,
    finish_attempted: Notify,
    runs: AtomicUsize,
}

struct ContendedLog {
    inner: FakeLog,
    contention: Arc<Contention>,
}

impl Log for ContendedLog {
    async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
        if events.iter().any(
            |event| matches!(event, Event::ToolFinished { call, .. } if call.as_str() == "t1-1-0"),
        ) {
            self.contention.finish_attempted.notify_one();
        }
        let row = self.contention.row.lock().await;
        let result = self.inner.append(events).await;
        drop(row);
        if result.is_ok()
            && events
                .iter()
                .any(|event| matches!(event, Event::ModelStepCompleted { step: 1, .. }))
        {
            self.contention.step_committed.notify_one();
        }
        result
    }

    async fn append_text(&self, text: String) -> Result<(), Fenced> {
        self.inner.append_text(text).await
    }

    async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
        self.inner.control_since(after).await
    }
}

struct ContendedReads {
    specs: Vec<ToolSpec>,
    contention: Arc<Contention>,
}

impl Tools for ContendedReads {
    fn catalog(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn search(&self, _principal: &PrincipalId, _query: &str) -> Vec<ToolName> {
        Vec::new()
    }

    async fn policy(&self, _ctx: &Context, _call: &ProposedCall) -> Verdict {
        Verdict::Allow
    }

    async fn run(
        &self,
        _thread: &ThreadId,
        call: &ProposedCall,
        _cancel: &CancellationToken,
    ) -> ToolResult {
        self.contention.runs.fetch_add(1, Ordering::SeqCst);
        if call.args["key"] == "a" {
            self.contention.read_holds_row.notified().await;
        } else {
            self.contention.step_committed.notified().await;
            let _row = self.contention.row.lock().await;
            self.contention.read_holds_row.notify_one();
            // Model a tool suspended while holding the durable log's row lock.
            // Only polling this read again can release it for the peer result.
            self.contention.finish_attempted.notified().await;
        }
        output_for(&call.id)
    }
}

#[tokio::test]
async fn peer_read_progresses_while_engine_records_a_finished_read() {
    run_contended_wave(false).await;
}

#[tokio::test]
async fn prefetched_peer_progresses_after_transfer_into_committed_wave() {
    run_contended_wave(true).await;
}

async fn run_contended_wave(prefetched: bool) {
    let log = FakeLog::default();
    log.start_turn("t1", "look");
    if !prefetched {
        log.host_append(Event::StepStarted {
            step: 1,
            control_through: Cursor::START,
        });
        let calls = ["a", "b"]
            .into_iter()
            .enumerate()
            .map(|(index, key)| {
                ProposedCall::new(
                    call_id("t1", 1, index),
                    ToolName::new("search"),
                    json!({"key": key}),
                    alice(),
                )
            })
            .collect();
        log.host_append(Event::ModelStepCompleted {
            step: 1,
            text: String::new(),
            calls,
            reasoning: None,
            served: None,
            timing: None,
        });
    }
    let contention = Arc::new(Contention::default());
    let model = if prefetched {
        FakeModel::new(vec![
            vec![
                call("search", json!({"key": "a"})),
                call("search", json!({"key": "b"})),
            ],
            vec![text("done")],
        ])
    } else {
        contention.step_committed.notify_one();
        FakeModel::new(vec![vec![text("done")]])
    };
    let engine = Engine::new(
        ContendedLog {
            inner: log.clone(),
            contention: contention.clone(),
        },
        model.clone(),
        ContendedReads {
            specs: vec![read_tool("search")],
            contention: contention.clone(),
        },
        FakeEffects::default(),
        Lexicon::default(),
        Budget {
            max_steps: 10,
            max_tokens: 1_000_000,
            max_cost_micros: 1_000_000,
            wall: Duration::from_secs(30),
        },
    );
    let mut ctx = log.rehydrate();
    let cancel = CancellationToken::new();
    // A deadlock bound, not a timing benchmark: both dependencies are explicit.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), engine.run(&mut ctx, &cancel))
            .await
            .expect("recording a peer result must keep polling the read holding its log lock"),
        Ok(Exit::Done)
    );
    assert_eq!(contention.runs.load(Ordering::SeqCst), 2);
    let events = log.events();
    let committed = events
        .iter()
        .position(|event| matches!(event, Event::ModelStepCompleted { step: 1, .. }))
        .unwrap();
    for index in 0..2 {
        let started = events
            .iter()
            .position(|event| matches!(event, Event::ToolStarted { call, .. } if call == &call_id("t1", 1, index)))
            .unwrap();
        assert_eq!(started < committed, prefetched);
    }
    for index in 0..2 {
        assert_eq!(
            log.events()
                .iter()
                .filter(|event| matches!(event, Event::ToolFinished { call, .. } if call == &call_id("t1", 1, index)))
                .count(),
            1
        );
    }
    assert_eq!(
        view(&model.seen()[usize::from(prefetched)]),
        strings(&[
            "user:look",
            "assistant::[t1-1-0,t1-1-1]",
            "tool:t1-1-0:ok:out/t1-1-0",
            "tool:t1-1-1:ok:out/t1-1-1",
        ])
    );
    assert_eq!(log.rehydrate(), ctx);
}
