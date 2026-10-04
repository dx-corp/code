use super::host::agent::{FromAgent, NativeAgent, RangeSelection};
use super::{
    hash,
    report::{Measurement, Trial},
    suite::{Case, grade},
};
use anyhow::{Context, Result, bail, ensure};
use std::{collections::HashSet, io::Write, path::Path, time::Duration};
use tokio::{
    sync::mpsc,
    time::{Instant, timeout_at},
};

pub async fn run(
    agent: &NativeAgent,
    events: &mut mpsc::UnboundedReceiver<FromAgent>,
    case: &Case,
    compacted: bool,
    budget: Duration,
    folder: &Path,
) -> Result<Trial> {
    let start = Instant::now();
    let deadline = start + budget;
    let mut trial = Trial {
        case_id: case.id.clone(),
        compacted,
        verified: false,
        terminal: false,
        failure: None,
        elapsed_seconds: 0.0,
        retries_started: 0,
        forced_compaction_applied: false,
        answer: String::new(),
        measurement: Measurement::default(),
    };
    std::fs::write(
        folder.join("original-history.json"),
        serde_json::to_vec_pretty(&case.history)?,
    )?;
    let mut log = std::fs::File::create(folder.join("events.jsonl"))?;
    let result = execute(agent, events, case, deadline, folder, &mut log, &mut trial).await;
    while let Ok(event) = events.try_recv() {
        record(&mut log, &event)?;
        trial.retries_started += usize::from(is_retry(&event));
    }
    trial.elapsed_seconds = start.elapsed().as_secs_f64();
    if let Err(error) = result {
        trial.failure = Some(
            if error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
            {
                "timeout"
            } else {
                "runtime_or_summary_failure"
            }
            .into(),
        );
        trial.measurement.complete = false;
        std::fs::write(folder.join("failure.txt"), format!("{error:#}"))?;
    }
    trial.verified =
        trial.terminal && trial.failure.is_none() && grade(&trial.answer, &case.expected);
    trial.measurement.complete &= trial.retries_started == 0;
    Ok(trial)
}

fn record(log: &mut std::fs::File, event: &FromAgent) -> Result<()> {
    if matches!(event, FromAgent::LocalAssistantContent { .. }) {
        // This native-only snapshot is intentionally excluded from the wire protocol.
        serde_json::to_writer(
            &mut *log,
            &serde_json::json!({
                "type": "local_assistant_content", "wire_serializable": false
            }),
        )?;
    } else {
        serde_json::to_writer(&mut *log, event)?;
    }
    writeln!(log)?;
    Ok(())
}

fn is_retry(event: &FromAgent) -> bool {
    matches!(
        event,
        FromAgent::RequestRetryObservation
            | FromAgent::StreamObservation {
                observation: maestro_ai::StreamObservation::Retry
            }
    )
}

async fn execute(
    agent: &NativeAgent,
    events: &mut mpsc::UnboundedReceiver<FromAgent>,
    case: &Case,
    deadline: Instant,
    folder: &Path,
    log: &mut std::fs::File,
    trial: &mut Trial,
) -> Result<()> {
    agent.replace_history(case.history.clone());
    let preview = timeout_at(deadline, agent.start_selective_summary_preview()?).await???;
    if trial.compacted {
        let request = agent
            .start_selective_summary(RangeSelection::FromTurn(1), preview.history_digest.clone())?;
        let mut receiver = request.receiver;
        let outcome = match timeout_at(deadline, &mut receiver).await {
            Ok(outcome) => outcome?,
            Err(error) => {
                request.cancellation.cancel();
                // Settle auxiliary usage without calling the provider again.
                if let Ok(Ok(outcome)) =
                    tokio::time::timeout(Duration::from_secs(2), receiver).await
                {
                    trial.measurement.add(outcome.usage.as_ref());
                }
                return Err(error.into());
            }
        };
        trial.measurement.add(outcome.usage.as_ref());
        let summary = outcome.result?;
        let raw = serde_json::to_vec_pretty(&summary.messages)?;
        std::fs::write(folder.join("compacted-history.json"), &raw)?;
        std::fs::write(folder.join("compacted-history.sha256"), hash(&raw))?;
        timeout_at(
            deadline,
            agent.apply_selective_summary(summary.messages, preview.history_digest)?,
        )
        .await???;
        trial.forced_compaction_applied = true;
    }
    timeout_at(deadline, agent.prompt(case.question.clone(), vec![])).await??;
    let mut response_ids = HashSet::new();
    loop {
        let event = timeout_at(deadline, events.recv())
            .await?
            .context("native event stream closed")?;
        record(log, &event)?;
        trial.retries_started += usize::from(is_retry(&event));
        match event {
            FromAgent::ResponseStart { .. } => trial.answer.clear(),
            FromAgent::ResponseChunk {
                content,
                is_thinking: false,
                ..
            } => {
                ensure!(
                    trial.answer.len() + content.len() <= 64 * 1024,
                    "answer exceeded evaluation limit"
                );
                trial.answer.push_str(&content);
            }
            FromAgent::ResponseEnd { response_id, usage }
                if response_id == "done" && usage.is_none() =>
            {
                // The actor's completion marker is not a billed provider response.
            }
            FromAgent::ResponseEnd { response_id, usage } => {
                ensure!(
                    response_ids.insert(response_id),
                    "duplicate response usage snapshot"
                );
                trial.measurement.add(usage.as_ref());
            }
            FromAgent::Compaction { auto: true, .. } => {
                bail!("unexpected automatic compaction invalidates the control")
            }
            FromAgent::ToolCall { .. } => bail!("tool call in a tool-free evaluation"),
            FromAgent::ProviderError { .. }
            | FromAgent::Error { .. }
            | FromAgent::TurnInterrupted { .. } => bail!("native turn failed"),
            FromAgent::TurnCompleted { .. } => {
                ensure!(
                    !response_ids.is_empty(),
                    "terminal without a provider response receipt"
                );
                trial.terminal = true;
                // Failed retries may have unreported spend. Never call partial cost complete.
                trial.measurement.complete = trial.retries_started == 0;
                return Ok(());
            }
            _ => {}
        }
    }
}
