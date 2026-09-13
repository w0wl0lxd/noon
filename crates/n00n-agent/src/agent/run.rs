use std::sync::{Arc, RwLock};

use serde_json::Value;
use tracing::{error, info, warn};

use n00n_config::canonical_tool_name;
use n00n_redact::demoted;

use n00n_providers::provider::Provider;
use n00n_providers::{
    ContentBlock, HistoryReplayReason, HostedToolSearch, Message, Model, OpenAiOptions,
    RequestDeliveryMetadata, RequestDeliveryPhase, RequestOptions, Role, StopReason,
    StreamResponse, System, TokenUsage,
};

use super::compaction::{self, CONTINUE_AFTER_COMPACT};
use super::compaction_hooks::CompactionTrigger;
use super::history::{History, sanitize_cancelled_history};
use super::instructions::LoadedInstructions;
use super::streaming::stream_with_retry;
use super::tool_dispatch::{self, RecentCalls};
use crate::cancel::{CancelMap, CancelToken, PreDispatchGate};
use crate::mcp::McpSession;
use crate::permissions::{PermissionAnswer, PermissionManager};
use crate::tools::{
    ActiveTools, Deadline, FileReadTracker, LocalTools, SessionIdentity, ToolAudience, ToolContext,
    ToolFilter, ToolRegistry,
};
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, EventSender, ExtractedCommand,
    FusionFailure, FusionLane, FusionPhase, FusionRoute, FusionState, InterruptPoint,
    InterruptSource, ToolDoneEvent, TurnCompleteEvent,
};
use n00n_config::{ToolKey, ToolOutputLines};
#[cfg(test)]
use n00n_storage::id::SessionRef;

use crate::tokenize::{
    count_json_with_tokenizer, count_tokens_with_tokenizer, tokenizer_for_model,
};

const MAX_REAUTH_ATTEMPTS: u32 = 2;
const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";
const THINKING_NUDGE_PROMPT: &str = "You provided reasoning but no final response. Please summarize your reasoning into a concise answer for the user.";
const MAX_TOKENS_CONTINUE_PROMPT: &str = "Continue exactly where you stopped.";
const IMAGE_TOKEN_ESTIMATE: usize = 2_048;
const HISTORY_REPLAY_PERMISSION_ID: &str = "history-replay";
const HISTORY_REPLAY_TOOL: &str = "history_replay";
const AMBIGUOUS_REPLAY_PERMISSION_ID: &str = "ambiguous-request-replay";
const AMBIGUOUS_REPLAY_TOOL: &str = "ambiguous_request_replay";
const AMBIGUOUS_REPLAY_RESET_MESSAGE: &str = "Resetting partial output before approved replay";
const HISTORY_REPLAY_CHANNEL_CLOSED_MESSAGE: &str = "History replay approval channel closed";
const AMBIGUOUS_REPLAY_CHANNEL_CLOSED_MESSAGE: &str = "Ambiguous replay approval channel closed";
const FUSION_REVIEW_PROMPT: &str = "Review the sidekick result above, verify it against the task, and produce the final lead response.";
const FUSION_FALLBACK_PROMPT: &str = "The sidekick delegation failed. Continue with exactly one lead fallback attempt and produce the final response without delegating again.";

const CACHE_BREAKPOINT_LONG_SESSION: usize = 60;
const CACHE_BREAKPOINT_MEDIUM_SESSION: usize = 30;
const CACHE_BREAKPOINT_SHORT_SESSION: usize = 10;

const CACHE_BREAKPOINTS_LONG: usize = 4;
const CACHE_BREAKPOINTS_MEDIUM: usize = 3;
const CACHE_BREAKPOINTS_SHORT: usize = 2;
const CACHE_BREAKPOINTS_MIN: usize = 1;

fn filter_provider_tools(tools: &mut Value, filter: &ToolFilter, mode: &AgentMode) {
    crate::tools::filter_definitions(tools, filter);
    filter_tools_for_mode(tools, mode);
}

fn filter_tools_for_mode(tools: &mut Value, mode: &AgentMode) {
    if !mode.is_readonly() {
        return;
    }
    if let Some(definitions) = tools.as_array_mut() {
        let registry = ToolRegistry::global();
        definitions.retain(|definition| {
            let Some(name) = definition.get("name").and_then(Value::as_str) else {
                return true;
            };
            let canonical = canonical_tool_name(name);
            // Bash is the only execute-kind tool allowed in plan mode, and only
            // for commands that pass the read-only classifier.
            if canonical == crate::tools::BASH_TOOL_NAME {
                return true;
            }
            // Keep the historical name-based guard and extend it to any tool
            // whose kind is "execute", so a renamed code_execution tool is
            // also filtered out.
            if canonical == crate::tools::CODE_EXECUTION_TOOL_NAME {
                return false;
            }
            // Only remove tools we can positively identify as execute-kind.
            // MCP and other unregistered tools are left for the dispatch layer.
            registry
                .get(canonical)
                .map_or(true, |entry| entry.tool.tool_kind() != Some("execute"))
        });
    }
}

/// Choose how many recent user-message breakpoints to mark for prompt caching.
///
/// Short conversations avoid paying for extra cache writes; longer
/// conversations benefit from more breakpoints because larger stable prefixes
/// can be reused on subsequent turns. The cap prevents runaway cache creation
/// costs as sessions grow very long.
#[must_use]
fn adaptive_cache_breakpoints(user_message_count: usize) -> usize {
    if user_message_count >= CACHE_BREAKPOINT_LONG_SESSION {
        CACHE_BREAKPOINTS_LONG
    } else if user_message_count >= CACHE_BREAKPOINT_MEDIUM_SESSION {
        CACHE_BREAKPOINTS_MEDIUM
    } else if user_message_count >= CACHE_BREAKPOINT_SHORT_SESSION {
        CACHE_BREAKPOINTS_SHORT
    } else {
        CACHE_BREAKPOINTS_MIN
    }
}

/// Resolves the model to use for compaction.
///
/// # Panics
/// Panics if the model registry lock is poisoned.
pub fn resolve_compaction_model(
    provider: &Arc<dyn Provider>,
    model: &Model,
    timeouts: n00n_providers::Timeouts,
    openai_options: OpenAiOptions,
) -> (Arc<dyn Provider>, Model) {
    if let Ok(registry) = n00n_providers::model_registry::model_registry().read()
        && let Some(spec) = registry.spec_for_tier_any(n00n_providers::ModelTier::Compaction)
        && let Ok(mut m) = Model::from_spec(&spec)
        && let Ok(p) = n00n_providers::provider::from_model_with_openai_options(
            &mut m,
            timeouts,
            openai_options,
        )
    {
        // Compaction budgeting (target/budget/remaining) must never exceed the
        // main chat model's context window: the retained transcript is fed back
        // to that model, and the summarizer request must fit the compaction
        // model's own window. The minimum satisfies both constraints. Only
        // pricing and streaming use the compaction model's other fields.
        if model.context_window != 0 {
            m.context_window = m.context_window.min(model.context_window);
        }
        return (Arc::from(p), m);
    }
    (Arc::clone(provider), model.clone())
}

enum TurnOutcome {
    Continue,
    Done(Option<StopReason>),
}

#[derive(Clone)]
pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub config: Arc<AgentConfig>,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub identity: Option<SessionIdentity>,
    pub timeouts: n00n_providers::Timeouts,
    pub openai_options: OpenAiOptions,
    pub file_tracker: Arc<FileReadTracker>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub subagent_cancels: Arc<CancelMap<String>>,
    pub registry: Arc<crate::tools::ToolRegistry>,
    pub audience: ToolAudience,
    pub state_revision: Option<u64>,
}

pub struct AgentRunParams<'h> {
    pub history: &'h mut History,
    pub system: System,
    pub event_tx: EventSender,
    pub tools: Value,
    pub tool_filter: ToolFilter,
}

#[cfg(test)]
enum TestCompactionHooks {
    Enabled,
    Disabled,
}

type CompactionCheckpoint =
    Box<dyn FnMut(&History, u64) -> Result<(), String> + Send + Sync + 'static>;

pub struct Agent<'h> {
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    history: &'h mut History,
    system: System,
    event_tx: EventSender,
    tools: Value,
    mode: Arc<AgentMode>,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    total_usage: TokenUsage,
    total_cost: f64,
    context_size: u32,
    num_turns: u32,
    recent_calls: RecentCalls,
    auto_compact: bool,
    #[cfg(test)]
    test_compaction_hooks: TestCompactionHooks,
    loaded_instructions: LoadedInstructions,
    pre_dispatch_rollback_len: Option<usize>,
    rollback_len: Option<usize>,
    pre_dispatch_gate: Option<Arc<PreDispatchGate>>,
    mcp: Option<McpSession>,
    config: Arc<AgentConfig>,
    tool_output_lines: ToolOutputLines,
    reauth_attempts: u32,
    post_tool_empty_retried: bool,
    thinking_empty_retried: bool,
    permissions: Arc<PermissionManager>,
    opts: RequestOptions,
    identity: Option<SessionIdentity>,
    timeouts: n00n_providers::Timeouts,
    openai_options: OpenAiOptions,
    file_tracker: Arc<FileReadTracker>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    subagent_cancels: Arc<crate::cancel::CancelMap<String>>,
    registry: Arc<crate::tools::ToolRegistry>,
    admission_scope: Arc<str>,
    audience: ToolAudience,
    workflow: bool,
    local_tools: LocalTools,
    active_skill_policy: Option<crate::skill_policy::ActiveSkillPolicy>,
    tool_filter: ToolFilter,
    allow_dynamic_mcp_tools: bool,
    active_tools: Arc<RwLock<ActiveTools>>,
    supports_tool_examples: bool,
    fusion_state: Option<FusionState>,
    state_revision: Option<u64>,
    state_revision_allocator: Option<Arc<dyn Fn() -> Result<u64, String> + Send + Sync>>,
    compaction_checkpoint: Option<CompactionCheckpoint>,
}

impl<'h> Agent<'h> {
    #[must_use]
    pub fn new(params: AgentParams, run: AgentRunParams<'h>) -> Self {
        let supports_tool_examples = params.model.supports_tool_examples();
        let fusion_enabled = params.config.fusion.enabled;
        let admission_scope = params
            .identity
            .as_ref()
            .map(SessionIdentity::session_id)
            .map_or_else(crate::tools::ToolAdmission::new_scope, |id| {
                Arc::<str>::from(id.to_string())
            });
        let fusion_state = if fusion_enabled {
            Some(FusionState::new_lead())
        } else {
            None
        };
        let mut agent = Self {
            provider: params.provider,
            model: Arc::new(params.model),
            config: params.config,
            tool_output_lines: params.tool_output_lines,
            permissions: params.permissions,
            timeouts: params.timeouts,
            openai_options: params.openai_options,
            history: run.history,
            system: run.system,
            event_tx: run.event_tx,
            tools: run.tools,
            mode: Arc::new(AgentMode::default()),
            user_response_rx: None,
            interrupt_source: None,
            cancel: CancelToken::none(),
            total_usage: TokenUsage::default(),
            total_cost: 0.0,
            context_size: 0,
            num_turns: 0,
            recent_calls: RecentCalls::new(),
            auto_compact: compaction::auto_compact_enabled(),
            #[cfg(test)]
            test_compaction_hooks: TestCompactionHooks::Enabled,
            loaded_instructions: LoadedInstructions::new(),
            pre_dispatch_rollback_len: None,
            rollback_len: None,
            pre_dispatch_gate: None,
            mcp: None,
            reauth_attempts: 0,
            post_tool_empty_retried: false,
            thinking_empty_retried: false,
            opts: RequestOptions::default(),
            identity: params.identity,
            file_tracker: params.file_tracker,
            prompt_slots: params.prompt_slots,
            subagent_cancels: params.subagent_cancels,
            registry: params.registry,
            admission_scope,
            audience: params.audience,
            workflow: false,
            local_tools: LocalTools::default(),
            active_skill_policy: None,
            tool_filter: run.tool_filter,
            allow_dynamic_mcp_tools: false,
            active_tools: Arc::new(RwLock::new(ActiveTools::default())),
            supports_tool_examples,
            fusion_state,
            state_revision: params.state_revision,
            state_revision_allocator: None,
            compaction_checkpoint: None,
        };
        if fusion_enabled {
            agent
                .system
                .push_dynamic(crate::fusion::fusion_lead_system_append());
        }
        agent.warm_active_tools();
        agent
    }

    #[must_use]
    pub fn with_mcp(mut self, mcp: Option<McpSession>) -> Self {
        self.mcp = mcp;
        self
    }

    #[must_use]
    pub fn with_dynamic_mcp_tools(mut self, enabled: bool) -> Self {
        self.allow_dynamic_mcp_tools = enabled;
        self
    }

    #[must_use]
    pub fn with_user_response_rx(
        mut self,
        rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
    ) -> Self {
        self.user_response_rx = Some(rx);
        self
    }

