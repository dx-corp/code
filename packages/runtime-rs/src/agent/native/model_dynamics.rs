//! Task-scoped intelligence changes at provider request boundaries.
use super::*;

#[derive(Debug, thiserror::Error)]
#[error("provider request failed: {0:#}")]
pub(super) struct ProviderRequestFailure(pub anyhow::Error);

impl NativeAgentRunner {
    pub(super) fn preserve_explicit_intelligence_choice(&mut self) {
        self.boost_original = None;
        let mut state = self.dynamics.lock().expect("model dynamics mutex");
        state.requested = false;
        state.status = super::super::model_dynamics::BoostStatus::Idle;
        let _ = self.event_tx.send(FromAgent::BoostChanged {
            status: state.status,
            thinking: None,
        });
    }

    pub(super) fn current_model_choice(&self) -> super::super::model_dynamics::ModelChoice {
        super::super::model_dynamics::ModelChoice {
            model: self.config.model.clone(),
            thinking: super::super::model_dynamics::thinking_level(
                self.config.thinking_enabled,
                self.config.thinking_budget,
            ),
        }
    }

    /// Change only at a request boundary; preserve the complete canonical history.
    pub(super) async fn apply_model_choice(
        &mut self,
        choice: &super::super::model_dynamics::ModelChoice,
    ) -> Result<()> {
        if self
            .client
            .as_ref()
            .is_some_and(UnifiedClient::is_managed_gateway)
        {
            anyhow::bail!("Hosted model choices require Platform authorization");
        }
        if let Some(reason) = self
            .tool_executor
            .model_allowed(&policy_model_id(&choice.model))
        {
            anyhow::bail!("{reason}");
        }
        if choice.model != self.config.model {
            self.tool_executor
                .validate_model_transition(&self.config.model, &choice.model)
                .map_err(anyhow::Error::msg)?;
            let current_policy_model = policy_model_id(&self.config.model);
            let target_policy_model = policy_model_id(&choice.model);
            let current_provider = current_policy_model
                .split_once('/')
                .map(|(provider, _)| provider);
            let target_provider = target_policy_model
                .split_once('/')
                .map(|(provider, _)| provider);
            let provider = if current_provider == target_provider {
                // Keep the already-authorized endpoint and connection profile
                // for same-provider model changes. Resolving a new client here
                // can silently select a different profile or base URL.
                self.client
                    .as_ref()
                    .context("Direct provider client unavailable")?
                    .provider_name()
                    .to_owned()
            } else {
                let resolved = self
                    .tool_executor
                    .resolve_model_for_automatic_transition(&choice.model)
                    .map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    resolved
                        .client
                        .as_ref()
                        .is_none_or(|client| !client.is_managed_gateway())
                        && !resolved.model_route.uses_app_server(),
                    "Automatic routing cannot cross inference authority"
                );
                let NativeResolvedClient {
                    client,
                    provider_name,
                    model_route,
                    ..
                } = resolved;
                self.client = client;
                self.model_route = model_route;
                provider_name
            };
            refresh_model_budgets_with_host(
                &self.tool_executor,
                &mut self.config,
                &mut self.compactor,
                &choice.model,
            );
            self.config.model.clone_from(&choice.model);
            self.model_tool_cache = None;
            self.hooks.hook_set_model(&choice.model).await;
            let _ = self.event_tx.send(FromAgent::ModelChanged {
                model: choice.model.clone(),
                provider,
            });
        }
        let thinking = self
            .tool_executor
            .normalize_thinking(&choice.model, choice.thinking);
        let (enabled, budget) = thinking.to_config();
        self.config.thinking_enabled = enabled;
        self.config.thinking_budget = budget;
        Ok(())
    }

    pub(super) async fn apply_requested_boost(&mut self) -> Result<()> {
        let preferences = self.config.model_dynamics.clone();
        let choice = self
            .tool_executor
            .boost_choice(&self.current_model_choice(), &preferences);
        let available = choice.as_ref().is_some_and(|choice| {
            self.tool_executor
                .model_allowed(&policy_model_id(&choice.model))
                .is_none()
        }) && !self
            .client
            .as_ref()
            .is_some_and(UnifiedClient::is_managed_gateway);
        self.dynamics
            .lock()
            .expect("model dynamics mutex")
            .available = available;
        let requested = {
            let mut state = self.dynamics.lock().expect("model dynamics mutex");
            let request = !state.used
                && (state.requested
                    || (preferences.auto_boost
                        && state.status == super::super::model_dynamics::BoostStatus::Suggested));
            state.requested = false;
            if request {
                state.used = true;
            }
            request
        };
        if !requested {
            return Ok(());
        }
        let original = self.current_model_choice();
        let result = match self
            .tool_executor
            .boost_choice(&original, &preferences)
            .context("No higher supported setting is configured")
        {
            Ok(choice) => match self.apply_model_choice(&choice).await {
                Ok(()) => {
                    self.boost_original = Some(original);
                    Ok(self.current_model_choice().thinking)
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let (status, thinking) = match result {
            Ok(_) if self.model_route.uses_app_server() => {
                (super::super::model_dynamics::BoostStatus::Pending, None)
            }
            Ok(thinking) => (
                super::super::model_dynamics::BoostStatus::Active,
                Some(thinking),
            ),
            Err(error) => {
                let _ = self.event_tx.send(FromAgent::Status {
                    message: format!("Boost unavailable: {error}"),
                });
                (super::super::model_dynamics::BoostStatus::Idle, None)
            }
        };
        self.dynamics.lock().expect("model dynamics mutex").status = status;
        let _ = self
            .event_tx
            .send(FromAgent::BoostChanged { status, thinking });
        Ok(())
    }

    pub(super) async fn validate_codex_boost(&mut self) {
        let Some(original) = self.boost_original.clone() else {
            return;
        };
        if self.dynamics.lock().expect("model dynamics mutex").status
            != super::super::model_dynamics::BoostStatus::Pending
        {
            return;
        }
        let Some(session) = self.codex_session.as_ref() else {
            return;
        };
        let result = session
            .is_reasoning_boost(
                original.thinking.to_config(),
                (self.config.thinking_enabled, self.config.thinking_budget),
            )
            .await;
        let (status, thinking) = if matches!(result, Ok(true)) {
            (
                super::super::model_dynamics::BoostStatus::Active,
                self.current_model_choice().thinking,
            )
        } else {
            let reason = result.err().map_or_else(
                || "The current setting already uses the highest supported effort".to_owned(),
                |error| error.to_string(),
            );
            let _ = self.event_tx.send(FromAgent::Status {
                message: format!("Boost unavailable: {reason}"),
            });
            // Codex boost keeps the same model; restore effort directly without changing authority.
            let (enabled, budget) = original.thinking.to_config();
            self.config.thinking_enabled = enabled;
            self.config.thinking_budget = budget;
            self.boost_original = None;
            (
                super::super::model_dynamics::BoostStatus::Idle,
                original.thinking,
            )
        };
        self.dynamics.lock().expect("model dynamics mutex").status = status;
        let _ = self.event_tx.send(FromAgent::BoostChanged {
            status,
            thinking: Some(thinking),
        });
    }

    pub(super) async fn finish_task_boost(&mut self, cancelled: bool) {
        let thinking = if let Some(original) = self.boost_original.clone() {
            if let Err(error) = self.apply_model_choice(&original).await {
                // Restoration must not change the inference authority to evade policy.
                let _ = self.event_tx.send(FromAgent::Status {
                    message: format!("Could not restore the previous model: {error}"),
                });
                Some(self.current_model_choice().thinking)
            } else {
                Some(original.thinking)
            }
        } else {
            None
        };
        self.boost_original = None;
        let status = {
            let mut state = self.dynamics.lock().expect("model dynamics mutex");
            let pending = !cancelled && state.requested && !state.used;
            *state = Default::default();
            if pending {
                state.requested = true;
                state.status = super::super::model_dynamics::BoostStatus::Pending;
            }
            state.status
        };
        let _ = self
            .event_tx
            .send(FromAgent::BoostChanged { status, thinking });
    }

    pub(super) async fn run_with_model_recovery(
        &mut self,
        step_budget: &mut TurnStepBudget,
    ) -> Result<()> {
        let preferences = self.config.model_dynamics.clone();
        self.dynamics
            .lock()
            .expect("model dynamics mutex")
            .fallback_models
            .insert(self.config.model.clone());
        let mut remaining = preferences.fallbacks.into_iter();
        loop {
            self.apply_requested_boost().await?;
            // Keep the large tool-loop future off the recovery wrapper's stack.
            let result = Box::pin(self.run_loop_inner(step_budget)).await;
            let eligible = result.as_ref().err().is_some_and(|error| {
                (error.is::<ProviderRequestFailure>() || error.is::<ProviderStreamFailure>())
                    && matches!(
                        super::super::retry::ErrorKind::classify(&format!("{error:#}")),
                        super::super::retry::ErrorKind::Transient
                            | super::super::retry::ErrorKind::RateLimited { .. }
                    )
            });
            if !eligible
                || !step_budget.can_continue()
                || self.model_route.uses_app_server()
                || self
                    .client
                    .as_ref()
                    .is_some_and(UnifiedClient::is_managed_gateway)
                || self
                    .cancel_token
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                || self.shutdown_token.is_cancelled()
            {
                return result;
            }
            let choice = {
                let mut state = self.dynamics.lock().expect("model dynamics mutex");
                state.fallback_models.insert(self.config.model.clone());
                if state.fallback_attempts >= 3 {
                    return result;
                }
                let choice =
                    remaining.find(|choice| state.fallback_models.insert(choice.model.clone()));
                if choice.is_some() {
                    state.fallback_attempts += 1;
                }
                choice
            };
            let Some(choice) = choice else {
                return result;
            };
            if let Err(error) = self.apply_model_choice(&choice).await {
                let _ = self.event_tx.send(FromAgent::Status {
                    message: format!("Model fallback unavailable: {error}"),
                });
                return result;
            }
            self.boost_original = None;
            self.dynamics.lock().expect("model dynamics mutex").status =
                super::super::model_dynamics::BoostStatus::Idle;
            let _ = self.event_tx.send(FromAgent::BoostChanged {
                status: super::super::model_dynamics::BoostStatus::Idle,
                thinking: Some(self.current_model_choice().thinking),
            });
            let _ = self.event_tx.send(FromAgent::Status {
                message: format!("Continuing with {}", choice.model),
            });
            // Completed tool calls remain in self.messages; only the failed generation is retried.
            self.repair_orphaned_tool_calls();
        }
    }
}
