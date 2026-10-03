//! Prompt hooks and attachment preparation.

use super::*;

impl NativeAgentRunner {
    pub(super) fn resolve_attachment_path(&self, raw: &str) -> PathBuf {
        if raw == "~" {
            if let Some(home) = dirs::home_dir() {
                return home;
            }
        }

        if let Some(stripped) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            if let Some(home) = dirs::home_dir() {
                return home.join(stripped);
            }
        }

        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            Path::new(&self.config.cwd).join(p)
        }
    }
    pub(super) fn detect_image_mime(path: &Path) -> Option<&'static str> {
        let ext = path.extension().and_then(|e| e.to_str())?.to_lowercase();
        match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            "bmp" => Some("image/bmp"),
            "svg" => Some("image/svg+xml"),
            _ => None,
        }
    }
    pub(super) fn truncate_text(text: &str, max_chars: usize) -> String {
        if text.chars().count() <= max_chars {
            return text.to_string();
        }
        text.chars().take(max_chars).collect()
    }
    pub(super) fn apply_message_hook_modification(
        prompt: &mut String,
        attachments: &mut Vec<String>,
        new_input: serde_json::Value,
    ) {
        match new_input {
            serde_json::Value::String(text) => {
                *prompt = text;
            }
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(text)) =
                    map.get("message").or_else(|| map.get("prompt"))
                {
                    *prompt = text.clone();
                }
                if let Some(serde_json::Value::Array(items)) = map.get("attachments") {
                    let mut next = Vec::new();
                    for item in items {
                        match item {
                            serde_json::Value::String(value) => next.push(value.clone()),
                            other => next.push(other.to_string()),
                        }
                    }
                    *attachments = next;
                }
            }
            _ => {}
        }
    }
    pub(super) fn merge_prompt_context(target: &mut Option<String>, context: String) {
        if context.trim().is_empty() {
            return;
        }
        match target {
            Some(existing) => {
                existing.push('\n');
                existing.push_str(&context);
            }
            None => {
                *target = Some(context);
            }
        }
    }
    pub(super) async fn prepare_pending_message(
        &mut self,
        pending: &PendingMessage,
    ) -> Result<Option<(Message, Option<String>)>> {
        // The skills this specific prompt's text triggered take effect here,
        // which is the first point that belongs to its own turn. Applying them
        // at enqueue time would have changed the turn that was still running,
        // and sharing one staged value across the queue let a prompt inherit
        // skills only a later prompt triggered. A staged prompt overtaken by an
        // authoritative
        // `SetSystemPrompt` is dropped: that update is newer and, because skill
        // activation is cumulative, already contains these skills.
        if apply_staged_system_prompt(
            &mut self.queued_system_prompts,
            pending.id,
            self.system_prompt_revision,
            &mut self.config.system_prompt,
            &mut self.runtime_prompt_revision,
        ) {
            self.refresh_runtime_audit();
        }

        let mut prompt = pending.content.clone();
        let mut attachments = pending.attachments.clone();
        let mut prompt_context: Option<String> = None;

        let hook_result = self
            .hooks
            .hook_user_prompt_submit(&prompt, attachments.len() as u32)
            .await;
        match hook_result {
            NativeHookResult::Block { reason } => {
                let _ = self.event_tx.send(FromAgent::Error {
                    message: format!("Prompt blocked by hook: {reason}"),
                    fatal: false,
                    terminal: false,
                    retryable: false,
                });
                return Ok(None);
            }
            NativeHookResult::ModifyInput { new_input } => {
                Self::apply_message_hook_modification(&mut prompt, &mut attachments, new_input);
            }
            NativeHookResult::InjectContext { context } => {
                Self::merge_prompt_context(&mut prompt_context, context);
            }
            NativeHookResult::Continue => {}
        }

        let hook_result = self
            .hooks
            .hook_pre_message(&prompt, &attachments, Some(&self.config.model))
            .await;
        match hook_result {
            NativeHookResult::Block { reason } => {
                let _ = self.event_tx.send(FromAgent::Error {
                    message: format!("Prompt blocked by hook: {reason}"),
                    fatal: false,
                    terminal: false,
                    retryable: false,
                });
                return Ok(None);
            }
            NativeHookResult::ModifyInput { new_input } => {
                Self::apply_message_hook_modification(&mut prompt, &mut attachments, new_input);
            }
            NativeHookResult::InjectContext { context } => {
                Self::merge_prompt_context(&mut prompt_context, context);
            }
            NativeHookResult::Continue => {}
        }

        let mut blocks = vec![ContentBlock::Text { text: prompt }];
        let attachment_blocks = self.load_attachment_blocks(&attachments).await;
        blocks.extend(attachment_blocks);

        let content = if blocks.len() == 1 {
            match &blocks[0] {
                ContentBlock::Text { text } => MessageContent::text(text.clone()),
                _ => MessageContent::Blocks(blocks),
            }
        } else {
            MessageContent::Blocks(blocks)
        };

        Ok(Some((
            Message {
                role: Role::User,
                content,
            },
            prompt_context,
        )))
    }
    pub(super) async fn append_pending_messages_for_turn(
        &mut self,
        pending: Vec<PendingMessage>,
    ) -> Result<bool> {
        let managed_request_lineage = pending
            .first()
            .and_then(|message| message.managed_request_lineage.clone());
        let managed_inference_authorization = pending
            .first()
            .and_then(|message| message.managed_inference_authorization.clone());
        let mut next_prompt_context: Option<String> = None;
        let mut appended = false;
        for pending_message in pending {
            if let Some((message, prompt_context)) =
                self.prepare_pending_message(&pending_message).await?
            {
                self.messages_mut().push(message);
                if pending_message.id != 0 {
                    self.processed_prompt_queue_ids.insert(pending_message.id);
                }
                if let Some(context) = prompt_context {
                    Self::merge_prompt_context(&mut next_prompt_context, context);
                }
                appended = true;
            }
        }
        self.prompt_context = next_prompt_context;
        if appended {
            if let Some(client) = self.client.as_mut() {
                client.set_managed_request_lineage(managed_request_lineage);
                client.set_managed_inference_authorization(
                    managed_inference_authorization.map(ManagedInferenceAuthorization::into_inner),
                );
            }
        }
        Ok(appended)
    }
    pub(super) async fn load_attachment_blocks(&self, raw_paths: &[String]) -> Vec<ContentBlock> {
        if raw_paths.is_empty() {
            return Vec::new();
        }

        let mut blocks = Vec::new();

        for raw in raw_paths {
            match self.tool_executor.file_read_verdict(raw) {
                NativeFirewallVerdict::Block { reason } => {
                    let _ = self.event_tx.send(FromAgent::Error {
                        message: format!("Attachment blocked: {reason}"),
                        fatal: false,
                        terminal: false,
                        retryable: false,
                    });
                    continue;
                }
                NativeFirewallVerdict::RequireApproval { reason } => {
                    let _ = self.event_tx.send(FromAgent::Status {
                        message: format!("Attachment is sensitive: {reason} (attaching anyway)"),
                    });
                }
                NativeFirewallVerdict::Allow => {}
            }

            let path = self.resolve_attachment_path(raw);

            let meta = match fs::metadata(&path).await {
                Ok(m) => m,
                Err(e) => {
                    let _ = self.event_tx.send(FromAgent::Error {
                        message: format!("Failed to read attachment metadata for {raw}: {e}"),
                        fatal: false,
                        terminal: false,
                        retryable: false,
                    });
                    continue;
                }
            };

            if !meta.is_file() {
                let _ = self.event_tx.send(FromAgent::Error {
                    message: format!("Attachment is not a file: {raw}"),
                    fatal: false,
                    terminal: false,
                    retryable: false,
                });
                continue;
            }

            let video_info = self.tool_executor.video_mime(&path);
            let video_mime = video_info.as_ref().map(|(mime, _)| mime.as_str());
            let attachment_limit = video_info
                .as_ref()
                .map_or(Self::MAX_ATTACHMENT_BYTES, |(_, limit)| *limit);
            if meta.len() > attachment_limit {
                let size_mb = meta.len().div_ceil(1024 * 1024);
                let _ = self.event_tx.send(FromAgent::Error {
                    message: format!("Attachment too large ({size_mb}MB): {raw}"),
                    fatal: false,
                    terminal: false,
                    retryable: false,
                });
                continue;
            }

            if let Some(mime) = video_mime {
                match self.tool_executor.extract_video_frames(&path).await {
                    Ok(frames) => {
                        blocks.push(ContentBlock::Text {
                            text: format!(
                                "\n\n[Video: {} ({mime}); {} sampled frames follow]",
                                path.file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or(raw),
                                frames.len()
                            ),
                        });
                        blocks.extend(frames.into_iter().map(|data| ContentBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: "image/jpeg".to_string(),
                                data,
                                owner: None,
                            },
                        }));
                    }
                    Err(error) => {
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: format!("Failed to process video attachment {raw}: {error}"),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                    }
                }
                continue;
            }

            if let Some(mime) = Self::detect_image_mime(&path) {
                match fs::read(&path).await {
                    Ok(bytes) => {
                        let data = STANDARD.encode(&bytes);
                        blocks.push(ContentBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: mime.to_string(),
                                data,
                                owner: None,
                            },
                        });
                    }
                    Err(e) => {
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: format!("Failed to read image attachment {raw}: {e}"),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                    }
                }
                continue;
            }

            match fs::read_to_string(&path).await {
                Ok(text) => {
                    let truncated = Self::truncate_text(&text, Self::MAX_TEXT_ATTACHMENT_CHARS);
                    let file_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(raw.as_str());
                    blocks.push(ContentBlock::Text {
                        text: format!("\n\n[Document: {file_name}]\n{truncated}"),
                    });
                }
                Err(e) => {
                    let _ = self.event_tx.send(FromAgent::Error {
                        message: format!("Unsupported attachment (not image/utf8 text) {raw}: {e}"),
                        fatal: false,
                        terminal: false,
                        retryable: false,
                    });
                }
            }
        }

        blocks
    }
}