    #[must_use]
    pub fn with_interrupt_source(mut self, source: Arc<dyn InterruptSource>) -> Self {
        self.interrupt_source = Some(source);
        self
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    #[must_use]
    pub fn with_pre_dispatch_gate(mut self, gate: Arc<PreDispatchGate>) -> Self {
        self.pre_dispatch_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_pre_dispatch_rollback_len(mut self, rollback_len: usize) -> Self {
        self.pre_dispatch_rollback_len = Some(rollback_len);
        self
    }

    #[must_use]
    pub fn with_local_tools(mut self, local_tools: LocalTools) -> Self {
        self.local_tools = local_tools;
        self
    }

    #[must_use]
    pub fn with_loaded_instructions(mut self, loaded: LoadedInstructions) -> Self {
        self.loaded_instructions = loaded;
        self
    }

    #[must_use]
    pub fn total_usage(&self) -> TokenUsage {
        self.total_usage
    }

    #[must_use]
    pub fn total_cost(&self) -> f64 {
        self.total_cost
    }

    #[must_use]
    pub fn state_revision(&self) -> Option<u64> {
        self.state_revision
    }

    #[must_use]
    pub fn with_state_revision_allocator(
        mut self,
        allocator: Arc<dyn Fn() -> Result<u64, String> + Send + Sync>,
    ) -> Self {
        self.state_revision_allocator = Some(allocator);
        self
    }

    #[must_use]
    pub fn with_compaction_checkpoint(
        mut self,
        checkpoint: impl FnMut(&History, u64) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.compaction_checkpoint = Some(Box::new(checkpoint));
        self
    }

    /// Runs one tool and emits its completion event.
    ///
    /// # Errors
    /// Returns an error when the completion event cannot be delivered.
    pub async fn run_tool(
        &self,
        id: String,
        name: &str,
        input: &Value,
    ) -> Result<ToolDoneEvent, AgentError> {
        let ctx = self.tool_context();
        let done = tool_dispatch::run(
            &self.registry,
            self.mcp.as_ref(),
            id,
            name,
            input,
            &ctx,
            tool_dispatch::Emit::Notify,
        )
        .await;
        self.event_tx
            .send(AgentEvent::ToolDone(Box::new(done.clone())))?;
        Ok(done)
    }

    /// Runs the agent loop with the given input.
    ///
    /// # Errors
    /// Returns an error if the agent loop fails due to provider errors,
    /// tool execution failures, or cancellation.
    pub async fn run(&mut self, input: AgentInput) -> Result<(), AgentError> {
        let protect_history_replay = !self.history.is_empty();
        if self.config.fusion.enabled {
            self.fusion_state = Some(FusionState::new_lead());
        }
        let rollback_len = self.rollback_len.unwrap_or_else(|| self.history.len());
        self.rollback_len = Some(rollback_len);
        let pre_dispatch_rollback_len = self
            .pre_dispatch_rollback_len
            .unwrap_or_else(|| rollback_len);
        validate_input_message(&input)?;
        let mut msg = Message::user_with_images(input.message.clone(), input.images);
        msg.control = input.control;
        self.history.push(msg);
        self.mode = Arc::new(input.mode);
        self.workflow = input.workflow;
        // Filter the caller-supplied tool list in place. Rebuilding from the
        // global registry would replace curated/session-local definitions
        // (e.g. structured_output) and expand restricted ToolFilter sets.
        // Extend MCP definitions first (always-load + loaded tools) so the
        // filtered list is complete without rebuilding from the registry.
        if let Some(mcp) = self.mcp.as_ref() {
            mcp.extend_tools(&mut self.tools);
        }
        let tool_filter = self.effective_tool_filter();
        filter_provider_tools(&mut self.tools, &tool_filter, &self.mode);
        self.context_size = estimate_message_tokens(self.history.as_slice(), &self.model.id)
            .saturating_add(estimate_tool_tokens(&self.tools, &self.model.id));
        let user_message_count = self
            .history
            .as_slice()
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .count();
        self.opts = RequestOptions {
            thinking: input.thinking,
            fast: input.fast,
            message_cache_breakpoints: adaptive_cache_breakpoints(user_message_count),
            openai_prompt_cache_mode: None,
            protect_history_replay,
            allow_history_replay: false,
            safety_identifier: None,
            moderation: false,
            idempotency_key: None,
            idempotency_supported: false,
            hosted_tool_search: None,
        };

        info!(
            model = %self.model.id,
            mode = ?self.mode,
            message_len = input.message.len(),
            "agent run started"
        );
        if let Some(state) = self.fusion_state.as_mut() {
            state.set_request_kind(crate::fusion::classify_delegation(&input.message));
            self.emit_fusion_phase(FusionPhase::Planning)?;
        }

        let result = async {
            self.try_auto_compact().await?;
            self.run_loop().await
        }
        .await;

        if let Some(state) = self.fusion_state.as_mut() {
            match &result {
                Err(AgentError::Cancelled) => {
                    if let Err(error) = state.cancel() {
                        warn!(?error, "fusion: failed to mark run cancelled");
                    }
                }
                Err(_) => {
                    if let Err(error) = state.fail() {
                        warn!(?error, "fusion: failed to mark run failed");
                    }
                }
                Ok(()) => {}
            }
            if matches!(&result, Err(AgentError::Cancelled)) && self.fusion_state.is_some() {
                self.emit_fusion_phase(FusionPhase::Cancelled)?;
            } else if result.is_err() && self.fusion_state.is_some() {
                self.emit_fusion_phase(FusionPhase::Failed)?;
            }
        }

        if matches!(result, Err(AgentError::Cancelled)) {
            if self
                .pre_dispatch_gate
                .as_ref()
                .is_some_and(|gate| gate.is_cancelled())
            {
                self.history.truncate(pre_dispatch_rollback_len);
            } else {
                let rollback_len = self.rollback_len.unwrap_or_else(|| self.history.len());
                sanitize_cancelled_history(self.history, rollback_len);
            }
        }

        result
    }

    async fn run_loop(&mut self) -> Result<(), AgentError> {
        loop {
            if let Some(max) = self.config.max_turns
                && self.num_turns >= max
            {
                self.complete_fusion_phase()?;
                self.emit_done(None)?;
                return Ok(());
            }
            match self.turn().await? {
                TurnOutcome::Continue => {}
                TurnOutcome::Done(stop_reason) => {
                    self.complete_fusion_phase()?;
                    self.emit_done(stop_reason)?;
                    return Ok(());
                }
            }
        }
    }

    fn commit_pre_dispatch(&self) -> bool {
        self.pre_dispatch_gate
            .as_ref()
            .is_none_or(|gate| gate.try_commit())
    }

    async fn stream_response(
        &self,
        mut opts: RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        if self.provider.supports_hosted_tool_search(&self.model) {
            opts.hosted_tool_search = self.hosted_tool_search();
        }
        stream_with_retry(super::streaming::StreamContext {
            provider: &*self.provider,
            model: &self.model,
            messages: self.history.as_slice(),
            system: &self.system,
            tools: &self.tools,
            event_tx: &self.event_tx,
            cancel: &self.cancel,
            opts,
            session_id: self.identity.as_ref().map(SessionIdentity::session_id),
        })
        .await
    }

    /// History replay resends the full transcript. In YOLO mode this is
    /// auto-approved because the cost is bounded and the operation is idempotent.
    /// YOLO is checked live so a mid-run toggle takes effect immediately; the
    /// per-request `allow_history_replay` flag is recomputed from `is_yolo()`
    /// each turn and flipped after an explicit approval. Ambiguous replay
    /// intentionally does NOT auto-approve in YOLO — it may duplicate provider
    /// charges/output.
    async fn approve_history_replay(&self, reason: HistoryReplayReason) -> Result<(), AgentError> {
        if self.permissions.is_yolo() {
            return Ok(());
        }
        let scope = history_replay_scope(
            reason,
            self.history.as_slice(),
            &self.system,
            &self.tools,
            &self.model,
            self.opts.fast,
        );
        let Some(response_rx) = self.user_response_rx.as_deref() else {
            return Err(AgentError::Config {
                message: format!("Full-history replay blocked without explicit approval. {scope}"),
            });
        };
        let response_rx = response_rx.lock().await;
        self.event_tx.send(AgentEvent::PermissionRequest {
            id: HISTORY_REPLAY_PERMISSION_ID.to_string(),
            tool: ToolKey::native(HISTORY_REPLAY_TOOL),
            scopes: vec![scope.clone()],
        })?;
        let response = self
            .cancel
            .race(response_rx.recv_async())
            .await
            .map_err(|_| AgentError::Cancelled)?;
        drop(response_rx);
        let answer = response.map_err(|error| AgentError::Config {
            message: format!("{HISTORY_REPLAY_CHANNEL_CLOSED_MESSAGE}: {error}"),
        })?;
        let approved = PermissionAnswer::decode(&answer).is_some_and(|answer| answer.is_allow());
        if approved {
            Ok(())
        } else {
            Err(AgentError::Config {
                message: format!("Full-history replay was not approved. {scope}"),
            })
        }
    }

    /// Ambiguous replay may duplicate provider output or charges after a
    /// connection reset. Even in YOLO mode this requires an explicit
    /// `PermissionRequest`; YOLO only auto-approves history replay (see
    /// `approve_history_replay` and `yolo_mode_does_not_auto_approve_ambiguous_request_replay`).
    async fn approve_ambiguous_request_replay(
        &self,
        metadata: Option<&RequestDeliveryMetadata>,
    ) -> Result<bool, AgentError> {
        let Some(response_rx) = self.user_response_rx.as_deref() else {
            return Ok(false);
        };
        let response_rx = response_rx.lock().await;
        self.event_tx.send(AgentEvent::PermissionRequest {
            id: AMBIGUOUS_REPLAY_PERMISSION_ID.to_string(),
            tool: ToolKey::native(AMBIGUOUS_REPLAY_TOOL),
            scopes: vec![ambiguous_request_replay_scope(metadata)],
        })?;
        let response = self
            .cancel
            .race(response_rx.recv_async())
            .await
            .map_err(|_| AgentError::Cancelled)?;
        drop(response_rx);
        let answer = response.map_err(|error| AgentError::Config {
            message: format!("{AMBIGUOUS_REPLAY_CHANNEL_CLOSED_MESSAGE}: {error}"),
        })?;
        Ok(PermissionAnswer::decode(&answer).is_some_and(|answer| answer.is_allow()))
    }

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() || !self.commit_pre_dispatch() {
            return Err(AgentError::Cancelled);
        }
        let mut opts = self.opts.clone();
        opts.allow_history_replay = self.permissions.is_yolo();
        let mut approved_history_replay = false;
        let mut approved_ambiguous_replay = false;
        let response = loop {
            match self.stream_response(opts.clone()).await {
                Err(AgentError::HistoryReplayRequired { reason }) if !approved_history_replay => {
                    self.approve_history_replay(reason).await?;
                    approved_history_replay = true;
                    opts.allow_history_replay = true;
                }
                Err(error @ AgentError::RequestSent { .. }) if !approved_ambiguous_replay => {
                    let metadata = match &error {
                        AgentError::RequestSent { metadata, .. } => metadata.as_ref(),
                        _ => None,
                    };
                    let output_emitted = metadata.is_some_and(|metadata| metadata.emitted_event);
                    if !self.approve_ambiguous_request_replay(metadata).await? {
                        break Err(error);
                    }
                    self.event_tx.send(AgentEvent::Retry {
                        attempt: 1,
                        message: AMBIGUOUS_REPLAY_RESET_MESSAGE.into(),
                        delay_ms: 0,
                    })?;
                    warn!(
                        delivery_phase = ?metadata.map(|metadata| metadata.phase),
                        response_id_present = metadata.is_some_and(|metadata| metadata.response_id.is_some()),
                        output_emitted,
                        "replaying ambiguous provider request after approval"
                    );
                    approved_ambiguous_replay = true;
                }
                result => break result,
            }
        };
        let response = match response {
            Ok(r) => {
                self.reauth_attempts = 0;
                r
            }
            Err(e) if e.is_auth_error() => {
                return self.wait_for_reauth(e).await;
            }
            Err(e) if e.is_cancelled() => {
                warn!(error = %e, model = %self.model.id, self.num_turns, "stream_message cancelled");
                return Err(e);
            }
            Err(e) => {
                error!(error = %e, model = %self.model.id, self.num_turns, "stream_message failed");
                return Err(e);
            }
        };
        self.num_turns += 1;
        self.finish_turn(response).await
    }

    async fn finish_turn(&mut self, response: StreamResponse) -> Result<TurnOutcome, AgentError> {
        let has_tools = response.message.has_tool_calls();
        let stop_reason = response.stop_reason;
        info!(
            input_tokens = response.usage.input,
            output_tokens = response.usage.output,
            cache_creation = response.usage.cache_creation,
            cache_read = response.usage.cache_read,
            has_tools,
            self.num_turns,
            model = %self.model.id,
            stop_reason = stop_reason.map_or("none", Into::into),
            "API response received"
        );

        let usage = response.usage;
        let cost = usage.cost(&self.model.pricing, self.opts.fast);
        self.record_usage(usage, cost);
        self.context_size = usage.context_tokens();
        self.emit_turn_complete(&response)?;

        let after_tool_results = self.history.ends_with_tool_results();

        if has_tools {
            let history_len_before = self.history.len();
            let tool_results = self.process_tool_calls(response).await?;
            if self.config.fusion.enabled
                && let Some(state) = self.fusion_state.as_mut()
            {
                state.observe_tool_results(&tool_results);
            }
            self.handle_fusion_results(&tool_results)?;
            self.apply_skill_policy_from_results(&tool_results);
            if self.apply_tool_search_results(&tool_results) {
                self.rebuild_tools();
            }
            let tool_results_start = history_len_before.saturating_add(1);
            self.context_size = self.context_size.saturating_add(estimate_message_tokens(
                &self.history.as_slice()[tool_results_start..],
                &self.model.id,
            ));
        } else {
            let has_text = response
                .message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if !text.is_empty()));
            let has_thinking = response.message.content.iter().any(|b| {
                matches!(
                    b,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            });

            if !has_text && !self.post_tool_empty_retried && after_tool_results {
                self.post_tool_empty_retried = true;
                if !response.message.content.is_empty() {
                    self.history.push(response.message);
                }
                demoted!(
                    "empty or reasoning-only response after tool calls, nudging model to continue"
                );
                self.event_tx.send(AgentEvent::Nudge)?;
                self.history.push(Message::synthetic(NUDGE_PROMPT.into()));
                return Ok(TurnOutcome::Continue);
            }

            if !has_text && has_thinking && !after_tool_results && !self.thinking_empty_retried {
                self.thinking_empty_retried = true;
                demoted!("assistant produced only reasoning, nudging for final answer");
                self.history.push(response.message);
                self.event_tx.send(AgentEvent::Nudge)?;
                self.history
                    .push(Message::synthetic(THINKING_NUDGE_PROMPT.into()));
                return Ok(TurnOutcome::Continue);
            }

            self.history.push(response.message);

            if has_text {
                self.thinking_empty_retried = false;
                if after_tool_results {
                    self.post_tool_empty_retried = false;
                }
            }

            if stop_reason == Some(StopReason::MaxTokens)
                && self.num_turns <= self.config.max_continuation_turns
            {
                warn!(
                    self.num_turns,
                    "response truncated (max_tokens), re-prompting"
                );
                self.history
                    .push(Message::synthetic(MAX_TOKENS_CONTINUE_PROMPT.into()));
                self.context_size = self.context_size.saturating_add(estimate_message_tokens(
                    &self.history.as_slice()[self.history.len().saturating_sub(1)..],
                    &self.model.id,
                ));
                self.try_auto_compact().await?;
                return Ok(TurnOutcome::Continue);
            }
        }

        if self.try_auto_compact().await?
            || self
                .handle_queued_commands(if has_tools {
                    InterruptPoint::ToolComplete
                } else {
                    InterruptPoint::Safe
                })
                .await?
        {
            return Ok(TurnOutcome::Continue);
        }

        if has_tools {
            Ok(TurnOutcome::Continue)
        } else {
            if let Some(state) = self.fusion_state.as_mut()
                && !state.phase().is_terminal()
                && let Err(error) = state.transition(FusionPhase::Complete)
            {
                warn!(?error, "fusion: failed to mark run complete");
            }
            Ok(TurnOutcome::Done(stop_reason))
        }
    }

