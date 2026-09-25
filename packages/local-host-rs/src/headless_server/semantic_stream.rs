//! Headless publication of the shared incremental assistant text policy.

use super::*;

impl RuntimeMeta {
    fn semantic_now_ms(&self) -> u64 {
        self.semantic_started_at
            .map_or(0, |start| start.elapsed().as_millis() as u64)
    }

    pub(super) fn record_response_chunk(
        &mut self,
        content: &str,
        is_thinking: bool,
    ) -> (crate::transcript::TranscriptGrade, Vec<SemanticRelease>) {
        let grade = self.transcript_grade;
        if grade != crate::transcript::TranscriptGrade::Delta {
            self.response_chunks
                .push((content.to_string(), is_thinking));
        }
        let now_ms = self.semantic_now_ms();
        if !is_thinking && !content.trim().is_empty() {
            self.semantic_raw_first_ms.get_or_insert(now_ms);
        }
        if is_thinking || grade != crate::transcript::TranscriptGrade::Delta {
            return (grade, Vec::new());
        }
        let releases = self.semantic_text.push(content, now_ms);
        self.semantic_peak_held_bytes = self.semantic_text.peak_held_bytes();
        self.record_semantic_releases(&releases, now_ms);
        (grade, releases)
    }

    fn flush_semantic_text(&mut self, reason: SemanticFlushReason) -> Vec<SemanticRelease> {
        let now_ms = self.semantic_now_ms();
        let releases = self.semantic_text.flush(reason, now_ms);
        self.record_semantic_releases(&releases, now_ms);
        releases
    }

    fn record_semantic_releases(&mut self, releases: &[SemanticRelease], now_ms: u64) {
        for release in releases {
            if !release.text.trim().is_empty() {
                self.semantic_first_released_ms.get_or_insert(now_ms);
            }
            self.semantic_published_chunks += 1;
            self.semantic_standalone_releases += u64::from(release.standalone_title);
            self.semantic_forced_flushes += u64::from(release.forced.is_some());
            self.semantic_total_hold_ms += release.held_ms;
            tracing::debug!(
                target: "maestro.semantic_text",
                event = "semantic_assistant_text_released",
                response_id = self.semantic_response_id.as_deref().unwrap_or(""),
                held_ms = release.held_ms,
                bytes = release.text.len(),
                forced_reason = ?release.forced,
                standalone_title = release.standalone_title,
            );
        }
    }
}

fn emit_releases(response_id: Option<String>, releases: Vec<SemanticRelease>) -> Result<()> {
    if let Some(response_id) = response_id {
        for release in releases {
            emit(&FromAgentMessage::ResponseChunk {
                response_id: response_id.clone(),
                content: release.text,
                is_thinking: false,
            })?;
        }
    }
    Ok(())
}

pub(super) fn flush_and_emit(
    meta: &Arc<Mutex<RuntimeMeta>>,
    reason: SemanticFlushReason,
) -> Result<()> {
    let (response_id, releases) = {
        let mut runtime = meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            runtime.semantic_response_id.clone(),
            runtime.flush_semantic_text(reason),
        )
    };
    emit_releases(response_id, releases)
}

pub(super) fn tick_and_emit(meta: &Arc<Mutex<RuntimeMeta>>) -> Result<()> {
    let (response_id, releases) = {
        let mut runtime = meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now_ms = runtime.semantic_now_ms();
        let releases = runtime.semantic_text.tick(now_ms);
        runtime.record_semantic_releases(&releases, now_ms);
        (runtime.semantic_response_id.clone(), releases)
    };
    emit_releases(response_id, releases)
}

pub(super) fn boundary_reason(msg: &FromAgent) -> Option<SemanticFlushReason> {
    match msg {
        FromAgent::ToolCall { .. } | FromAgent::ToolStart { .. } | FromAgent::ToolEnd { .. } => {
            Some(SemanticFlushReason::Tool)
        }
        FromAgent::ResponseStart { .. } => Some(SemanticFlushReason::Finalization),
        FromAgent::TurnCompleted { .. } => Some(SemanticFlushReason::TurnEnd),
        FromAgent::TurnInterrupted { .. } => Some(SemanticFlushReason::Cancellation),
        FromAgent::Error { .. } | FromAgent::ProviderError { .. } => {
            Some(SemanticFlushReason::Error)
        }
        _ => None,
    }
}