    async fn wait_for_reauth(&mut self, err: AgentError) -> Result<TurnOutcome, AgentError> {
        if self.reauth_attempts >= MAX_REAUTH_ATTEMPTS {
            error!(error = %err, attempts = self.reauth_attempts, "max re-auth attempts reached");
            return Err(err);
        }
        let Some(rx) = &self.user_response_rx else {
            error!(error = %err, model = %self.model.id, self.num_turns, "stream_message failed");
            return Err(err);
        };
        self.reauth_attempts += 1;
        warn!(error = %err, attempt = self.reauth_attempts, "auth error, waiting for re-authentication");
        self.event_tx.send(AgentEvent::AuthRequired)?;
        let rx = rx.lock().await;
        match futures_lite::future::race(rx.recv_async(), async {
            self.cancel.cancelled().await;
            Err(flume::RecvError::Disconnected)
        })
        .await
        {
            Ok(_) => {
                self.provider.refresh_auth().await?;
                Ok(TurnOutcome::Continue)
            }
            Err(_) => Err(AgentError::Cancelled),
        }
    }

    fn record_usage(&mut self, usage: TokenUsage, cost: f64) {
        if self.config.fusion.enabled
            && let Some(state) = self.fusion_state.as_mut()
        {
            // The main agent always runs the lead wire model/provider; sidekick
            // costs are recorded from fusion_delegate telemetry instead.
            state.record_lane_usage(FusionLane::Lead, usage, cost);
        }
        self.total_usage += usage;
        self.total_cost += cost;
    }

    fn emit_turn_complete(&self, response: &StreamResponse) -> Result<(), AgentError> {
        self.event_tx
            .send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: response.message.clone(),
                usage: response.usage,
                model: self.model.spec(),
                context_size: Some(response.usage.context_tokens()),
            })))
    }

    fn emit_done(&self, stop_reason: Option<StopReason>) -> Result<(), AgentError> {
        info!(
            self.num_turns,
            total_input = self.total_usage.input,
            total_output = self.total_usage.output,
            "agent run completed"
        );
        self.event_tx.send(AgentEvent::Done {
            usage: self.total_usage,
            num_turns: self.num_turns,
            stop_reason,
            fusion: self
                .fusion_state
                .as_ref()
                .filter(|_| self.config.fusion.enabled)
                .map(FusionState::usage_stats),
        })
    }

    async fn process_tool_calls(
        &mut self,
        response: StreamResponse,
    ) -> Result<Vec<ToolDoneEvent>, AgentError> {
        self.post_tool_empty_retried = false;
        self.thinking_empty_retried = false;
        let ctx = self.tool_context();
        let fusion = self
            .fusion_state
            .as_ref()
            .map(|state| tool_dispatch::FusionDispatchAuth {
                phase: state.phase(),
                lane: state.lane(),
                classification: state.request_kind(),
            });
        tool_dispatch::process_tool_calls(
            response,
            &mut self.recent_calls,
            self.mcp.as_ref(),
            self.history,
            &self.event_tx,
            &ctx,
            fusion,
        )
        .await
    }

    fn emit_fusion_phase(&self, phase: FusionPhase) -> Result<(), AgentError> {
        self.event_tx
            .send(AgentEvent::FusionPhase { phase, label: None })
    }

    fn complete_fusion_phase(&mut self) -> Result<(), AgentError> {
        let Some(state) = self.fusion_state.as_mut() else {
            return Ok(());
        };
        if !state.phase().is_terminal()
            && let Err(error) = state.transition(FusionPhase::Complete)
        {
            warn!(?error, "fusion: failed to mark run complete");
        }
        self.emit_fusion_phase(FusionPhase::Complete)
    }

    fn handle_fusion_results(&mut self, results: &[ToolDoneEvent]) -> Result<(), AgentError> {
        let Some(result) = results.iter().find(|result| {
            canonical_tool_name(&result.tool) == crate::fusion::FUSION_DELEGATE_TOOL
                && result.output.as_text() != crate::fusion::FUSION_DELEGATE_BLOCKED
        }) else {
            return Ok(());
        };
        let Some(state) = self.fusion_state.as_mut() else {
            return Ok(());
        };
        if let Err(error) = state.transition(FusionPhase::Executing) {
            warn!(?error, "fusion: rejected delegation transition");
            return Ok(());
        }
        self.emit_fusion_phase(FusionPhase::Executing)?;

        if result.is_error {
            let failure = fusion_failure_from_result(result);
            if let Some(state) = self.fusion_state.as_mut()
                && let Err(error) = state.delegate_failed(failure)
            {
                warn!(?error, ?failure, "fusion: rejected fallback transition");
                return Ok(());
            }
            self.history
                .push(Message::synthetic(FUSION_FALLBACK_PROMPT.into()));
            self.emit_fusion_phase(FusionPhase::LeadFallback)?;
        } else {
            if let Some(state) = self.fusion_state.as_mut()
                && let Err(error) = state.transition(FusionPhase::Reviewing)
            {
                warn!(?error, "fusion: rejected review transition");
                return Ok(());
            }
            self.history
                .push(Message::synthetic(FUSION_REVIEW_PROMPT.into()));
            self.emit_fusion_phase(FusionPhase::Reviewing)?;
        }
        Ok(())
    }

    fn effective_tool_filter(&self) -> ToolFilter {
        let Some(mcp) = self.mcp.as_ref() else {
            return self.tool_filter.clone();
        };
        let mut filter = self.tool_filter.clone();
        let tool_search = crate::mcp::TOOL_SEARCH_TOOL_NAME;
        if crate::tools::is_tool_enabled(&self.config.disabled_tools, tool_search) {
            if !filter.matches(tool_search) {
                filter = filter.including([tool_search.to_owned()]);
            }
        } else {
            filter = filter.excluding(&[tool_search]);
        }
        if !self.allow_dynamic_mcp_tools {
            return filter;
        }
        let capability_exclusions = crate::tools::capability_exclusions(&self.model);
        let mut definitions = Value::Array(Vec::new());
        mcp.extend_tools(&mut definitions);
        if self.provider.supports_hosted_tool_search(&self.model)
            && let Some(items) = definitions.as_array_mut()
        {
            items.extend(
                mcp.deferred_definitions()
                    .into_iter()
                    .map(|tool| tool.definition),
            );
        }
        let names = definitions
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|definition| definition.get("name").and_then(Value::as_str))
            .filter(|name| {
                crate::tools::is_tool_enabled(&self.config.disabled_tools, name)
                    && !capability_exclusions.contains(name)
            })
            .map(str::to_owned);
        filter.including(names)
    }

    fn hosted_tool_search(&self) -> Option<HostedToolSearch> {
        let vars = crate::template::env_vars();
        let effective_filter = self.effective_tool_filter();
        let ctx = crate::tools::DescriptionContext {
            filter: &effective_filter,
            audience: self.audience,
            workflow: self.workflow,
        };
        let active_snapshot = self
            .active_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut definitions = self.registry.deferred_definitions(
            &vars,
            &ctx,
            self.supports_tool_examples,
            &active_snapshot,
        );
        if let Some(mcp) = &self.mcp {
            definitions.extend(mcp.deferred_definitions());
            definitions.sort_by(|left, right| {
                left.namespace.cmp(&right.namespace).then_with(|| {
                    left.definition["name"]
                        .as_str()
                        .cmp(&right.definition["name"].as_str())
                })
            });
        }
        let mut mode_filtered = Value::Array(
            definitions
                .iter()
                .map(|tool| tool.definition.clone())
                .collect(),
        );
        filter_provider_tools(&mut mode_filtered, &effective_filter, &self.mode);
        let allowed: std::collections::HashSet<&str> = mode_filtered
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|definition| definition.get("name").and_then(Value::as_str))
            .collect();
        definitions.retain(|tool| {
            tool.definition["name"]
                .as_str()
                .is_some_and(|name| allowed.contains(name))
        });
        (!definitions.is_empty()).then_some(HostedToolSearch { tools: definitions })
    }

    fn tool_context(&self) -> ToolContext {
        ToolContext {
            provider: Arc::clone(&self.provider),
            model: Arc::clone(&self.model),
            event_tx: self.event_tx.clone(),
            mode: Arc::clone(&self.mode),
            tool_use_id: None,
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deadline: Deadline::None,
            config: Arc::clone(&self.config),
            tool_output_lines: self.tool_output_lines,
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            openai_options: self.openai_options.clone(),
            file_tracker: Arc::clone(&self.file_tracker),
            prompt_slots: Arc::clone(&self.prompt_slots),
            opts: self.opts.clone(),
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            identity: self.identity.clone(),
            registry: Arc::clone(&self.registry),
            admission_scope: Arc::clone(&self.admission_scope),
            workflow: self.workflow,
            audience: self.audience,
            local_tools: Arc::clone(&self.local_tools),
            active_skill_policy: self.active_skill_policy.clone(),
            tool_filter: self.effective_tool_filter(),
            live_sink: None,
        }
    }

    fn apply_skill_policy_from_results(&mut self, results: &[ToolDoneEvent]) {
        for done in results {
            crate::skill_policy::ActiveSkillPolicy::apply_from_skill_tool_result(
                &mut self.active_skill_policy,
                &done.tool,
                done.is_error,
                done.output.state(),
            );
        }
    }

    fn rebuild_tools(&mut self) {
        let vars = crate::template::env_vars();
        let effective_filter = self.effective_tool_filter();
        let ctx = crate::tools::DescriptionContext {
            filter: &effective_filter,
            audience: self.audience,
            workflow: self.workflow,
        };
        let active_snapshot = self
            .active_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut tools = self.registry.definitions_active(
            &vars,
            &ctx,
            self.supports_tool_examples,
            &active_snapshot,
        );
        if let Some(mcp) = &self.mcp {
            mcp.extend_tools(&mut tools);
        }
        filter_provider_tools(&mut tools, &effective_filter, &self.mode);
        self.tools = tools;
    }

    fn warm_active_tools(&mut self) {
        let Some(arr) = self.tools.as_array() else {
            return;
        };
        let mut active = self
            .active_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for def in arr {
            let Some(name) = def.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(entry) = self.registry.get(name)
                && entry.defer_loading
            {
                active.names.insert(name.to_owned());
            }
        }
    }

    fn apply_tool_search_results(&mut self, results: &[ToolDoneEvent]) -> bool {
        let mut dirty = false;
        let mut active = self
            .active_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for done in results {
            match n00n_config::canonical_tool_name(done.tool.as_ref()) {
                "search_tools" => {
                    let text = done.output.as_text();
                    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) {
                        for item in items {
                            if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                                active.names.insert(name.to_owned());
                                dirty = true;
                            }
                        }
                    }
                }
                "load_toolset" => {
                    let text = done.output.as_text();
                    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text)
                        && let Some(ns) = obj.get("namespace").and_then(|v| v.as_str())
                    {
                        active.namespaces.insert(ns.to_owned());
                        dirty = true;
                    }
                }
                _ => {}
            }
        }
        dirty
    }

    async fn try_auto_compact(&mut self) -> Result<bool, AgentError> {
        if !self.auto_compact
            || !compaction::is_overflow(
                &TokenUsage {
                    input: self.context_size,
                    ..Default::default()
                },
                &self.model,
                self.config.compaction_buffer,
            )
        {
            return Ok(false);
        }
        info!(context_size = self.context_size, "auto-compacting");
        if let Err(error) = self.do_compact_with_status().await {
            if matches!(error, AgentError::Cancelled) {
                return Err(error);
            }
            warn!(
                error = %error,
                "auto-compaction failed; continuing without compacting"
            );
            return Ok(false);
        }
        Ok(true)
    }

    fn apply_fusion_route(&mut self, route: FusionRoute) {
        if self.fusion_state.is_none() {
            return;
        }
        let lane = match route {
            FusionRoute::EscalateToLead => FusionLane::Lead,
            FusionRoute::Stay(lane) | FusionRoute::Switch(lane) => lane,
        };
        if let Some(state) = self.fusion_state.as_mut() {
            state.set_lane(lane);
        }
        self.apply_fusion_lane_context(lane);
    }

    fn apply_fusion_lane_context(&mut self, lane: FusionLane) {
        let append = match lane {
            FusionLane::Lead => crate::fusion::fusion_lead_system_append(),
            FusionLane::Sidekick => crate::fusion::fusion_sidekick_system_append(),
        };
        if !self.system.replace_last_dynamic(append) {
            self.system.push_dynamic(append);
        }
        self.supports_tool_examples = self.model.supports_tool_examples();
        let model_filter = ToolFilter::from_config(&self.config, &self.model, &[]);
        self.tool_filter = std::mem::take(&mut self.tool_filter).intersect(&model_filter);
        self.rebuild_tools();
    }

    async fn do_compact_with_status(&mut self) -> Result<(), AgentError> {
        match self.do_compact().await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.event_tx.send(AgentEvent::AutoCompactFailed {
                    error: error.to_string(),
                })?;
                Err(error)
            }
        }
    }

    async fn do_compact(&mut self) -> Result<(), AgentError> {
        if !self.commit_pre_dispatch() {
            return Err(AgentError::Cancelled);
        }
        self.event_tx.send(AgentEvent::AutoCompacting)?;
        let (compact_provider, compact_model) = resolve_compaction_model(
            &self.provider,
            &self.model,
            self.timeouts,
            self.openai_options.clone(),
        );
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
        #[cfg(test)]
        let run_hooks = matches!(self.test_compaction_hooks, TestCompactionHooks::Enabled);
        #[cfg(not(test))]
        let run_hooks = true;
        let next_state_revision = if self.state_revision_allocator.is_some() {
            None
        } else {
            self.state_revision
                .map(|revision| {
                    revision.checked_add(1).ok_or_else(|| AgentError::Config {
                        message: "compaction state revision overflow".into(),
                    })
                })
                .transpose()?
        };
        let previous_messages = self.history.as_slice().to_vec();
        let previous_transcript = self.history.transcript().to_vec();
        let previous_state_revision = self.state_revision;
        let (usage, summary) = compaction::compact_history(
            &*compact_provider,
            &compact_model,
            self.history,
            &self.event_tx,
            &self.cancel,
            CompactionTrigger::Auto,
            self.identity.as_ref().map(SessionIdentity::session_id),
            &cwd,
            None,
            run_hooks,
            next_state_revision,
        )
        .await?;
        let cost = usage.cost(&compact_model.pricing, false);
        self.record_usage(usage, cost);
        let next_state_revision = if let Some(allocator) = &self.state_revision_allocator {
            match allocator() {
                Ok(revision) => {
                    if let Err(message) = self.history.set_outer_compaction_state_revision(revision)
                    {
                        self.history.restore(previous_messages, previous_transcript);
                        return Err(AgentError::Config {
                            message: message.into(),
                        });
                    }
                    Some(revision)
                }
                Err(message) => {
                    self.history.restore(previous_messages, previous_transcript);
                    return Err(AgentError::Config { message });
                }
            }
        } else {
            next_state_revision
        };
        self.state_revision = next_state_revision;
        if let (Some(revision), Some(checkpoint)) =
            (next_state_revision, self.compaction_checkpoint.as_mut())
            && let Err(message) = checkpoint(self.history, revision)
        {
            self.history.restore(previous_messages, previous_transcript);
            self.state_revision = previous_state_revision;
            return Err(AgentError::Config {
                message: format!("failed to persist compaction checkpoint: {message}"),
            });
        }
        if self.config.fusion.enabled {
            let route = self.fusion_state.as_mut().map(|state| {
                let recent_errors = state.recent_tool_errors();
                let route = crate::fusion::route_after_compact(state, &summary, recent_errors);
                state.clear_recent_tool_errors();
                route
            });
            if let Some(route) = route {
                self.apply_fusion_route(route);
            }
        }
        self.rollback_len = Some(self.history.len());
        self.history
            .push(Message::synthetic(CONTINUE_AFTER_COMPACT.into()));
        self.context_size = estimate_message_tokens(self.history.as_slice(), &self.model.id)
            .saturating_add(estimate_tool_tokens(&self.tools, &self.model.id));
        self.event_tx
            .send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: Message::assistant(summary),
                usage,
                model: compact_model.spec(),
                context_size: Some(self.context_size),
            })))?;
        self.event_tx.send(AgentEvent::CompactionDone {
            state_revision: next_state_revision,
        })?;
        Ok(())
    }

    async fn handle_queued_commands(&mut self, point: InterruptPoint) -> Result<bool, AgentError> {
        let Some(source) = self.interrupt_source.clone() else {
            return Ok(false);
        };
        let mut handled = false;
        while let Some(cmd) = source.poll(point) {
            handled = true;
            match cmd {
                ExtractedCommand::Interrupt(mut input, _) => {
                    validate_input_message(&input)?;
                    self.event_tx.send(AgentEvent::QueueItemConsumed {
                        text: input.message.clone(),
                        image_count: input.images.len(),
                        images: input.images.clone(),
                        control: input.control,
                    })?;
                    for msg in std::mem::take(&mut input.preamble) {
                        self.history.push(msg);
                    }
                    self.mode = Arc::new(input.mode);
                    if let Some(state) = self.fusion_state.as_mut() {
                        state.set_request_kind(crate::fusion::classify_delegation(&input.message));
                    }
                    let display = input.message;
                    if input.control {
                        let wrapped = format!(
                            "<control-interrupt>\nA control message was sent to this session. Address it and continue.\n\n{display}\n</control-interrupt>"
                        );
                        self.history
                            .push(Message::control_display(wrapped, display));
                    } else {
                        let wrapped = format!(
                            "<user-interrupt>\nThe user sent a new message while you were working. Address it and continue.\n\n{display}\n</user-interrupt>"
                        );
                        self.history.push(Message::user_display(wrapped, display));
                    }
                }
                ExtractedCommand::Compact(_) => {
                    self.do_compact_with_status().await?;
                }
            }
        }
        Ok(handled)
    }
}

fn fusion_failure_from_result(result: &ToolDoneEvent) -> FusionFailure {
    let message = result.output.as_text().to_ascii_lowercase();
    if message.contains("timeout") || message.contains("timed out") {
        FusionFailure::Timeout
    } else if message.contains("model unavailable") {
        FusionFailure::ModelUnavailable
    } else if message.contains("cancel") {
        FusionFailure::DelegateCancelled
    } else {
        FusionFailure::ToolError
    }
}

fn validate_input_message(input: &AgentInput) -> Result<(), AgentError> {
    if input.message.trim().is_empty() && input.images.is_empty() {
        return Err(AgentError::Config {
            message: "message is empty".into(),
        });
    }
    Ok(())
}

fn ambiguous_request_replay_scope(metadata: Option<&RequestDeliveryMetadata>) -> String {
    let phase = match metadata.map(|metadata| metadata.phase) {
        Some(RequestDeliveryPhase::NotSent) => "not sent",
        Some(RequestDeliveryPhase::SentAwaitingAcceptance) => "sent; acceptance unknown",
        Some(RequestDeliveryPhase::Accepted) => "accepted",
        None => "delivery unknown",
    };
    let response_id = metadata
        .and_then(|metadata| metadata.response_id.as_deref())
        .map_or("unknown", |_| "known");
    let output = metadata.map_or("unknown", |metadata| {
        if metadata.emitted_event {
            "already emitted"
        } else {
            "not observed"
        }
    });
    format!(
        "Replay one provider request ({phase}; response ID {response_id}; output {output}). This may duplicate output or charges"
    )
}

fn history_replay_scope(
    reason: HistoryReplayReason,
    messages: &[Message],
    system: &System,
    tools: &Value,
    model: &Model,
    fast: bool,
) -> String {
    let tokenizer = tokenizer_for_model(&model.id);
    let system_tokens =
        u32_from_usize_saturating(count_tokens_with_tokenizer(tokenizer, &system.to_string()));
    let estimated_tokens = estimate_message_tokens(messages, &model.id)
        .saturating_add(estimate_tool_tokens(tools, &model.id))
        .saturating_add(system_tokens);
    let estimated_cost = TokenUsage {
        input: estimated_tokens,
        ..Default::default()
    }
    .cost(&model.pricing, fast);
    let cost = if model.pricing.is_zero() {
        "cost unavailable".to_string()
    } else {
        format!("up to ${estimated_cost:.4} before cache discounts")
    };
    format!(
        "{reason}; resend approximately {estimated_tokens} input tokens ({cost}). Allow this replay?"
    )
}

#[must_use]
fn u32_from_usize_saturating(value: usize) -> u32 {
    if let Ok(n) = u32::try_from(value) {
        n
    } else {
        warn!(value, "token count exceeded u32 range; saturating");
        u32::MAX
    }
}

#[must_use]
pub fn estimate_message_tokens(messages: &[Message], model_id: &str) -> u32 {
    if messages.is_empty() {
        return 0;
    }
    let tokenizer = tokenizer_for_model(model_id);
    let total: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            ContentBlock::Text { text } => count_tokens_with_tokenizer(tokenizer, text),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                count_tokens_with_tokenizer(tokenizer, thinking)
                    + signature
                        .as_ref()
                        .map_or(0, |s| count_tokens_with_tokenizer(tokenizer, s))
            }
            ContentBlock::RedactedThinking { data } => count_tokens_with_tokenizer(tokenizer, data),
            ContentBlock::ToolResult { content, .. } => {
                count_tokens_with_tokenizer(tokenizer, content)
            }
            ContentBlock::ToolUse { input, .. } | ContentBlock::NamespacedToolUse { input, .. } => {
                count_json_with_tokenizer(tokenizer, input)
            }
            ContentBlock::ProviderItem { data, .. } => count_json_with_tokenizer(tokenizer, data),
            ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
            ContentBlock::File { source } => source
                .file_data
                .as_ref()
                .map_or(IMAGE_TOKEN_ESTIMATE, |data| {
                    count_tokens_with_tokenizer(tokenizer, data)
                }),
        })
        .sum();
    u32_from_usize_saturating(total)
}