pub(super) fn reset_response(meta: &mut RuntimeMeta, response_id: &str) {
    meta.semantic_text = SemanticTextRelease::default();
    meta.semantic_response_id = Some(response_id.to_owned());
    meta.semantic_started_at = Some(Instant::now());
    meta.semantic_raw_first_ms = None;
    meta.semantic_first_released_ms = None;
    meta.semantic_published_chunks = 0;
    meta.semantic_standalone_releases = 0;
    meta.semantic_forced_flushes = 0;
    meta.semantic_peak_held_bytes = 0;
    meta.semantic_total_hold_ms = 0;
}

pub(super) fn publish_response_chunk(
    meta: &Arc<Mutex<RuntimeMeta>>,
    response_id: String,
    content: String,
    is_thinking: bool,
) -> Result<()> {
    let (grade, first_raw_text, releases) = {
        let mut runtime = meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let first_raw_text =
            !is_thinking && !content.trim().is_empty() && runtime.semantic_raw_first_ms.is_none();
        let (grade, releases) = runtime.record_response_chunk(&content, is_thinking);
        (grade, first_raw_text, releases)
    };
    if first_raw_text {
        emit(&FromAgentMessage::AssistantTextObserved {
            response_id: response_id.clone(),
        })?;
    }
    if grade == crate::transcript::TranscriptGrade::Delta {
        if is_thinking {
            emit(&FromAgentMessage::ResponseChunk {
                response_id,
                content,
                is_thinking,
            })?;
        } else {
            emit_releases(Some(response_id), releases)?;
        }
    }
    Ok(())
}

pub(super) fn log_response(meta: &Arc<Mutex<RuntimeMeta>>, response_id: &str) {
    let runtime = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tracing::info!(
        target: "maestro.semantic_text",
        event = "semantic_assistant_text_response",
        response_id,
        transport_first_text_ms = ?runtime.semantic_raw_first_ms,
        first_released_content_ms = ?runtime.semantic_first_released_ms,
        standalone_title_releases = runtime.semantic_standalone_releases,
        titles_seen = runtime.semantic_text.titles_seen(),
        standalone_title_release_percent = if runtime.semantic_text.titles_seen() == 0 {
            0.0
        } else {
            100.0 * runtime.semantic_standalone_releases as f64
                / runtime.semantic_text.titles_seen() as f64
        },
        published_text_chunks = runtime.semantic_published_chunks,
        forced_flushes = runtime.semantic_forced_flushes,
        peak_bytes_held = runtime.semantic_peak_held_bytes,
        total_hold_ms = runtime.semantic_total_hold_ms,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_delta_bridge_publishes_title_with_following_text() {
        let mut meta = RuntimeMeta {
            transcript_grade: crate::transcript::TranscriptGrade::Delta,
            semantic_started_at: Some(Instant::now()),
            ..RuntimeMeta::default()
        };
        let (_, title) = meta.record_response_chunk("## Risk\n", false);
        assert!(title.is_empty());
        assert_eq!(meta.semantic_text.titles_seen(), 1);
        assert!(meta.semantic_raw_first_ms.is_some());
        assert!(meta.semantic_first_released_ms.is_none());
        let (_, paragraph) = meta.record_response_chunk("The risk is delay.\n", false);
        assert_eq!(paragraph.len(), 1);
        assert_eq!(paragraph[0].text, "## Risk\nThe risk is delay.\n");
        assert!(meta.semantic_first_released_ms.is_some());
        assert_eq!(meta.semantic_published_chunks, 1);
        assert_eq!(meta.semantic_standalone_releases, 0);
    }
    #[test]
    fn delta_transcript_does_not_buffer_emitted_response_chunks() {
        let mut meta = RuntimeMeta {
            transcript_grade: crate::transcript::TranscriptGrade::Delta,
            ..RuntimeMeta::default()
        };

        for _ in 0..10_000 {
            assert_eq!(
                meta.record_response_chunk("already emitted", false).0,
                crate::transcript::TranscriptGrade::Delta,
            );
        }

        assert!(meta.response_chunks.is_empty());
    }
}