#[must_use]
pub fn estimate_tool_tokens(tools: &Value, model_id: &str) -> u32 {
    let tokenizer = tokenizer_for_model(model_id);
    u32_from_usize_saturating(count_json_with_tokenizer(tokenizer, tools))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::{HashMap, VecDeque};
    use std::future;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use n00n_providers::provider::{BoxFuture, Provider};
    use n00n_providers::{
        ContentBlock, ImageMediaType, ImageSource, Message, Model, ProviderEvent, RequestOptions,
        Role, StopReason, StreamResponse, TokenUsage,
    };

    use n00n_storage::sessions::TranscriptEntry;
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::permissions::{PermissionAnswer, PermissionManager};
    use crate::tools::registry::ToolInvocation;
    use crate::tools::{
        DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, LocalToolFn, ParseError, Tool,
        ToolContext, ToolRegistry, ToolSource,
    };
    use crate::{Envelope, ToolOutput};
    use serde_json::json;

    #[test]
    fn plan_mode_hides_code_execution_but_keeps_research_tools() {
        let mut tools = json!([
            {"name": "code_execution"},
            {"name": "codegraph"},
            {"name": "server__search"}
        ]);

        filter_tools_for_mode(&mut tools, &AgentMode::Plan("plan.md".into()));

        let names: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|definition| definition["name"].as_str())
            .collect();
        assert_eq!(names, ["codegraph", "server__search"]);
    }

    #[test]
    fn dynamic_mcp_filter_includes_tool_search_with_mcp() {
        let mut history = History::new(Vec::new());
        let (mut agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        agent.tool_filter = ToolFilter::Only(vec!["read".into()]);
        agent = agent.with_mcp(Some(mcp)).with_dynamic_mcp_tools(false);

        let effective_filter = agent.effective_tool_filter();
        assert!(effective_filter.matches("tool_search"));
        assert!(effective_filter.matches("read"));
        assert!(!effective_filter.matches("write"));
        assert!(!effective_filter.matches("srv__fetch_issue"));
    }

    #[test]
    fn dynamic_mcp_filter_keeps_disabled_tool_search_blocked() {
        let mut history = History::new(Vec::new());
        let (mut agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        let mut config = (*agent.config).clone();
        config
            .disabled_tools
            .push(crate::mcp::TOOL_SEARCH_TOOL_NAME.into());
        agent.config = Arc::new(config);
        agent = agent.with_mcp(Some(mcp)).with_dynamic_mcp_tools(true);

        assert!(!agent.effective_tool_filter().matches("tool_search"));
    }

    #[test]
    fn dynamic_mcp_filter_keeps_disabled_tools_blocked() {
        const DISABLED_MCP_TOOL: &str = "srv__fetch_issue";

        let mut history = History::new(Vec::new());
        let (mut agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        let mut config = (*agent.config).clone();
        config.disabled_tools.push(DISABLED_MCP_TOOL.into());
        agent.config = Arc::new(config);
        agent.tool_filter = ToolFilter::Only(vec![crate::mcp::TOOL_SEARCH_TOOL_NAME.into()]);
        agent = agent
            .with_mcp(Some(mcp.clone()))
            .with_dynamic_mcp_tools(true);

        mcp.search_tools("issue").unwrap();

        assert!(!agent.effective_tool_filter().matches(DISABLED_MCP_TOOL));
    }

    #[test]
    fn dynamic_mcp_filter_includes_loaded_tools_with_flag() {
        let mut history = History::new(Vec::new());
        let (mut agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        agent.tool_filter = ToolFilter::Only(vec!["read".into()]);
        agent = agent
            .with_mcp(Some(mcp.clone()))
            .with_dynamic_mcp_tools(true);

        mcp.search_tools("issue").unwrap();
        let effective_filter = agent.effective_tool_filter();
        assert!(effective_filter.matches("tool_search"));
        assert!(effective_filter.matches("srv__fetch_issue"));
        assert!(effective_filter.matches("read"));
        assert!(!effective_filter.matches("write"));
    }

    #[test]
    fn estimate_message_tokens_empty_is_zero() {
        assert_eq!(estimate_message_tokens(&[], ""), 0);
    }

    const COST_EPSILON: f64 = 1e-12;

    #[test]
    fn estimate_message_tokens_counts_content_blocks() {
        let messages = vec![Message::user("hello world".into())];
        let tokens = estimate_message_tokens(&messages, "");
        assert!(
            tokens >= 2,
            "expected at least two tokens for two words, got {tokens}"
        );
    }

    #[test]
    fn estimate_tool_tokens_counts_json() {
        let tools = serde_json::json!([{"name": "skill", "description": "A tool"}]);
        let tokens = estimate_tool_tokens(&tools, "");
        assert!(tokens > 0, "expected positive token count for tools");
    }

    #[test]
    fn estimate_tool_tokens_empty_array_is_nonzero() {
        let tools = json!([]);
        let tokens = estimate_tool_tokens(&tools, "");
        assert!(tokens > 0, "empty array JSON still has token count");
    }

    #[test]
    fn history_replay_scope_reports_reason_tokens_and_cost() {
        let scope = history_replay_scope(
            HistoryReplayReason::ContinuationNotFound,
            &[Message::user("restored context".into())],
            &System::from("system"),
            &json!([{"name": "read"}]),
            &default_model(),
            false,
        );

        assert!(scope.contains("saved continuation was not found"));
        assert!(scope.contains("input tokens"));
        assert!(scope.contains("before cache discounts"));
    }

    #[test]
    fn history_replay_requires_an_interactive_approval_channel() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("restored".into())]);
            let (agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);

            let error = agent
                .approve_history_replay(HistoryReplayReason::ContinuationUnavailable)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                AgentError::Config { message }
                    if message.contains("blocked without explicit approval")
                        && message.contains("input tokens")
            ));
        });
    }

    #[test]
    fn ambiguous_replay_cancellation_is_not_a_denial() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("before".into())]);
            let (agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let (_response_tx, response_rx) = flume::unbounded::<String>();
            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();
            let agent = agent
                .with_cancel(cancel)
                .with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));

            let result = agent.approve_ambiguous_request_replay(None).await;

            assert!(
                matches!(result, Err(AgentError::Cancelled)),
                "expected cancellation, got {result:?}"
            );
        });
    }

    #[test]
    fn history_replay_accepts_explicit_user_approval() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("restored".into())]);
            let (agent, event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let (response_tx, response_rx) = flume::unbounded();
            let agent = agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));
            response_tx
                .send(PermissionAnswer::AllowOnce.encode())
                .unwrap();

            agent
                .approve_history_replay(HistoryReplayReason::ContinuationNotFound)
                .await
                .unwrap();

            let event = event_rx.recv().unwrap();
            assert!(matches!(
                event.event,
                AgentEvent::PermissionRequest { tool, scopes, .. }
                    if tool == ToolKey::native(HISTORY_REPLAY_TOOL)
                        && scopes[0].contains("saved continuation was not found")
            ));
        });
    }

    #[test]
    fn history_replay_propagates_closed_approval_channel() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("restored".into())]);
            let (agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let (response_tx, response_rx) = flume::unbounded::<String>();
            drop(response_tx);
            let agent = agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));

            let error = agent
                .approve_history_replay(HistoryReplayReason::ContinuationNotFound)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                AgentError::Config { message }
                    if message.contains(HISTORY_REPLAY_CHANNEL_CLOSED_MESSAGE)
            ));
        });
    }

    #[test]
    fn ambiguous_request_replay_requires_an_interactive_approval_channel() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let metadata = RequestDeliveryMetadata {
                phase: RequestDeliveryPhase::SentAwaitingAcceptance,
                response_id: None,
                idempotency_key: None,
                close_code: None,
                close_reason: None,
                emitted_event: false,
            };

            assert!(
                !agent
                    .approve_ambiguous_request_replay(Some(&metadata))
                    .await
                    .unwrap()
            );
        });
    }

    #[test]
    fn yolo_mode_does_not_auto_approve_ambiguous_request_replay() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);
            agent.permissions.set_yolo(true);

            assert!(!agent.approve_ambiguous_request_replay(None).await.unwrap());
        });
    }

    #[test]
    fn ambiguous_request_replay_propagates_closed_approval_channel() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let (response_tx, response_rx) = flume::unbounded::<String>();
            drop(response_tx);
            let agent = agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));

            let error = agent
                .approve_ambiguous_request_replay(None)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                AgentError::Config { message }
                    if message.contains(AMBIGUOUS_REPLAY_CHANNEL_CLOSED_MESSAGE)
            ));
        });
    }

    #[test]
    fn ambiguous_request_replay_accepts_explicit_user_approval() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let (response_tx, response_rx) = flume::unbounded();
            let agent = agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));
            response_tx
                .send(PermissionAnswer::AllowOnce.encode())
                .unwrap();
            let metadata = RequestDeliveryMetadata {
                phase: RequestDeliveryPhase::SentAwaitingAcceptance,
                response_id: None,
                idempotency_key: None,
                close_code: None,
                close_reason: None,
                emitted_event: false,
            };

            assert!(
                agent
                    .approve_ambiguous_request_replay(Some(&metadata))
                    .await
                    .unwrap()
            );

            let event = event_rx.recv().unwrap();
            assert!(matches!(
                event.event,
                AgentEvent::PermissionRequest { tool, scopes, .. }
                    if tool == ToolKey::native(AMBIGUOUS_REPLAY_TOOL)
                        && scopes[0].contains("duplicate output or charges")
            ));
        });
    }

    #[test]
    fn approved_ambiguous_request_is_replayed_once() {
        smol::block_on(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = AmbiguousProvider {
                calls: Arc::clone(&calls),
                failures: 1,
                emits_output: true,
            };
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent_with_registry(
                provider,
                &mut history,
                AgentConfig::default(),
                Arc::new(ToolRegistry::new()),
            );
            let (response_tx, response_rx) = flume::unbounded();
            response_tx
                .send(PermissionAnswer::AllowOnce.encode())
                .unwrap();
            agent = agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));

            agent.run(default_input()).await.unwrap();

            assert_eq!(calls.load(Ordering::Relaxed), 2);
            let events = drain_events(&event_rx);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event.event,
                        AgentEvent::PermissionRequest { ref tool, .. }
                            if *tool == ToolKey::native(AMBIGUOUS_REPLAY_TOOL)
                    ))
                    .count(),
                1
            );
            let stale_index = events
                .iter()
                .position(|event| {
                    matches!(
                        &event.event,
                        AgentEvent::TextDelta { text } if text == "stale"
                    )
                })
                .unwrap();
            let reset_index = events
                .iter()
                .position(|event| {
                    matches!(
                        &event.event,
                        AgentEvent::Retry {
                            attempt: 1,
                            message,
                            delay_ms: 0,
                        } if message == AMBIGUOUS_REPLAY_RESET_MESSAGE
                    )
                })
                .unwrap();
            assert!(stale_index < reset_index);
        });
    }

    #[test]
    fn ambiguous_request_without_output_still_requires_approval() {
        smol::block_on(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = AmbiguousProvider {
                calls: Arc::clone(&calls),
                failures: 1,
                emits_output: false,
            };
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent_with_registry(
                provider,
                &mut history,
                AgentConfig::default(),
                Arc::new(ToolRegistry::new()),
            );

            let (response_tx, response_rx) = flume::unbounded();
            response_tx
                .send(PermissionAnswer::AllowOnce.encode())
                .unwrap();
            agent = agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));

            agent.run(default_input()).await.unwrap();

            assert_eq!(calls.load(Ordering::Relaxed), 2);
            assert_eq!(
                drain_events(&event_rx)
                    .iter()
                    .filter(|event| matches!(
                        event.event,
                        AgentEvent::PermissionRequest { ref tool, .. }
                            if *tool == ToolKey::native(AMBIGUOUS_REPLAY_TOOL)
                    ))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn ambiguous_request_is_not_replayed_twice() {
        smol::block_on(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = AmbiguousProvider {
                calls: Arc::clone(&calls),
                failures: 2,
                emits_output: true,
            };
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent_with_registry(
                provider,
                &mut history,
                AgentConfig::default(),
                Arc::new(ToolRegistry::new()),
            );
            let (response_tx, response_rx) = flume::unbounded();
            response_tx
                .send(PermissionAnswer::AllowOnce.encode())
                .unwrap();
            agent = agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(response_rx)));

            let error = agent.run(default_input()).await.unwrap_err();

            assert!(matches!(error, AgentError::RequestSent { .. }));
            assert_eq!(calls.load(Ordering::Relaxed), 2);
            let events = drain_events(&event_rx);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event.event,
                        AgentEvent::PermissionRequest { ref tool, .. }
                            if *tool == ToolKey::native(AMBIGUOUS_REPLAY_TOOL)
                    ))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn context_size_additions_use_saturating_add() {
        let context_size: u32 = u32::MAX - 100;
        let additional: u32 = 200;
        let result = context_size.saturating_add(additional);
        assert_eq!(result, u32::MAX);
    }

    #[test]
    fn estimate_message_tokens_counts_each_content_block() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::Image {
                    source: ImageSource {
                        media_type: ImageMediaType::Png,
                        data: Arc::from("data"),
                        detail: None,
                        file_id: None,
                        url: None,
                    },
                },
            ],
            ..Default::default()
        }];
        let tokens = estimate_message_tokens(&messages, "");
        assert!(
            tokens >= u32_from_usize_saturating(IMAGE_TOKEN_ESTIMATE),
            "image blocks should add {IMAGE_TOKEN_ESTIMATE} tokens"
        );
    }

    #[test]
    fn estimate_message_tokens_counts_inline_file_data() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::File {
                source: n00n_providers::FileSource {
                    file_data: Some("large inline attachment ".repeat(500)),
                    ..Default::default()
                },
            }],
            ..Default::default()
        }];
        let tokens = estimate_message_tokens(&messages, "");
        assert!(tokens > u32_from_usize_saturating(IMAGE_TOKEN_ESTIMATE));
    }

    struct AmbiguousProvider {
        calls: Arc<AtomicUsize>,
        failures: usize,
        emits_output: bool,
    }

    impl Provider for AmbiguousProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a System,
            _: &'a Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let call = self.calls.fetch_add(1, Ordering::Relaxed);
                if call < self.failures {
                    if self.emits_output {
                        event_tx
                            .send(ProviderEvent::TextDelta {
                                text: "stale".into(),
                            })
                            .unwrap();
                    }
                    return Err(AgentError::RequestSent {
                        message: "WebSocket connection reset".into(),
                        metadata: Some(RequestDeliveryMetadata {
                            phase: RequestDeliveryPhase::SentAwaitingAcceptance,
                            response_id: None,
                            idempotency_key: None,
                            close_code: None,
                            close_reason: None,
                            emitted_event: self.emits_output,
                        }),
                    });
                }
                Ok(text_response(StopReason::EndTurn))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<n00n_providers::ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[test]
    fn estimate_message_tokens_counts_thinking_and_signature() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "thinking text".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "redacted".into(),
                },
            ],
            ..Default::default()
        }];
        let tokens = estimate_message_tokens(&messages, "");
        assert!(
            tokens > 0,
            "thinking and redacted blocks should contribute tokens"
        );
    }

    #[test]
    fn estimate_tool_tokens_empty_array_costs_one() {
        assert_eq!(estimate_tool_tokens(&serde_json::json!([]), ""), 1);
    }

    struct MockInterruptSource {
        commands: Mutex<VecDeque<ExtractedCommand>>,
    }

    impl MockInterruptSource {
        fn new(commands: Vec<ExtractedCommand>) -> Arc<Self> {
            Arc::new(Self {
                commands: Mutex::new(commands.into()),
            })
        }
    }

    impl InterruptSource for MockInterruptSource {
        fn poll(&self, _: InterruptPoint) -> Option<ExtractedCommand> {
            self.commands.lock().unwrap().pop_front()
        }
    }

    struct MockProvider {
        responses: Mutex<Vec<StreamResponse>>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        tool_requests: Arc<Mutex<Vec<Value>>>,
        cancel_on_request: Option<usize>,
        calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Arc::new(Mutex::new(Vec::new())),
                tool_requests: Arc::new(Mutex::new(Vec::new())),
                cancel_on_request: None,
                calls: AtomicUsize::new(0),
            }
        }

        fn recording(responses: Vec<StreamResponse>) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
            let requests = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    responses: Mutex::new(responses),
                    requests: Arc::clone(&requests),
                    tool_requests: Arc::new(Mutex::new(Vec::new())),
                    cancel_on_request: None,
                    calls: AtomicUsize::new(0),
                },
                requests,
            )
        }

        fn cancel_on_request(responses: Vec<StreamResponse>, request: usize) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Arc::new(Mutex::new(Vec::new())),
                tool_requests: Arc::new(Mutex::new(Vec::new())),
                cancel_on_request: Some(request),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a System,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let request = self.calls.fetch_add(1, Ordering::Relaxed);
                if self.cancel_on_request == Some(request) {
                    return Err(AgentError::Cancelled);
                }
                self.requests.lock().unwrap().push(messages.to_vec());
                self.tool_requests.lock().unwrap().push(tools.clone());
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                Ok(responses.remove(0))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<n00n_providers::ModelInfo>, AgentError>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
        }
    }

    fn empty_response() -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
        }
    }

    fn thinking_response() -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    thinking: "done".into(),
                    signature: None,
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
        }
    }

    fn make_agent(
        provider: MockProvider,
        history: &mut History,
    ) -> (Agent<'_>, flume::Receiver<Envelope>) {
        make_agent_with_config(provider, history, AgentConfig::default())
    }

    fn make_agent_with_registry<P: Provider + 'static>(
        provider: P,
        history: &mut History,
        config: AgentConfig,
        registry: Arc<crate::tools::ToolRegistry>,
    ) -> (Agent<'_>, flume::Receiver<Envelope>) {
        let (raw_tx, event_rx) = flume::unbounded();
        let vars = crate::template::env_vars();
        let filter = ToolFilter::from_config(&config, &default_model(), &[]);
        let tools = registry.definitions(
            &vars,
            &crate::tools::DescriptionContext {
                filter: &filter,
                audience: ToolAudience::MAIN,
                workflow: false,
            },
            false,
        );
        let mut agent = Agent::new(
            AgentParams {
                provider: Arc::new(provider),
                model: default_model(),
                config: Arc::new(config),
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::new(PermissionManager::new(
                    n00n_config::PermissionsConfig {
                        default: n00n_config::DefaultEffect::Allow,
                        rules: vec![],
                        ..Default::default()
                    },
                    std::path::PathBuf::from("/tmp"),
                )),
                identity: None,
                timeouts: n00n_providers::Timeouts::default(),
                openai_options: OpenAiOptions::default(),
                file_tracker: FileReadTracker::fresh(),
                prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),

                subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                registry,
                audience: ToolAudience::MAIN,
                state_revision: Some(0),
            },
            AgentRunParams {
                history,
                system: System::from("system"),
                event_tx: EventSender::new(raw_tx, 0),
                tools,
                tool_filter: filter,
            },
        );
        agent.test_compaction_hooks = TestCompactionHooks::Disabled;
        (agent, event_rx)
    }

    #[test]
    fn sequential_auto_compactions_allocate_distinct_revisions() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("one".into())]);
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]),
                &mut history,
            );

            agent.do_compact().await.unwrap();
            agent.history.push(Message::user("two".into()));
            agent.do_compact().await.unwrap();

            assert_eq!(agent.state_revision(), Some(2));
            assert!(matches!(
                agent.history.transcript(),
                [TranscriptEntry::Compaction {
                    entries,
                    state_revision: Some(2),
                    ..
                }, TranscriptEntry::GeneratedMessage(_), TranscriptEntry::GeneratedMessage(_), TranscriptEntry::Message(_)]
                    if matches!(entries.as_slice(), [TranscriptEntry::Compaction { state_revision: Some(1), .. }, ..])
            ));
        });
    }

    #[test]
    fn shared_compaction_revision_is_allocated_after_provider_response() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("one".into())]);
            let (provider, requests) =
                MockProvider::recording(vec![text_response(StopReason::EndTurn)]);
            let requests_at_allocation = Arc::clone(&requests);
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_state_revision_allocator(Arc::new(move || {
                assert_eq!(requests_at_allocation.lock().unwrap().len(), 1);
                Ok(7)
            }));

            agent.do_compact().await.unwrap();

            assert_eq!(agent.history.latest_state_revision(), Some(7));
            let continue_prompt = agent.history.as_slice().last().unwrap();
            assert!(matches!(continue_prompt.role, Role::User));
            assert_eq!(continue_prompt.display_text.as_deref(), Some(""));
            assert!(matches!(
                continue_prompt.content.as_slice(),
                [ContentBlock::Text { text }] if text == CONTINUE_AFTER_COMPACT
            ));
            let events = drain_events(&event_rx);
            let compacting = events
                .iter()
                .position(|envelope| matches!(envelope.event, AgentEvent::AutoCompacting))
                .unwrap();
            let done = events
                .iter()
                .position(|envelope| matches!(envelope.event, AgentEvent::CompactionDone { .. }))
                .unwrap();
            assert!(compacting < done);
        });
    }

    #[test]
    fn failed_compaction_emits_terminal_status() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("one".into())]);
            let (mut agent, event_rx) =
                make_agent(MockProvider::cancel_on_request(Vec::new(), 0), &mut history);

            agent.do_compact_with_status().await.unwrap_err();

            let events = drain_events(&event_rx);
            assert!(
                events
                    .iter()
                    .any(|envelope| matches!(envelope.event, AgentEvent::AutoCompacting))
            );
            assert!(
                events
                    .iter()
                    .any(|envelope| matches!(envelope.event, AgentEvent::AutoCompactFailed { .. }))
            );
        });
    }

    #[test]
    fn compaction_checkpoint_completes_before_boundary_events_are_sent() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("one".into())]);
            let (agent, event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let checkpointed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let checkpointed_in_hook = Arc::clone(&checkpointed);
            let event_rx_in_hook = event_rx.clone();
            let mut agent = agent.with_compaction_checkpoint(move |history, revision| {
                assert_eq!(revision, 1);
                assert_eq!(history.latest_state_revision(), Some(1));
                assert!(!event_rx_in_hook.try_iter().any(|envelope| matches!(
                    envelope.event,
                    AgentEvent::TurnComplete(_) | AgentEvent::CompactionDone { .. }
                )));
                checkpointed_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            });

            agent.do_compact().await.unwrap();

            assert!(checkpointed.load(std::sync::atomic::Ordering::SeqCst));
            assert!(event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::CompactionDone {
                    state_revision: Some(1)
                }
            )));
        });
    }

    #[test]
    fn failed_compaction_checkpoint_is_not_acknowledged() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("one".into())]);
            let charged = TokenUsage {
                input: 100,
                output: 50,
                ..Default::default()
            };
            let mut response = text_response(StopReason::EndTurn);
            response.usage = charged;
            let (agent, event_rx) = make_agent(MockProvider::new(vec![response]), &mut history);
            let mut agent = agent.with_compaction_checkpoint(|_, _| Err("disk full".into()));

            let error = agent.do_compact().await.unwrap_err();

            assert!(error.to_string().contains("disk full"));
            assert_eq!(agent.history.latest_state_revision(), None);
            assert_eq!(agent.history.len(), 1);
            assert_eq!(agent.total_usage(), charged);
            assert!(
                !event_rx
                    .try_iter()
                    .any(|envelope| matches!(envelope.event, AgentEvent::CompactionDone { .. }))
            );
        });
    }

    #[test]
    fn auto_compaction_revision_overflow_fails_without_consuming_state() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("one".into())]);
            let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            agent.state_revision = Some(u64::MAX);
            let original_transcript_len = agent.history.transcript().len();

            let error = agent.do_compact().await.unwrap_err();

            assert!(matches!(
                error,
                AgentError::Config { message }
                    if message == "compaction state revision overflow"
            ));
            assert_eq!(agent.state_revision(), Some(u64::MAX));
            assert_eq!(agent.history.transcript().len(), original_transcript_len);
            assert!(matches!(
                agent.history.transcript(),
                [TranscriptEntry::Message(_)]
            ));
        });
    }

    #[test]
    fn tool_context_preserves_agent_session_identity() {
        let mut history = History::new(Vec::new());
        let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let identity = SessionIdentity::root(SessionRef::generate());
        agent.identity = Some(identity.clone());

        let ctx = agent.tool_context();

        assert_eq!(ctx.identity, Some(identity));
    }

    fn make_agent_with_config(
        provider: MockProvider,
        history: &mut History,
        config: AgentConfig,
    ) -> (Agent<'_>, flume::Receiver<Envelope>) {
        make_agent_with_registry(
            provider,
            history,
            config,
            Arc::new(crate::tools::ToolRegistry::new()),
        )
    }

    #[test]
    fn discovery_results_activate_canonical_tools_and_namespaces() {
        let mut history = History::new(Vec::new());
        let (mut agent, _events) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let search_result = ToolDoneEvent {
            id: "search".to_owned(),
            tool: Arc::from("tool_search"),
            output: ToolOutput::Plain(
                r#"[{"name":"fetch_url","namespace":"web","description":"Fetch URL"}]"#.into(),
            ),
            is_error: false,
            annotation: None,
            written_path: None,
        };
        let namespace_result = ToolDoneEvent {
            id: "namespace".to_owned(),
            tool: Arc::from("load_namespace"),
            output: ToolOutput::Plain(
                r#"{"namespace":"knowledge","tools":["load_skill","use_memory"]}"#.into(),
            ),
            is_error: false,
            annotation: None,
            written_path: None,
        };

        assert!(agent.apply_tool_search_results(&[search_result, namespace_result]));
        {
            let active = agent
                .active_tools
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(active.names.contains("fetch_url"));
            assert!(active.namespaces.contains("knowledge"));
        }
    }

    #[test]
    fn malformed_discovery_results_do_not_change_active_tools() {
        let mut history = History::new(Vec::new());
        let (mut agent, _events) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let malformed = ToolDoneEvent {
            id: "search".to_owned(),
            tool: Arc::from("search_tools"),
            output: ToolOutput::Plain("not json".into()),
            is_error: false,
            annotation: None,
            written_path: None,
        };

        assert!(!agent.apply_tool_search_results(&[malformed]));
        {
            let active = agent
                .active_tools
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(active.names.is_empty());
            assert!(active.namespaces.is_empty());
        }
    }
    fn default_input() -> AgentInput {
        AgentInput {
            message: "hello".into(),
            mode: AgentMode::Build,
            images: Vec::new(),
            preamble: Vec::new(),
            thinking: n00n_providers::ThinkingConfig::default(),
            fast: false,
            workflow: false,
            control: false,
            prompt: None,
            plan_path: None,
        }
    }

    #[test]
    fn queued_interrupt_validation_rejects_empty_text_without_images() {
        let mut input = default_input();
        input.message.clear();
        assert!(matches!(
            validate_input_message(&input),
            Err(AgentError::Config { .. })
        ));

        input.images.push(ImageSource::new(
            n00n_providers::ImageMediaType::Png,
            std::sync::Arc::from("dGVzdA=="),
        ));
        assert!(validate_input_message(&input).is_ok());
    }

    #[test]
    fn run_preserves_control_on_initial_history_message() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let mut input = default_input();
            input.control = true;
            agent.run(input).await.unwrap();
            let first = history
                .as_slice()
                .first()
                .expect("history should contain user message");
            assert!(
                first.control,
                "initial Agent::run message must retain input.control"
            );
        });
    }

    #[test]
    fn explicit_base_filter_allows_mcp_tools_loaded_after_search() {
        let mut history = History::new(Vec::new());
        let (mut agent, _) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        agent.tool_filter = ToolFilter::Only(vec![
            "read".into(),
            crate::mcp::TOOL_SEARCH_TOOL_NAME.into(),
        ]);
        agent = agent
            .with_mcp(Some(mcp.clone()))
            .with_dynamic_mcp_tools(true);

        assert!(agent.effective_tool_filter().matches("tool_search"));
        assert!(!agent.effective_tool_filter().matches("write"));
        assert!(!agent.effective_tool_filter().matches("srv__fetch_issue"));

        mcp.search_tools("issue").unwrap();
        let effective_filter = agent.effective_tool_filter();
        let mut definitions = serde_json::json!([
            {"name": "read"},
            {"name": "write"},
            {"name": "srv__fetch_issue"}
        ]);
        filter_provider_tools(&mut definitions, &effective_filter, &AgentMode::Build);

        let names: Vec<_> = definitions
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|definition| definition["name"].as_str())
            .collect();
        assert_eq!(names, ["read", "srv__fetch_issue"]);
    }
    fn drain_events(rx: &flume::Receiver<Envelope>) -> Vec<Envelope> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    async fn run_agent(provider: MockProvider) -> (u32, Option<StopReason>) {
        let mut history = History::new(Vec::new());
        let (mut agent, event_rx) = make_agent(provider, &mut history);
        let _ = agent.run(default_input()).await;
        drain_events(&event_rx)
            .into_iter()
            .find_map(|e| match e.event {
                AgentEvent::Done {
                    num_turns,
                    stop_reason,
                    ..
                } => Some((num_turns, stop_reason)),
                _ => None,
            })
            .expect("expected Done event")
    }

    fn has_event(events: &[Envelope], predicate: impl Fn(&AgentEvent) -> bool) -> bool {
        events.iter().any(|e| predicate(&e.event))
    }

    fn has_interrupt_in_history(history: &[Message]) -> bool {
        history.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.contains("<user-interrupt>") || text.contains("<control-interrupt>")),
            )
        })
    }

    fn tool_call_response(tool_name: &str, tool_id: &str) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: tool_id.into(),
                    name: tool_name.into(),
                    input: if tool_name == "fusion_delegate" {
                        serde_json::json!({
                            "description": "Implement parser fix",
                            "goal": "Implement the parser fix and add focused tests",
                            "constraints": "Keep the change scoped to parser code",
                            "definition_of_done": "Run cargo test",
                        })
                    } else {
                        serde_json::json!({"pattern": "*.nonexistent_test_xyz", "path": "/tmp"})
                    },
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
        }
    }

    fn small_context_model(context_window: u32, max_output_tokens: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model.max_output_tokens = Some(max_output_tokens);
        model
    }

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(
            matches!(&last.content[0], ContentBlock::Text { text } if text == "[Cancelled by user]")
        );
    }

    #[test_case(&[StopReason::EndTurn],                                                     1, Some(StopReason::EndTurn)  ; "end_turn_completes")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn],                                 2, Some(StopReason::EndTurn)  ; "max_tokens_continues")]
    #[test_case(&[StopReason::MaxTokens, StopReason::MaxTokens, StopReason::MaxTokens, StopReason::MaxTokens], 4, Some(StopReason::MaxTokens) ; "max_tokens_gives_up_after_limit")]
    fn turn_counting(stops: &[StopReason], expected_turns: u32, expected_stop: Option<StopReason>) {
        smol::block_on(async {
            let responses: Vec<_> = stops.iter().map(|s| text_response(*s)).collect();
            let provider = MockProvider::new(responses);
            let (turns, stop_reason) = run_agent(provider).await;
            assert_eq!(turns, expected_turns);
            assert_eq!(stop_reason, expected_stop);
        });
    }

    #[test]
    fn max_tokens_continuation_adds_incremental_prompt() {
        smol::block_on(async {
            let (provider, requests) = MockProvider::recording(vec![
                text_response(StopReason::MaxTokens),
                text_response(StopReason::EndTurn),
            ]);
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(provider, &mut history);

            agent.run(default_input()).await.unwrap();

            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(matches!(
                requests[1].last(),
                Some(message)
                    if message.first_text_content() == Some(MAX_TOKENS_CONTINUE_PROMPT)
            ));
        });
    }

    #[test]
    fn fusion_disabled_preserves_baseline_run_identity_output_and_cost() {
        smol::block_on(async {
            let charged = TokenUsage {
                input: 100,
                output: 25,
                cache_creation: 10,
                cache_read: 5,
            };
            let mut response = text_response(StopReason::EndTurn);
            response.usage = charged;
            let (provider, requests) = MockProvider::recording(vec![response]);
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(provider, &mut history);
            let provider_before = Arc::clone(&agent.provider);
            let model_before = agent.model.id.clone();

            assert!(agent.fusion_state.is_none());
            assert!(
                !agent.tools.as_array().unwrap().iter().any(|tool| {
                    tool.get("name").and_then(Value::as_str) == Some("delegate_fusion")
                }),
                "disabled Fusion must not advertise delegate_fusion"
            );

            agent.run(default_input()).await.unwrap();
            let expected_cost = charged.cost(&default_model().pricing, false);
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert_eq!(agent.model.id, model_before);
            assert!(Arc::ptr_eq(&agent.provider, &provider_before));
            assert_eq!(agent.total_usage(), charged);
            assert!((agent.total_cost() - expected_cost).abs() < COST_EPSILON);
            assert_eq!(history.len(), 2);
            assert_eq!(history.as_slice()[1].first_text_content(), Some("response"));

            let events = drain_events(&event_rx);
            assert!(events.iter().all(|envelope| {
                !serde_json::to_string(&envelope.event)
                    .unwrap()
                    .contains("fusion_phase")
            }));
            let done = events
                .into_iter()
                .find_map(|envelope| match envelope.event {
                    AgentEvent::Done { fusion, .. } => Some(fusion),
                    _ => None,
                });
            assert!(matches!(done, Some(None)));
        });
    }

    #[test]
    fn disabled_hallucinated_delegate_is_denied_without_subagent_launch() {
        smol::block_on(async {
            let (provider, requests) = MockProvider::recording(vec![
                tool_call_response("fusion_delegate", "delegate-1"),
                text_response(StopReason::EndTurn),
            ]);
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(provider, &mut history);

            agent.run(default_input()).await.unwrap();

            assert_eq!(requests.lock().unwrap().len(), 2, "lead continues normally");
            assert!(agent.fusion_state.is_none());
            drop(agent);
            let tool_result = history
                .as_slice()
                .iter()
                .flat_map(|message| &message.content)
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => Some((content, is_error)),
                    _ => None,
                })
                .expect("denial is returned to the lead as a tool result");
            assert!(*tool_result.1);
            assert!(
                tool_result.0.contains("unknown tool")
                    || tool_result.0.contains("not available")
                    || tool_result.0.contains("unavailable")
                    || tool_result.0.contains("disabled"),
                "unexpected denial: {}",
                tool_result.0
            );
        });
    }

    fn fusion_enabled_config() -> AgentConfig {
        let mut config = AgentConfig::default();
        config.fusion.enabled = true;
        config
    }

    fn message_text(messages: &[Message]) -> String {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } | ContentBlock::ToolResult { content: text, .. } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn fusion_routing_switches_lane_without_replacing_lead_model_or_provider() {
        let mut history = History::new(Vec::new());
        let (mut agent, _event_rx) = make_agent_with_config(
            MockProvider::new(Vec::new()),
            &mut history,
            fusion_enabled_config(),
        );
        let model_before = agent.model.id.clone();
        let provider_before = Arc::clone(&agent.provider);

        agent.apply_fusion_route(FusionRoute::Switch(FusionLane::Sidekick));

        assert_eq!(agent.model.id, model_before);
        assert!(Arc::ptr_eq(&agent.provider, &provider_before));
        assert_eq!(
            agent.fusion_state.as_ref().unwrap().lane(),
            FusionLane::Sidekick
        );
    }

    #[test]
    fn main_agent_usage_is_always_charged_to_lead_lane() {
        let mut history = History::new(Vec::new());
        let (mut agent, _event_rx) = make_agent_with_config(
            MockProvider::new(Vec::new()),
            &mut history,
            fusion_enabled_config(),
        );
        agent.apply_fusion_route(FusionRoute::Switch(FusionLane::Sidekick));
        let usage = TokenUsage {
            input: 10,
            output: 2,
            ..Default::default()
        };
        agent.record_usage(usage, 0.25);
        let state = agent.fusion_state.as_ref().unwrap();
        assert_eq!(state.lead_usage, usage);
        assert!((state.lead_cost - 0.25).abs() < COST_EPSILON);
        assert_eq!(state.sidekick_usage, TokenUsage::default());
        assert!(state.sidekick_cost.abs() < COST_EPSILON);
    }

    #[test_case("task" ; "task")]
    #[test_case("workflow" ; "workflow")]
    fn recoverable_subagent_failure_reaches_next_provider_turn(tool_name: &str) {
        smol::block_on(async {
            const FAILURE: &str = "sub-agent error: provider unavailable";
            let (provider, requests) = MockProvider::recording(vec![
                tool_call_response(tool_name, "subagent-1"),
                text_response(StopReason::EndTurn),
            ]);
            let calls = Arc::new(AtomicUsize::new(0));
            let call_counter = Arc::clone(&calls);
            let mut local = HashMap::new();
            local.insert(
                tool_name.to_owned(),
                Arc::new(move |_: &Value| {
                    call_counter.fetch_add(1, Ordering::Relaxed);
                    Err(FAILURE.to_owned())
                }) as LocalToolFn,
            );
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_local_tools(Arc::new(local));

            agent.run(default_input()).await.unwrap();

            assert_eq!(calls.load(Ordering::Relaxed), 1);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests[1]
                    .iter()
                    .any(|message| message.content.iter().any(|block| {
                        matches!(
                            block,
                            ContentBlock::ToolResult {
                                content,
                                is_error: true,
                                ..
                            } if content == FAILURE
                        )
                    }))
            );
        });
    }

    #[test]
    fn parent_cancellation_after_subagent_start_prevents_next_provider_turn() {
        struct PendingInvocation {
            started: flume::Sender<()>,
        }

        impl ToolInvocation for PendingInvocation {
            fn start_header(&self) -> HeaderFuture {
                HeaderFuture::Ready(HeaderResult::plain("pending subagent".into()))
            }

            fn execute(self: Box<Self>, _ctx: &ToolContext) -> ExecFuture<'_> {
                Box::pin(async move {
                    let _ = self.started.send_async(()).await;
                    future::pending().await
                })
            }
        }

        struct PendingTask {
            started: flume::Sender<()>,
        }

        impl Tool for PendingTask {
            fn name(&self) -> &'static str {
                "task"
            }

            fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
                "pending task".into()
            }

            fn schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }

            fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
                Ok(Box::new(PendingInvocation {
                    started: self.started.clone(),
                }))
            }
        }

        smol::block_on(async {
            let (provider, requests) = MockProvider::recording(vec![
                tool_call_response("task", "task-1"),
                text_response(StopReason::EndTurn),
            ]);
            let (started_tx, started_rx) = flume::bounded(1);
            let registry = ToolRegistry::new();
            let tool: Arc<dyn Tool> = Arc::new(PendingTask {
                started: started_tx,
            });
            registry
                .register(
                    &tool,
                    &ToolSource::Lua {
                        plugin: "task".into(),
                    },
                )
                .unwrap();
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent_with_registry(
                provider,
                &mut history,
                AgentConfig::default(),
                Arc::new(registry),
            );
            let (trigger, cancel) = CancelToken::new();
            let mut agent = agent.with_cancel(cancel);
            let cancel_after_start = async {
                started_rx.recv_async().await.unwrap();
                trigger.cancel();
            };

            let (result, ()) =
                futures_lite::future::zip(agent.run(default_input()), cancel_after_start).await;

            assert!(matches!(result, Err(AgentError::Cancelled)));
            assert_eq!(requests.lock().unwrap().len(), 1);
        });
    }

    #[test]
    fn successful_delegate_is_followed_by_exactly_one_lead_review_turn() {
        smol::block_on(async {
            let (provider, requests) = MockProvider::recording(vec![
                tool_call_response("fusion_delegate", "delegate-1"),
                text_response(StopReason::EndTurn),
            ]);
            let calls = Arc::new(AtomicUsize::new(0));
            let call_counter = Arc::clone(&calls);
            let mut local = std::collections::HashMap::new();
            local.insert(
                "fusion_delegate".to_owned(),
                Arc::new(move |_: &Value| {
                    call_counter.fetch_add(1, Ordering::Relaxed);
                    Ok("sidekick completed".to_owned())
                }) as crate::tools::LocalToolFn,
            );
            let mut history = History::new(Vec::new());
            let (agent, event_rx) =
                make_agent_with_config(provider, &mut history, fusion_enabled_config());
            let mut agent = agent.with_local_tools(Arc::new(local));
            let mut input = default_input();
            input.message = "grep for TODO markers".into();

            agent.run(input).await.unwrap();

            assert_eq!(calls.load(Ordering::Relaxed), 1);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "one delegation turn and one review turn");
            assert!(
                message_text(&requests[1])
                    .to_ascii_lowercase()
                    .contains("review"),
                "next lead request must contain review instruction: {}",
                message_text(&requests[1])
            );
            assert_eq!(
                agent.model.id,
                default_model().id,
                "lead produces final response"
            );

            let phases: Vec<_> = drain_events(&event_rx)
                .into_iter()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::FusionPhase { phase, .. } => Some(phase),
                    _ => None,
                })
                .collect();
            assert_eq!(
                phases,
                [
                    crate::fusion::FusionPhase::Planning,
                    crate::fusion::FusionPhase::Executing,
                    crate::fusion::FusionPhase::Reviewing,
                    crate::fusion::FusionPhase::Complete
                ]
            );
        });
    }

    #[test_case("generic tool error" ; "generic error")]
    #[test_case("delegate timed out" ; "timeout")]
    #[test_case("model unavailable" ; "model unavailable")]
    #[test_case("delegate cancelled" ; "delegate cancellation")]
    fn delegate_failure_is_followed_by_exactly_one_lead_fallback(error: &str) {
        smol::block_on(async {
            let (provider, requests) = MockProvider::recording(vec![
                tool_call_response("fusion_delegate", "delegate-1"),
                text_response(StopReason::EndTurn),
            ]);
            let calls = Arc::new(AtomicUsize::new(0));
            let call_counter = Arc::clone(&calls);
            let error = error.to_owned();
            let mut local = std::collections::HashMap::new();
            local.insert(
                "fusion_delegate".to_owned(),
                Arc::new(move |_: &Value| {
                    call_counter.fetch_add(1, Ordering::Relaxed);
                    Err(error.clone())
                }) as crate::tools::LocalToolFn,
            );
            let mut history = History::new(Vec::new());
            let (agent, event_rx) =
                make_agent_with_config(provider, &mut history, fusion_enabled_config());
            let mut agent = agent.with_local_tools(Arc::new(local));
            let mut input = default_input();
            input.message = "run cargo test".into();

            agent.run(input).await.unwrap();

            assert_eq!(
                calls.load(Ordering::Relaxed),
                1,
                "delegate is never retried"
            );
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                message_text(&requests[1])
                    .to_ascii_lowercase()
                    .contains("fallback"),
                "next lead request must contain fallback instruction: {}",
                message_text(&requests[1])
            );
            let phases: Vec<_> = drain_events(&event_rx)
                .into_iter()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::FusionPhase { phase, .. } => Some(phase),
                    _ => None,
                })
                .collect();
            assert_eq!(
                phases,
                [
                    crate::fusion::FusionPhase::Planning,
                    crate::fusion::FusionPhase::Executing,
                    crate::fusion::FusionPhase::LeadFallback,
                    crate::fusion::FusionPhase::Complete
                ]
            );
        });
    }
    #[test]
    fn charged_usage_survives_event_delivery_failure() {
        smol::block_on(async {
            let charged = TokenUsage {
                input: 100,
                output: 50,
                cache_creation: 30,
                cache_read: 20,
            };
            let mut response = text_response(StopReason::EndTurn);
            response.usage = charged;
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(vec![response]), &mut history);
            drop(event_rx);

            assert!(agent.run(default_input()).await.is_err());
            assert_eq!(agent.total_usage(), charged);
            let expected_cost = charged.cost(&default_model().pricing, false);
            assert!((agent.total_cost() - expected_cost).abs() < COST_EPSILON);
        });
    }

    #[test]
    fn mixed_model_usage_preserves_per_call_cost() {
        let mut history = History::new(Vec::new());
        let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let main_model = default_model();
        let compact_model = Model::from_spec("anthropic/claude-haiku-4-5").unwrap();
        let main_usage = TokenUsage {
            input: 1_000_000,
            output: 100_000,
            ..Default::default()
        };
        let compact_usage = TokenUsage {
            input: 200_000,
            output: 20_000,
            ..Default::default()
        };
        let main_cost = main_usage.cost(&main_model.pricing, true);
        let compact_cost = compact_usage.cost(&compact_model.pricing, false);

        agent.record_usage(main_usage, main_cost);
        agent.record_usage(compact_usage, compact_cost);

        let mut expected_usage = main_usage;
        expected_usage += compact_usage;
        assert_eq!(agent.total_usage(), expected_usage);
        assert!((agent.total_cost() - (main_cost + compact_cost)).abs() < COST_EPSILON);
        let incorrectly_repriced = agent.total_usage().cost(&main_model.pricing, true);
        assert!(
            (agent.total_cost() - incorrectly_repriced).abs() > COST_EPSILON,
            "mixed usage must not be repriced as one main-model fast request"
        );
    }

    #[test]
    fn response_context_includes_output_tokens() {
        smol::block_on(async {
            let mut response = text_response(StopReason::EndTurn);
            response.usage = TokenUsage {
                input: 100,
                output: 50,
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) =
                make_agent(MockProvider::new(vec![response]), &mut history);

            agent.run(default_input()).await.unwrap();

            assert_eq!(agent.context_size, 150);
        });
    }

    #[test_case(Some(true),  false, true,  true  ; "after_tool_use_turn")]
    #[test_case(Some(false), false, true,  true  ; "after_text_only_turn")]
    #[test_case(Some(false), true,  true,  true  ; "control_after_text_only_turn")]
    #[test_case(None,        false, false, false ; "channel_empty")]
    fn interrupt_handling(
        queued: Option<bool>,
        control: bool,
        expect_consumed: bool,
        expect_injected: bool,
    ) {
        smol::block_on(async {
            let source = if queued.is_some() {
                let mut input = default_input();
                input.control = control;
                Some(MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                    input, 0,
                )]))
            } else {
                None
            };

            let tool_use = queued.unwrap_or_else(|| true);
            let responses = if tool_use {
                vec![
                    tool_call_response("glob", "t1"),
                    text_response(StopReason::EndTurn),
                ]
            } else {
                vec![
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]
            };

            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            if let Some(s) = source {
                agent = agent.with_interrupt_source(s);
            }
            let _ = agent.run(default_input()).await;
            let events = drain_events(&event_rx);

            assert_eq!(
                has_event(&events, |e| matches!(
                    e,
                    AgentEvent::QueueItemConsumed { .. }
                )),
                expect_consumed,
            );
            assert_eq!(
                has_interrupt_in_history(history.as_slice()),
                expect_injected
            );
            assert_eq!(
                history.as_slice().iter().any(|message| message.control),
                expect_injected && control
            );
        });
    }

    #[test_case(
        (0..10).map(|i| Message::user(format!("msg {i}"))).collect(),
        vec![ExtractedCommand::Compact(0)],
        vec![tool_call_response("glob", "t1"), text_response(StopReason::EndTurn), text_response(StopReason::EndTurn)]
        ; "compaction_via_interrupt_source"
    )]
    fn compaction_through_interrupt(
        prior: Vec<Message>,
        commands: Vec<ExtractedCommand>,
        responses: Vec<StreamResponse>,
    ) {
        smol::block_on(async {
            let source = MockInterruptSource::new(commands);

            let mut history = History::new(prior);
            let (agent, _event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let result = agent
                .with_interrupt_source(source)
                .run(default_input())
                .await;

            assert!(result.is_ok());
        });
    }

    #[test]
    fn oversized_initial_context_compacts_before_normal_request() {
        smol::block_on(async {
            let prior = vec![Message::user("x".repeat(680_000))];
            let mut history = History::new(prior);
            let (provider, requests) = MockProvider::recording(vec![
                text_response(StopReason::EndTurn),
                text_response(StopReason::EndTurn),
            ]);
            let (mut agent, event_rx) = make_agent(provider, &mut history);
            agent.model = Arc::new(small_context_model(200_000, 8_192));

            agent.run(default_input()).await.unwrap();

            assert_eq!(requests.lock().unwrap().len(), 2);
            assert!(has_event(&drain_events(&event_rx), |event| matches!(
                event,
                AgentEvent::AutoCompacting
            )));
        });
    }

    #[test_case(true,  170_000, true  ; "enabled_and_over_threshold")]
    #[test_case(true,  150_000, false ; "enabled_but_below_threshold")]
    #[test_case(false, 170_000, false ; "disabled_even_over_threshold")]
    fn try_auto_compact_behavior(enabled: bool, context_size: u32, expected: bool) {
        smol::block_on(async {
            let responses = if expected {
                vec![text_response(StopReason::EndTurn)]
            } else {
                vec![]
            };
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            agent.model = Arc::new(small_context_model(200_000, 8_192));
            agent.auto_compact = enabled;
            agent.context_size = context_size;
            let result = agent.try_auto_compact().await.unwrap();

            assert_eq!(result, expected);
            drop(agent);
            assert_eq!(
                has_event(&drain_events(&event_rx), |e| matches!(
                    e,
                    AgentEvent::AutoCompacting
                )),
                expected,
            );
        });
    }

    #[test]
    fn cancel_token_aborts_during_api_call() {
        smol::block_on(async {
            struct HangingProvider;
            impl Provider for HangingProvider {
                fn stream_message<'a>(
                    &'a self,
                    _: &'a Model,
                    _: &'a [Message],
                    _: &'a System,
                    _: &'a Value,
                    _: &'a flume::Sender<ProviderEvent>,
                    _: RequestOptions,
                    _: Option<&'a SessionRef>,
                ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
                    Box::pin(async {
                        futures_lite::future::pending::<()>().await;
                        unreachable!()
                    })
                }
                fn list_models(
                    &self,
                ) -> BoxFuture<'_, Result<Vec<n00n_providers::ModelInfo>, AgentError>>
                {
                    Box::pin(async { Ok(vec![]) })
                }
            }

            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();

            let (raw_tx, _rx) = flume::unbounded();
            let mut history = History::new(Vec::new());
            let mut agent = Agent::new(
                AgentParams {
                    provider: Arc::new(HangingProvider),
                    model: default_model(),
                    config: Arc::new(AgentConfig::default()),
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::new(PermissionManager::new(
                        n00n_config::PermissionsConfig {
                            default: n00n_config::DefaultEffect::Allow,
                            rules: vec![],
                            ..Default::default()
                        },
                        std::path::PathBuf::from("/tmp"),
                    )),
                    identity: None,
                    timeouts: n00n_providers::Timeouts::default(),
                    openai_options: OpenAiOptions::default(),
                    file_tracker: FileReadTracker::fresh(),
                    prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
                    subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                    registry: Arc::new(crate::tools::ToolRegistry::new()),
                    audience: ToolAudience::MAIN,
                    state_revision: Some(0),
                },
                AgentRunParams {
                    history: &mut history,
                    system: System::from("system"),
                    event_tx: EventSender::new(raw_tx, 0),
                    tools: serde_json::json!([]),
                    tool_filter: ToolFilter::All,
                },
            )
            .with_cancel(cancel);

            let result = agent.run(default_input()).await;
            assert!(matches!(result, Err(AgentError::Cancelled)));
            drop(agent);
            assert_ends_with_cancel_marker(&history);
        });
    }

    #[test]
    fn post_compaction_cancellation_keeps_compaction_transcript_and_cleanup() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("x".repeat(680_000))]);
            let (mut agent, _event_rx) = make_agent(
                MockProvider::cancel_on_request(vec![text_response(StopReason::EndTurn)], 1),
                &mut history,
            );
            agent.model = Arc::new(small_context_model(200_000, 8_192));

            let result = agent.run(default_input()).await;

            assert!(matches!(result, Err(AgentError::Cancelled)));
            drop(agent);
            assert!(
                history
                    .transcript()
                    .iter()
                    .any(|entry| matches!(entry, TranscriptEntry::Compaction { .. }))
            );
            assert_ends_with_cancel_marker(&history);
        });
    }

    #[test]
    fn pre_dispatch_cancellation_rolls_back_without_provider_call() {
        smol::block_on(async {
            let (provider, requests) =
                MockProvider::recording(vec![text_response(StopReason::EndTurn)]);
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(provider, &mut history);
            let gate = Arc::new(PreDispatchGate::new());
            assert!(gate.try_cancel());
            agent = agent
                .with_pre_dispatch_gate(gate)
                .with_pre_dispatch_rollback_len(0);

            let result = agent.run(default_input()).await;

            assert!(matches!(result, Err(AgentError::Cancelled)));
            drop(agent);
            assert_eq!(history.len(), 0);
            assert_eq!(requests.lock().unwrap().len(), 0);
        });
    }

    #[test_case(
        vec![tool_call_response("nonexistent_tool_xyz", "t1"), text_response(StopReason::EndTurn)],
        "t1"
        ; "parse_error"
    )]
    #[test_case(
        vec![tool_call_response("glob", "t1"), tool_call_response("glob", "t2"), tool_call_response("glob", "t3"), text_response(StopReason::EndTurn)],
        "t3"
        ; "doom_loop"
    )]
    fn error_emits_tool_done_event(responses: Vec<StreamResponse>, expected_error_id: &str) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            assert!(has_event(&events, |e| matches!(
                e,
                AgentEvent::ToolDone(done) if done.is_error && done.id == expected_error_id
            )));
        });
    }

    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        3, 1
        ; "nudge_on_empty_after_tools"
    )]
    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            text_response(StopReason::EndTurn),
        ],
        2, 0
        ; "no_nudge_when_text_after_tools"
    )]
    #[test_case(
        vec![
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        1, 0
        ; "no_nudge_without_recent_tools"
    )]
    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            thinking_response(),
            text_response(StopReason::EndTurn),
        ],
        3, 1
        ; "nudge_on_reasoning_after_tools"
    )]
    #[test_case(
        vec![
            thinking_response(),
            text_response(StopReason::EndTurn),
        ],
        2, 1
        ; "nudge_on_first_turn_reasoning"
    )]
    #[test_case(
        vec![
            thinking_response(),
            thinking_response(),
        ],
        2, 1
        ; "nudge_only_once_on_repeated_reasoning"
    )]
    fn nudge_behavior(responses: Vec<StreamResponse>, expected_turns: u32, expected_nudges: usize) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            let nudges = events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::Nudge))
                .count();
            assert_eq!(nudges, expected_nudges);

            let done = events
                .iter()
                .find_map(|e| match &e.event {
                    AgentEvent::Done { num_turns, .. } => Some(*num_turns),
                    _ => None,
                })
                .expect("expected Done event");
            assert_eq!(done, expected_turns);
        });
    }

    #[test]
    fn adaptive_cache_breakpoints_scales_with_user_message_count() {
        assert_eq!(adaptive_cache_breakpoints(0), CACHE_BREAKPOINTS_MIN);
        assert_eq!(adaptive_cache_breakpoints(1), CACHE_BREAKPOINTS_MIN);
        assert_eq!(
            adaptive_cache_breakpoints(CACHE_BREAKPOINT_SHORT_SESSION - 1),
            CACHE_BREAKPOINTS_MIN
        );
        assert_eq!(
            adaptive_cache_breakpoints(CACHE_BREAKPOINT_SHORT_SESSION),
            CACHE_BREAKPOINTS_SHORT
        );
        assert_eq!(
            adaptive_cache_breakpoints(CACHE_BREAKPOINT_MEDIUM_SESSION),
            CACHE_BREAKPOINTS_MEDIUM
        );
        assert_eq!(
            adaptive_cache_breakpoints(CACHE_BREAKPOINT_LONG_SESSION),
            CACHE_BREAKPOINTS_LONG
        );
        assert_eq!(
            adaptive_cache_breakpoints(CACHE_BREAKPOINT_LONG_SESSION + 100),
            CACHE_BREAKPOINTS_LONG
        );
    }
}
