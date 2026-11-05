//! Native execution; admission/recovery stays in the sitting lifecycle.

mod maintenance;
pub(super) use maintenance::{ordinary_due, run as maintain};

use super::*;
use crate::mcp::chat::{ChatToolBridge, CHAT_TOOLS_PATH};
use atomic_core::{
    agent::{ChatToolSession, ClaudeChatInput},
    AtomicChatNativeOutcome, AtomicChatUsage, ChatEvent, ChatMessageWithContext,
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Operation {
    New,
    Continue,
    Retry,
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Purpose {
    Reply,
    Recap,
}

pub(super) struct NativeTurn<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub attempt_id: &'a str,
    pub operation: Operation,
    pub purpose: Purpose,
    // A recorded-input Retry control, not every resumed native session.
    pub continuation: bool,
    pub input: ClaudeChatInput,
    pub input_tokens: usize,
}

fn input_counts(input: &ClaudeChatInput, total: usize) -> (usize, usize, usize) {
    let prompt = atomic_core::chunking::count_tokens(&input.prompt).saturating_add(8);
    let system = input
        .system_prompt
        .as_deref()
        .map(atomic_core::chunking::count_tokens)
        .unwrap_or(0);
    (
        prompt,
        system,
        total.saturating_sub(prompt).saturating_sub(system),
    )
}

#[derive(Deserialize)]
pub(super) struct NativeContext {
    estimated_tokens: usize,
    overhead_tokens: usize,
    system_tokens: usize,
}

impl NativeContext {
    fn project(&self, input: &ClaudeChatInput, total: usize) -> usize {
        let (prompt, system, overhead) = input_counts(input, total);
        self.estimated_tokens
            .saturating_add(prompt)
            .saturating_add(system.saturating_sub(self.system_tokens))
            .saturating_add(overhead.saturating_sub(self.overhead_tokens))
    }
}

#[derive(Deserialize)]
struct UsageCounts {
    input_tokens: usize,
    cache_creation_input_tokens: usize,
    cache_read_input_tokens: usize,
    output_tokens: usize,
}

#[derive(Deserialize)]
pub(super) struct NativeUsage {
    #[serde(deserialize_with = "nonempty_message_id")]
    message_id: String,
    usage: UsageCounts,
}

fn nonempty_message_id<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<String, D::Error> {
    let id = String::deserialize(decoder)?;
    if id.is_empty() {
        return Err(serde::de::Error::custom(
            "Native usage requires a message ID",
        ));
    }
    Ok(id)
}

impl NativeUsage {
    fn snapshot(self, turn_id: &str) -> AtomicChatUsage {
        AtomicChatUsage {
            turn_id: turn_id.into(),
            message_id: self.message_id,
            input_tokens: self.usage.input_tokens,
            cache_creation_input_tokens: self.usage.cache_creation_input_tokens,
            cache_read_input_tokens: self.usage.cache_read_input_tokens,
            output_tokens: self.usage.output_tokens,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct NativeStatus {
    pub session_id: Option<String>,
    pub writer_idle: Option<bool>,
    pub turn: Option<NativeReceipt>,
    pub context: Option<NativeContext>,
    pub maintenance: Option<maintenance::Receipt>,
}

#[derive(Deserialize)]
pub(super) struct NativeReceipt {
    pub session_id: String,
    pub turn_id: String,
    pub attempt_id: String,
    pub purpose: String,
    pub status: String,
    pub usage: Option<NativeUsage>,
    pub text: Option<String>,
    pub error: Option<String>,
    pub submission: Option<Submission>,
    pub initialized: Option<bool>,
    #[serde(default)]
    pub input_recorded: bool,
    native_result: Option<Value>,
}

impl NativeReceipt {
    fn recorded_input(&self) -> Option<bool> {
        if self.input_recorded || self.submission == Some(Submission::Recorded) {
            Some(true)
        } else if self.submission == Some(Submission::NotSubmitted) {
            Some(false)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Submission {
    NotSubmitted,
    Recorded,
    Unknown,
}

const RETRY_CONTINUATION: &str = "Continue the recorded unfinished turn from its saved native context. The user's message is already recorded; do not repeat it or repeat completed actions.";

pub(super) struct PreparedInput {
    pub operation: Operation,
    pub continuation: bool,
    pub input: ClaudeChatInput,
    pub input_tokens: usize,
    pub context_tokens: Option<usize>,
}

/// Read-only preparation under the same guard later used for admission/execution.
/// The count sizes this input and schemas, not the sitting's native occupancy.
pub(super) async fn prepare_input(
    db: &Db,
    scope: &MemuScope,
    id: &str,
    request: &ChatRequest,
    profile: &ClaudeChatProfile,
    work: &ConversationWork,
) -> Result<PreparedInput, JsonError> {
    if !work.matches(db, id) {
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":"Input preparation requires this sitting's lock"}),
        ));
    }
    let conv = read_chat(db, id).await?;
    if !conversation_matches(&conv, scope) {
        return Err((
            StatusCode::NOT_FOUND,
            serde_json::json!({"error":"Conversation not found"}),
        ));
    }
    let saved = conv.conversation.atomic_chat_state.as_ref()
        .filter(|saved| saved.backend == Backend::ClaudeCode && saved.phase != Phase::Closed)
        .ok_or_else(|| (StatusCode::CONFLICT, serde_json::json!({"error":"Native input requires an open or closing Claude Code sitting; existing API history cannot be migrated."})))?;
    let previous_turn = saved
        .recap
        .as_ref()
        .map(|recap| recap.request_message_id.as_str())
        .or_else(|| {
            saved
                .active
                .as_ref()
                .map(|turn| turn.user_message_id.as_str())
        })
        .or_else(|| {
            conv.messages
                .iter()
                .rev()
                .find(|row| row.message.role == "user")
                .map(|row| row.message.id.as_str())
        })
        .unwrap_or("");
    let observed = status(scope, id, previous_turn)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, serde_json::json!({"error":error})))?;
    let recorded = saved
        .active
        .as_ref()
        .filter(|turn| {
            turn.status == TurnStatus::Failed
                && matches!(request, ChatRequest::Retry { user_message_id }
            if *user_message_id == turn.user_message_id)
        })
        .and_then(|turn| turn.native_input_recorded);
    let (operation, continuation) = input_plan(&observed, request, recorded)?;
    if operation == Operation::New
        && atomic_core::chat::dialogue_messages(&conv, None)
            .iter()
            .any(|row| row.message.role == "assistant")
    {
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":"This sitting already has dialogue. Keep its history; do not initialize a replacement native session."}),
        ));
    }
    let (input, tokens) =
        db.0.project_claude_chat_input(
            id,
            request,
            profile.common_settings(),
            tools_for(scope),
            operation == Operation::New,
            continuation.then_some(RETRY_CONTINUATION),
        )
        .await
        .map_err(core_json_error)?;
    let context_tokens = observed
        .context
        .as_ref()
        .map(|context| context.project(&input, tokens));
    Ok(PreparedInput {
        operation,
        continuation,
        input,
        input_tokens: tokens,
        context_tokens,
    })
}

/// Choose input before admission replaces the saved attempt. A receipt from an
/// older attempt may still prove this user row was recorded; a file cannot.
fn input_plan(
    observed: &NativeStatus,
    request: &ChatRequest,
    recorded: Option<bool>,
) -> Result<(Operation, bool), JsonError> {
    let refused = |error: &str| (StatusCode::CONFLICT, serde_json::json!({"error":error}));
    if observed.writer_idle != Some(true) {
        return Err(refused("Native writer exit is not established. Keep this history and reload before continuing."));
    }
    let recap = matches!(request, ChatRequest::Recap { .. });
    if observed.maintenance.as_ref().is_some_and(|maintenance| {
        matches!(maintenance.status, atomic_core::AtomicChatMaintenanceStatus::Running
            | atomic_core::AtomicChatMaintenanceStatus::Prepared)
            || (maintenance.status == atomic_core::AtomicChatMaintenanceStatus::Blocked && !recap)
    }) {
        return Err(refused("Native maintenance is unresolved; only blocked history may proceed to close-out."));
    }
    if observed.turn.is_none() {
        let retry = matches!(request, ChatRequest::Retry { .. });
        // Missing MCP evidence proves nothing. Only affirmative saved input
        // evidence on a Failed turn permits Retry; never replace recorded input.
        if observed.session_id.is_none() {
            if matches!(request, ChatRequest::New { .. }) || (retry && recorded == Some(false)) {
                return Ok((Operation::New, false));
            }
        } else if (retry && recorded.is_some()) || recap {
            return Ok((
                if retry {
                    Operation::Retry
                } else {
                    Operation::Continue
                },
                retry && recorded == Some(true),
            ));
        }
        return Err(refused("Native input evidence is missing. Keep this history; do not initialize a replacement sitting."));
    }
    let receipt = observed.turn.as_ref().filter(|receipt| {
        Some(&receipt.session_id) == observed.session_id.as_ref()
    }).ok_or_else(|| refused("Native input evidence is missing. Keep this history; do not initialize a replacement sitting."))?;
    if let ChatRequest::Retry { user_message_id } = request {
        if receipt.turn_id != *user_message_id || receipt.purpose != "reply" {
            return Err(refused(
                "Retry has no matching saved native input evidence. Keep this history and reload.",
            ));
        }
    }
    let recorded = recorded == Some(true) || receipt.recorded_input() == Some(true);
    if receipt.initialized == Some(false)
        && receipt.submission == Some(Submission::NotSubmitted)
        && !recorded
        && !recap
        && receipt.status == "failed"
    {
        return Ok((Operation::New, false));
    }
    if receipt.initialized != Some(true) {
        return Err(refused("This sitting's native initialization is unconfirmed. Keep its history; do not create a replacement session."));
    }
    let retry = matches!(request, ChatRequest::Retry { .. });
    if matches!(request, ChatRequest::New { .. }) && receipt.status != "complete" {
        return Err(refused("The previous native turn is unfinished. Retry known failed work or close out from saved history."));
    }
    if retry
        && (receipt.status != "failed"
            || receipt.submission.is_none()
            || receipt.submission == Some(Submission::Unknown))
    {
        return Err(refused("The saved input's submission is unknown. Close out from saved history; do not repeat it."));
    }
    Ok((
        if retry {
            Operation::Retry
        } else {
            Operation::Continue
        },
        retry && recorded,
    ))
}

impl NativeStatus {
    fn matches_turn(
        &self,
        turn_id: &str,
        attempt_id: &str,
        purpose: Purpose,
        observed_session: Option<&str>,
    ) -> bool {
        self.turn.as_ref().is_some_and(|receipt| {
            receipt.turn_id == turn_id
                && receipt.attempt_id == attempt_id
                && Some(&receipt.session_id) == self.session_id.as_ref()
                && observed_session.is_none_or(|id| id == receipt.session_id)
                && receipt.purpose
                    == if purpose == Purpose::Reply {
                        "reply"
                    } else {
                        "recap"
                    }
        })
    }
}

// Used only after matching receipt identity and proving native writer exit.
fn receipt_result(receipt: NativeReceipt, purpose: Purpose) -> Result<String, (String, bool)> {
    if receipt.status == "complete" {
        return receipt.text.ok_or_else(|| {
            (
                "Native completion has no saved result; history is retained".into(),
                true,
            )
        });
    }
    let uncertain = purpose == Purpose::Reply && receipt.status != "failed";
    let error = if purpose == Purpose::Recap && receipt.status != "failed" {
        "The tool-free recap result was not recorded. Writer exit is confirmed; Retry close-out in this sitting.".into()
    } else {
        receipt.error.unwrap_or_else(|| {
            if uncertain {
                "Native completion was not recorded. Close out from saved history; do not replay this turn.".into()
            } else {
                "The native attempt failed. Retry the saved turn or close out.".into()
            }
        })
    };
    Err((error, uncertain))
}

pub(super) async fn status(
    scope: &MemuScope,
    id: &str,
    turn_id: &str,
) -> Result<NativeStatus, String> {
    fetch_status(scope, id, Some(turn_id)).await
}

async fn fetch_status(scope: &MemuScope, id: &str, turn_id: Option<&str>) -> Result<NativeStatus, String> {
    let mut query = vec![("user_id", scope.user_id.as_str()),
                         ("soul_id", scope.soul_id.as_str()), ("conversation_id", id)];
    if let Some(turn_id) = turn_id {
        query.push(("turn_id", turn_id));
    }
    let response = reqwest::Client::new()
        .get(format!("{}/integration/atomic/chat/status", scope.base_url))
        .query(&query)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let code = response.status();
        if code == reqwest::StatusCode::CONFLICT {
            let body: Value = response
                .json()
                .await
                .map_err(|e| format!("Invalid native status: {e}"))?;
            if body["detail"]["code"] == "atomic_native_turn_missing" {
                return serde_json::from_value(body["detail"].clone())
                    .map_err(|e| format!("Invalid native status: {e}"));
            }
        }
        return Err(format!("Native status failed ({code})"));
    }
    response
        .json()
        .await
        .map_err(|e| format!("Invalid native status: {e}"))
}

/// Caller holds the scoped sitting lock. Native exit and orphan Rust tool
/// cleanup still need their independent proofs before reading final records.
/// No model request or new admission here.
pub(super) async fn reconcile(
    state: &AppState,
    db: &Db,
    scope: &MemuScope,
    bridge: &ChatToolBridge,
    id: &str,
    work: &ConversationWork,
) -> Result<(), JsonError> {
    if !work.matches(db, id) {
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":"Recovery requires this sitting's lock"}),
        ));
    }
    maintenance::reconcile(state, db, scope, bridge, id, work).await?;
    let conv = read_chat(db, id).await?;
    if !conversation_matches(&conv, scope) {
        return Err((
            StatusCode::NOT_FOUND,
            serde_json::json!({"error":"Conversation not found"}),
        ));
    }
    let saved = conv
        .conversation
        .atomic_chat_state
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                serde_json::json!({"error":"This sitting has no native admission"}),
            )
        })?;
    if saved.backend != Backend::ClaudeCode {
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":"Restore claude_code=false for this API sitting"}),
        ));
    }
    if saved.phase == Phase::Closed {
        return maintain(state, db, scope, bridge, id, atomic_core::AtomicChatMaintenanceKind::Purge, work)
            .await.map(|_| ());
    }
    if saved
            .recap
            .as_ref()
            .is_some_and(|recap| matches!(recap.outcome, RecapOutcome::Complete { .. }))
    {
        return Ok(());
    }
    let (turn_id, attempt_id, purpose, was_failed) = if let Some(recap) = &saved.recap {
        (
            &recap.request_message_id,
            recap.native_attempt_id.as_deref(),
            Purpose::Recap,
            matches!(recap.outcome, RecapOutcome::Failed { .. }),
        )
    } else if let Some(active) = &saved.active {
        (
            &active.user_message_id,
            active.native_attempt_id.as_deref(),
            Purpose::Reply,
            active.status == TurnStatus::Failed,
        )
    } else {
        return Ok(());
    };
    let attempt_id = attempt_id.ok_or_else(|| (StatusCode::CONFLICT,
        serde_json::json!({"error":"Native attempt identity is missing; keep this history and start a new sitting"})))?;
    let observed = status(scope, id, turn_id).await;
    let status_unavailable = observed.is_err();
    let matched = observed
        .as_ref()
        .is_ok_and(|status| status.matches_turn(turn_id, attempt_id, purpose, None));
    let idle = observed
        .as_ref()
        .is_ok_and(|status| status.writer_idle == Some(true));
    if observed
        .as_ref()
        .is_ok_and(|status| status.writer_idle == Some(false))
    {
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":"Native work is still running. Wait, or open a new independent sitting; this history is retained."}),
        ));
    }
    let mut changed_tools = false;
    if idle {
        // Writer proof applies even when this attempt never obtained a receipt.
        for (message_id, records) in bridge
            .drain_sitting(&scope.user_id, &scope.soul_id, work)
            .await
        {
            db.0.save_chat_tool_records(id, &message_id, &records)
                .await
                .map_err(core_json_error)?;
            changed_tools = true;
        }
    }
    // A known refusal needs no new outcome, but orphan Rust work must drain first.
    if was_failed && !matched && idle {
        return Ok(());
    }
    let mut native = AtomicChatNativeOutcome {
        attempt_id: attempt_id.into(),
        usage: None,
        input_recorded: None,
        context_tokens: observed
            .as_ref()
            .ok()
            .filter(|_| matched && idle)
            .and_then(|status| status.context.as_ref())
            .map(|context| context.estimated_tokens),
    };
    let mut receipt = if matched {
        observed.ok().and_then(|status| status.turn)
    } else {
        None
    };
    native.input_recorded = receipt.as_ref().and_then(NativeReceipt::recorded_input);
    if let Some(usage) = receipt.as_mut().and_then(|receipt| receipt.usage.take()) {
        native.usage = Some(usage.snapshot(turn_id));
    }
    if idle {
        // The binder checkpoints any stranded running rows as unknown. Count
        // that persisted change once; the next reload reads their failed status.
        changed_tools |= conv
            .messages
            .iter()
            .find(|row| row.message.id == *turn_id)
            .is_some_and(|row| row.tool_calls.iter().any(|call| call.status == "running"));
        let records =
            db.0.saved_chat_tool_records(id, turn_id, tools_for(scope))
                .await
                .map_err(core_json_error)?;
        if !matched {
            // Missing ordinary receipts never prove submission; tool-free recap
            // attempts can be retried once writer exit is confirmed.
            let (error, uncertain) = if purpose == Purpose::Reply {
                ("Native completion is unknown. Writer exit is confirmed; close out in the existing session, not ordinary Retry.", true)
            } else {
                ("The tool-free recap result was not recorded. Writer exit is confirmed; Retry close-out in this sitting.", false)
            };
            return reconcile_failure(state, db, id, turn_id, saved, &native, purpose,
                error, uncertain, changed_tools).await;
        }
        let receipt = receipt.expect("matching receipt");
        let (error, uncertain) = match receipt_result(receipt, purpose) {
            Ok(text) => {
                let records = records.for_reply(&text);
                db.0.save_chat_reply(id, Some(turn_id), &text, &records, Some(&native))
                    .await
                    .map_err(core_json_error)?;
                chat_changed(state, db, id);
                return Ok(());
            }
            Err(failure) => failure,
        };
        let uncertain = uncertain
            || records
                .tool_calls
                .iter()
                .any(atomic_core::ChatToolCall::outcome_unknown);
        return reconcile_failure(
            state,
            db,
            id,
            turn_id,
            saved,
            &native,
            purpose,
            &error,
            uncertain,
            changed_tools,
        )
        .await;
    }
    let error = if status_unavailable {
        "Native status could not be read. Keep this history; check the memory server and reload, or start a new independent sitting."
    } else {
        "No matching native completion or writer-exit proof is available. Keep this history and start a new independent sitting, or reload after the pending work settles."
    };
    if !was_failed {
        reconcile_failure(
            state, db, id, turn_id, saved, &native, purpose, error, true, false,
        )
        .await?;
    }
    Err((StatusCode::CONFLICT, serde_json::json!({"error":error})))
}

fn context_length_error(receipt: &NativeReceipt) -> bool {
    let Some(result) = receipt.native_result.as_ref().filter(|result| result["is_error"] == true) else { return false; };
    result["errors"].as_array().into_iter().flatten().filter_map(Value::as_str)
        .chain(result["result"].as_str()).any(|error| {
            let error = error.to_ascii_lowercase();
            ["prompt is too long", "context_length_exceeded", "maximum context length", "exceeds the context window"]
                .iter().any(|phrase| error.contains(phrase))
        })
}

/// One existing native recap path, with at most one reserved file cleanup.
/// The cleanup basis identifies whether its one subsequent recap was admitted.
pub(super) async fn execute_recap(
    state: &AppState, db: &Db, scope: &MemuScope, profile: &ClaudeChatProfile,
    bridge: &ChatToolBridge, bridge_url: &str, id: &str, request: &ChatRequest,
    capacity: usize, work: &ConversationWork,
) -> Result<(), JsonError> {
    use atomic_core::{AtomicChatMaintenanceKind as Kind, AtomicChatMaintenanceStatus as Status};
    loop {
        let conv = read_chat(db, id).await?;
        let saved = conv.conversation.atomic_chat_state.as_ref().expect("closing native sitting");
        let final_cleanup = saved.maintenance.as_ref().filter(|prior| prior.kind == Kind::Final && prior.final_used);
        if let Some(cleanup) = final_cleanup.filter(|cleanup| cleanup.unresolved()) {
            return Err((StatusCode::CONFLICT, serde_json::json!({"error":cleanup.error.as_deref()
                .unwrap_or("Final native cleanup is unfinished; keep this sitting and reload.")})));
        }
        if maintenance::final_recap_used(saved) {
            let recap = saved.recap.as_ref().expect("post-clean recap attempt");
            let observed = status(scope, id, &recap.request_message_id).await
                .map_err(|error| (StatusCode::BAD_GATEWAY, serde_json::json!({"error":error})))?;
            if observed.writer_idle == Some(true) && recap.native_attempt_id.as_ref().is_some_and(|attempt|
                observed.matches_turn(&recap.request_message_id, attempt, Purpose::Recap, None))
                && observed.turn.as_ref().is_some_and(|receipt| receipt.status == "failed" && context_length_error(receipt))
            {
                return Err((StatusCode::CONFLICT, serde_json::json!({"error":
                    "The same-session recap is still too large after final cleanup. Keep this history; contact help@openalma.org for close-out assistance."})));
            }
        }
        let prepared = prepare_input(db, scope, id, request, profile, work).await?;
        let needs_cleanup = final_cleanup.is_none() && prepared.context_tokens.is_some_and(|tokens|
            tokens.saturating_add(atomic_core::agent::CHAT_RESPONSE_MAX_TOKENS as usize) > capacity);
        if !needs_cleanup {
            let control = db.0.admit_chat_turn(id, &scope.user_id, &scope.soul_id, &Backend::ClaudeCode,
                                              request, capacity).await.map_err(core_json_error)?;
            chat_changed(state, db, id);
            let result = execute_prepared(state, db, scope, profile, bridge, bridge_url,
                                          id, &control.id, Purpose::Recap, prepared).await;
            if result.is_ok() { return Ok(()); }
            // Only authoritative provider context failure, with this attempt's
            // writer exited, can spend the final cleanup after a failed call.
            let current = read_chat(db, id).await?;
            let attempt = current.conversation.atomic_chat_state.as_ref()
                .and_then(|saved| saved.recap.as_ref()).and_then(|recap| recap.native_attempt_id.as_deref());
            let context_failure = if final_cleanup.is_none() {
                status(scope, id, &control.id).await.ok().is_some_and(|observed|
                    observed.writer_idle == Some(true) && attempt.is_some_and(|attempt|
                        observed.matches_turn(&control.id, attempt, Purpose::Recap, None))
                    && observed.turn.as_ref().is_some_and(|receipt| receipt.status == "failed" && context_length_error(receipt)))
            } else { false };
            if !context_failure { return result.map(|_| ()); }
        }
        if saved.context_window_tokens.is_some_and(|previous| capacity < previous) {
            return Err((StatusCode::CONFLICT, serde_json::json!({"error":
                "The selected usable context window is smaller than this sitting used. Restore a larger compatible model and claude_code_context_window_tokens; cleanup does not repair configuration changes."})));
        }
        let outcome = maintain(state, db, scope, bridge, id, Kind::Final, work).await?;
        if outcome != Status::Complete {
            return Err((StatusCode::CONFLICT, serde_json::json!({"error":
                "Final native cleanup did not complete. Keep this sitting; its saved maintenance error has the details."})));
        }
        // Preserve the old estimate, but let Claude's actual context limit
        // decide this single same-session retry after successful cleanup.
    }
}

async fn reconcile_failure(
    state: &AppState,
    db: &Db,
    id: &str,
    turn_id: &str,
    saved: &atomic_core::AtomicChatState,
    native: &AtomicChatNativeOutcome,
    purpose: Purpose,
    error: &str,
    uncertain: bool,
    changed_tools: bool,
) -> Result<(), JsonError> {
    let unchanged = if purpose == Purpose::Recap {
        saved
            .recap
            .as_ref()
            .is_some_and(|recap| match &recap.outcome {
                RecapOutcome::Failed { error: old } => !uncertain && old == error,
                RecapOutcome::Uncertain { error: old } => uncertain && old == error,
                _ => false,
            })
    } else {
        saved.active.as_ref().is_some_and(|turn| {
            turn.status
                == if uncertain {
                    TurnStatus::Uncertain
                } else {
                    TurnStatus::Failed
                }
                && turn.error.as_deref() == Some(error)
                && (turn.native_input_recorded == Some(true)
                    || turn.native_input_recorded
                        == native
                            .input_recorded
                            .filter(|recorded| *recorded || !uncertain))
        })
    };
    if unchanged
        && !changed_tools
        && saved.native_context_tokens == native.context_tokens
        && native
            .usage
            .as_ref()
            .is_none_or(|usage| saved.native_usage.as_ref() == Some(usage))
    {
        return Ok(());
    }
    db.0.fail_chat_turn(id, turn_id, error, uncertain, Some(native))
        .await
        .map_err(core_json_error)?;
    chat_changed(state, db, id);
    Ok(())
}

// These are MCP-normalized observations. Only the bridge executes/checkpoints tools.
#[derive(Deserialize)]
struct BlockIdentity {
    record_id: String,
    message_id: Option<String>,
    parent_call_id: Option<String>,
    block_index: usize,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    Session {
        session_id: String,
        turn_id: String,
        attempt_id: String,
    },
    Text {
        #[serde(flatten)]
        identity: BlockIdentity,
        text: String,
    },
    ToolCall,
    ToolResult,
    Usage {
        #[serde(flatten)]
        usage: NativeUsage,
    },
    Complete {
        session_id: String,
        text: String,
    },
    Error {
        error: String,
    },
}

#[derive(Default)]
struct NativeStream {
    session_id: Option<String>,
    refused: bool,
    usage: Option<AtomicChatUsage>,
    text_message: Option<String>,
    blocks: Vec<(String, std::collections::BTreeMap<usize, String>)>,
}

impl NativeStream {
    fn accept(
        &mut self,
        frame: Frame,
        turn: &NativeTurn<'_>,
        on_event: &impl Fn(ChatEvent),
    ) -> Result<Option<String>, String> {
        match frame {
            Frame::Session {
                session_id,
                turn_id,
                attempt_id,
            } => {
                if turn_id != turn.turn_id
                    || attempt_id != turn.attempt_id
                    || self.session_id.as_ref().is_some_and(|id| id != &session_id)
                {
                    return Err("Native session/attempt does not match this turn".into());
                }
                self.session_id = Some(session_id);
            }
            Frame::Text { identity, text } => {
                if identity.parent_call_id.is_none() {
                    let key = identity.message_id.unwrap_or_else(|| identity.record_id.clone());
                    if self.text_message.as_ref() != Some(&key) {
                        self.blocks.clear();
                        self.text_message = Some(key);
                    }
                    let record_index = match self.blocks.iter().position(|(id, _)| id == &identity.record_id) {
                        Some(index) => index,
                        None => {
                            self.blocks.push((identity.record_id, Default::default()));
                            self.blocks.len() - 1
                        }
                    };
                    self.blocks[record_index].1.insert(identity.block_index, text);
                    on_event(ChatEvent::StreamDelta {
                        conversation_id: turn.conversation_id.into(),
                        content: self.blocks.iter().flat_map(|(_, blocks)| blocks.values())
                            .cloned().collect::<Vec<_>>().join(""),
                    });
                }
            }
            Frame::Usage { usage } => self.usage = Some(usage.snapshot(turn.turn_id)),
            Frame::Complete { session_id, text } => {
                if self.session_id.as_ref() != Some(&session_id) {
                    return Err("Native completion has no matching session".into());
                }
                return Ok(Some(text));
            }
            Frame::Error { error } => return Err(error),
            Frame::ToolCall | Frame::ToolResult => {}
        }
        Ok(None)
    }
}

async fn read_reply(
    scope: &MemuScope,
    profile: &ClaudeChatProfile,
    turn: &NativeTurn<'_>,
    bridge_url: Option<&str>,
    credential: Option<&str>,
    stream: &mut NativeStream,
    on_event: &impl Fn(ChatEvent),
) -> Result<String, String> {
    // MCP's captured timeout owns child execution. Allow startup/terminal delivery too.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(
            profile.timeout_seconds.saturating_add(60),
        ))
        .build()
        .map_err(|e| e.to_string())?;
    let (prompt_tokens, system_tokens, overhead_tokens) =
        input_counts(&turn.input, turn.input_tokens);
    let mut request = client
        .post(format!("{}/integration/atomic/chat", scope.base_url))
        .json(&serde_json::json!({
            "user_id": scope.user_id, "soul_id": scope.soul_id,
            "conversation_id": turn.conversation_id, "turn_id": turn.turn_id,
            "attempt_id": turn.attempt_id,
            "operation": turn.operation, "purpose": turn.purpose,
            "prompt": turn.input.prompt, "system_prompt": turn.input.system_prompt,
            "bridge_url": bridge_url, "profile_fingerprint": profile.fingerprint,
            "prompt_tokens": prompt_tokens,
            "system_tokens": system_tokens,
            "overhead_tokens": overhead_tokens,
        }));
    if let Some(credential) = credential {
        request = request.bearer_auth(credential);
    }
    let mut response = request.send().await.map_err(|e| {
        // A connection failure cannot submit this request. A lost response can.
        stream.refused = e.is_connect();
        e.to_string()
    })?;
    if !response.status().is_success() {
        let code = response.status();
        stream.refused = true;
        let body = response.text().await.map_err(|e| e.to_string())?;
        return Err(format!("Native chat refused ({code}): {body}"));
    }
    let mut pending = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let frame =
                serde_json::from_slice(&line).map_err(|e| format!("Invalid native frame: {e}"))?;
            if let Some(text) = stream.accept(frame, turn, on_event)? {
                return Ok(text);
            }
        }
        // MCP bounds delivery at 16 MiB. An unterminated frame cannot grow forever.
        if pending.len() > 16 * 1024 * 1024 {
            return Err("Native frame exceeds delivery limit".into());
        }
    }
    Err("Native stream ended without validated completion; reload before retrying".into())
}

async fn observe(
    scope: &MemuScope,
    turn: &NativeTurn<'_>,
    stream: &mut NativeStream,
    observation_budget: Duration,
) -> Result<NativeStatus, String> {
    let started = std::time::Instant::now();
    let mut first_observation = true;
    loop {
        // Even a fully elapsed HTTP timeout gets one bounded receipt read.
        let remaining = if first_observation {
            Duration::from_secs(30)
        } else {
            observation_budget.saturating_sub(started.elapsed())
        };
        first_observation = false;
        if remaining.is_zero() {
            break Err("Native exit could not be confirmed within the execution budget".into());
        }
        let mut evidence =
            tokio::time::timeout(remaining, status(scope, turn.conversation_id, turn.turn_id))
                .await
                .unwrap_or_else(|_| Err("Native status observation timed out".into()));
        if let Ok(evidence) = &mut evidence {
            if evidence.matches_turn(
                turn.turn_id,
                turn.attempt_id,
                turn.purpose,
                stream.session_id.as_deref(),
            ) {
                if let Some(usage) = evidence.turn.as_mut().and_then(|turn| turn.usage.take()) {
                    stream.usage = Some(usage.snapshot(turn.turn_id));
                }
            }
        }
        if stream.refused
            || evidence.as_ref().is_ok_and(|e| {
                e.writer_idle == Some(true)
                    && e.matches_turn(
                        turn.turn_id,
                        turn.attempt_id,
                        turn.purpose,
                        stream.session_id.as_deref(),
                    )
            })
        {
            break evidence;
        }
        tokio::time::sleep(
            Duration::from_secs(2).min(observation_budget.saturating_sub(started.elapsed())),
        )
        .await;
    }
}

/// Called only under the executor-owned sitting lock, after managed admission.
/// Carry the exact read-only projection into the runner.
pub(super) async fn execute_prepared(
    state: &AppState,
    db: &Db,
    scope: &MemuScope,
    profile: &ClaudeChatProfile,
    bridge: &ChatToolBridge,
    bridge_url: &str,
    id: &str,
    turn_id: &str,
    purpose: Purpose,
    prepared: PreparedInput,
) -> Result<ChatMessageWithContext, JsonError> {
    let conv = read_chat(db, id).await?;
    let saved = conv
        .conversation
        .atomic_chat_state
        .as_ref()
        .expect("admitted sitting");
    let (attempt, page, canvas) = match purpose {
        Purpose::Reply => {
            let active = saved
                .active
                .as_ref()
                .filter(|turn| turn.user_message_id == turn_id)
                .expect("admitted native reply");
            (
                active.native_attempt_id.as_deref(),
                active.page_context.clone(),
                active.canvas_context.clone(),
            )
        }
        Purpose::Recap => (
            saved
                .recap
                .as_ref()
                .filter(|recap| recap.request_message_id == turn_id)
                .and_then(|recap| recap.native_attempt_id.as_deref()),
            None,
            None,
        ),
    };
    let attempt = attempt.expect("admitted native attempt");
    let session =
        db.0.chat_tool_session(
            id,
            turn_id,
            profile.common_settings(),
            tools_for(scope),
            page,
            canvas,
            chat_event_callback(
                state.event_tx.clone(),
                db.1.clone(),
                Some((scope.user_id.clone(), scope.soul_id.clone())),
            ),
        )
        .await;
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            let outcome = AtomicChatNativeOutcome {
                attempt_id: attempt.into(),
                usage: None,
                input_recorded: Some(prepared.continuation),
                context_tokens: None,
            };
            db.0.fail_chat_turn(id, turn_id, &error.to_string(), false, Some(&outcome))
                .await
                .map_err(core_json_error)?;
            chat_changed(state, db, id);
            return Err(core_json_error(error));
        }
    };
    execute(
        state,
        db,
        scope,
        profile,
        bridge,
        bridge_url,
        NativeTurn {
            conversation_id: id,
            turn_id,
            attempt_id: attempt,
            purpose,
            operation: prepared.operation,
            continuation: prepared.continuation,
            input: prepared.input,
            input_tokens: prepared.input_tokens,
        },
        session,
    )
    .await
}

pub(super) async fn execute(
    state: &AppState,
    db: &Db,
    scope: &MemuScope,
    profile: &ClaudeChatProfile,
    bridge: &ChatToolBridge,
    bridge_base_url: &str,
    turn: NativeTurn<'_>,
    session: ChatToolSession,
) -> Result<ChatMessageWithContext, JsonError> {
    let reply_events = turn.purpose == Purpose::Reply;
    let broadcast = chat_event_callback(
        state.event_tx.clone(),
        db.1.clone(),
        Some((scope.user_id.clone(), scope.soul_id.clone())),
    );
    let on_event = move |event| {
        if reply_events {
            broadcast(event);
        }
    };
    let (lease, recap_records) = if turn.purpose == Purpose::Reply {
        let lease = match bridge.register(session) {
            Ok(lease) => lease,
            Err(error) => {
                let outcome = AtomicChatNativeOutcome {
                    attempt_id: turn.attempt_id.into(),
                    usage: None,
                    input_recorded: Some(turn.continuation),
                    context_tokens: None,
                };
                db.0.fail_chat_turn(
                    turn.conversation_id,
                    turn.turn_id,
                    &error,
                    false,
                    Some(&outcome),
                )
                .await
                .map_err(core_json_error)?;
                chat_changed(state, db, turn.conversation_id);
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error":error}),
                ));
            }
        };
        (Some(lease), None)
    } else {
        (None, Some(session.finish()))
    };
    let bridge_url = format!("{}{CHAT_TOOLS_PATH}", bridge_base_url.trim_end_matches('/'));
    let started = std::time::Instant::now();
    let observation_budget = Duration::from_secs(profile.timeout_seconds.saturating_add(60));
    let mut stream = NativeStream::default();
    let result = read_reply(
        scope,
        profile,
        &turn,
        lease.as_ref().map(|_| bridge_url.as_str()),
        lease.as_ref().map(|l| l.credential()),
        &mut stream,
        &on_event,
    )
    .await;
    // Keep this executor-owned lease usable if only HTTP delivery was lost.
    // A local timeout, terminal frame, or dead MCP connection is not writer proof.
    // After the captured execution budget plus grace, abandon future admission,
    // drain admitted tools and retain Uncertain. That is not proof of child exit.
    let evidence = observe(
        scope,
        &turn,
        &mut stream,
        observation_budget.saturating_sub(started.elapsed()),
    )
    .await;
    // Stop admission and drain actual tools on every path, before terminal publication.
    let records = match lease {
        Some(lease) => lease.finish().await,
        None => recap_records.expect("recap records"),
    };
    let context_tokens = evidence
        .as_ref()
        .ok()
        .filter(|status| {
            status.writer_idle == Some(true)
                && (stream.refused
                    || status.matches_turn(
                        turn.turn_id,
                        turn.attempt_id,
                        turn.purpose,
                        stream.session_id.as_deref(),
                    ))
        })
        .and_then(|status| status.context.as_ref())
        .map(|context| context.estimated_tokens);
    let input_recorded = match evidence
        .as_ref()
        .ok()
        .filter(|status| {
            status.matches_turn(
                turn.turn_id,
                turn.attempt_id,
                turn.purpose,
                stream.session_id.as_deref(),
            )
        })
        .and_then(|status| status.turn.as_ref())
    {
        Some(receipt) => receipt.recorded_input(),
        None => stream.refused.then_some(turn.continuation),
    };
    let result = {
        let error = match &result {
            Ok(_) => "Native completion could not be reconciled".to_string(),
            Err(error) => error.clone(),
        };
        // Stream loss/overflow can leave the native producer running. Never infer
        // writer exit from this Rust worker or its drained tool lease.
        match evidence {
            Ok(status)
                if status.matches_turn(
                    turn.turn_id,
                    turn.attempt_id,
                    turn.purpose,
                    stream.session_id.as_deref(),
                ) =>
            {
                let receipt = status.turn.expect("matched native receipt");
                if status.writer_idle == Some(true) {
                    receipt_result(receipt, turn.purpose)
                } else {
                    Err((receipt.error.unwrap_or(error), true))
                }
            }
            _ => Err((error, !stream.refused)),
        }
    };
    let native_outcome = AtomicChatNativeOutcome {
        attempt_id: turn.attempt_id.into(),
        usage: stream.usage,
        input_recorded,
        context_tokens,
    };
    let text = match result {
        Ok(text) => text,
        Err((error, uncertain)) => {
            // Finish can know an outcome whose per-tool checkpoint failed.
            db.0.save_chat_tool_records(turn.conversation_id, turn.turn_id, &records)
                .await
                .map_err(core_json_error)?;
            let uncertain = uncertain
                || records
                    .tool_calls
                    .iter()
                    .any(atomic_core::ChatToolCall::outcome_unknown);
            db.0.fail_chat_turn(
                turn.conversation_id,
                turn.turn_id,
                &error,
                uncertain,
                Some(&native_outcome),
            )
            .await
            .map_err(core_json_error)?;
            chat_changed(state, db, turn.conversation_id);
            return Err((StatusCode::BAD_GATEWAY, serde_json::json!({"error":error})));
        }
    };
    let records = records.for_reply(&text);
    let message =
        db.0.save_chat_reply(
            turn.conversation_id,
            Some(turn.turn_id),
            &text,
            &records,
            Some(&native_outcome),
        )
        .await
        .map_err(core_json_error)?;
    let mut reply = ChatMessageWithContext {
        message,
        tool_calls: records.tool_calls,
        citations: records.citations,
    };
    for call in &mut reply.tool_calls {
        call.message_id = reply.message.id.clone();
    }
    for citation in &mut reply.citations {
        citation.message_id = reply.message.id.clone();
    }
    on_event(ChatEvent::Complete {
        conversation_id: turn.conversation_id.into(),
        message: reply.clone(),
    });
    chat_changed(state, db, turn.conversation_id);
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpServer};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    struct MockNative {
        mode: &'static str,
        credential: Mutex<String>,
        attempt_id: Mutex<String>,
        turn_id: Mutex<String>,
        maintenance: Mutex<Option<Value>>,
        status_reads: AtomicUsize,
        purpose: Mutex<String>,
        context_tokens: AtomicUsize,
        publications: Mutex<Vec<Value>>,
        purge_calls: AtomicUsize,
        reply_calls: AtomicUsize,
        publication_calls: AtomicUsize,
        admission_error: Mutex<String>,
    }

    fn counts() -> Value {
        serde_json::json!({"input_tokens":20,"cache_creation_input_tokens":30,
            "cache_read_input_tokens":100,"output_tokens":5})
    }

    #[test]
    fn context_projection_adds_only_uncovered_input_and_overhead_growth() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tests/fixtures/native_context.json"))
                .unwrap();
        for case in cases {
            let input = ClaudeChatInput {
                prompt: case["prompt"].as_str().unwrap().into(),
                system_prompt: case["system_prompt"].as_str().map(str::to_owned),
            };
            let total = case["input_tokens"].as_u64().unwrap() as usize;
            assert_eq!(
                input_counts(&input, total),
                (
                    case["prompt_tokens"].as_u64().unwrap() as usize,
                    case["system_tokens"].as_u64().unwrap() as usize,
                    case["overhead_tokens"].as_u64().unwrap() as usize,
                )
            );
            let context: Option<NativeContext> =
                serde_json::from_value(case["before"].clone()).unwrap();
            let projected = context
                .as_ref()
                .map(|context| context.project(&input, total));
            assert_eq!(
                serde_json::to_value(projected).unwrap(),
                case["projected"],
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn input_evidence_distinguishes_initialization_replay_and_recorded_retry() {
        let retry = ChatRequest::Retry {
            user_message_id: "saved-user".into(),
        };
        let evidence = |submission: &str, initialized: bool, input_recorded: bool| {
            serde_json::from_value::<NativeStatus>(serde_json::json!({
                "session_id":"native", "writer_idle":true, "turn":{
                    "session_id":"native", "turn_id":"saved-user", "attempt_id":"old-attempt",
                    "purpose":"reply", "status":"failed", "submission":submission,
                    "initialized":initialized, "input_recorded":input_recorded
                }
            }))
            .unwrap()
        };
        let unused = input_plan(&evidence("not_submitted", false, false), &retry, None).unwrap();
        assert_eq!(unused, (Operation::New, false));
        let unsubmitted =
            input_plan(&evidence("not_submitted", true, false), &retry, None).unwrap();
        assert_eq!(unsubmitted, (Operation::Retry, false));
        for submission in ["recorded", "not_submitted"] {
            let recorded = input_plan(&evidence(submission, true, true), &retry, None).unwrap();
            assert_eq!(recorded, (Operation::Retry, true));
        }
        assert!(input_plan(&evidence("unknown", true, false), &retry, None).is_err());
        assert!(input_plan(&evidence("unknown", true, true), &retry, Some(true)).is_err());
        assert!(input_plan(&evidence("recorded", false, true), &retry, None).is_err());
        let mut mismatched = evidence("recorded", true, true);
        mismatched.turn.as_mut().unwrap().turn_id = "different-user".into();
        assert!(input_plan(&mismatched, &retry, None).is_err());
        mismatched.turn.as_mut().unwrap().turn_id = "saved-user".into();
        mismatched.turn.as_mut().unwrap().session_id = "different-native".into();
        assert!(input_plan(&mismatched, &retry, None).is_err());
        let recap = ChatRequest::Recap {
            instruction: "Recap this sitting".into(),
        };
        let closeout = input_plan(&evidence("unknown", true, false), &recap, None).unwrap();
        assert_eq!(closeout, (Operation::Continue, false));
        for idle in [None, Some(false)] {
            let mut running = evidence("recorded", true, true);
            running.writer_idle = idle;
            assert!(input_plan(&running, &retry, Some(false)).is_err());
            assert!(input_plan(&running, &recap, None).is_err());
        }
        let mut empty: NativeStatus = serde_json::from_value(serde_json::json!({
            "session_id":null,"turn":null,"writer_idle":true
        }))
        .unwrap();
        assert_eq!(
            input_plan(&empty, &retry, Some(false)).unwrap(),
            (Operation::New, false)
        );
        assert!(input_plan(&empty, &retry, None).is_err());
        assert!(input_plan(&empty, &retry, Some(true)).is_err());
        assert!(input_plan(&empty, &recap, None).is_err());
        empty.session_id = Some("native".into());
        for recorded in [false, true] {
            let plan = input_plan(&empty, &retry, Some(recorded)).unwrap();
            assert_eq!(plan, (Operation::Retry, recorded));
        }
        assert!(input_plan(&empty, &retry, None).is_err());
    }

    #[test]
    fn snapshots_replace_blocks_and_usage_without_mixing_nested_messages() {
        let turn = NativeTurn {
            conversation_id: "conversation",
            turn_id: "turn",
            attempt_id: "attempt",
            operation: Operation::Continue,
            purpose: Purpose::Reply,
            continuation: false,
            input: ClaudeChatInput {
                prompt: "new input".into(),
                system_prompt: None,
            },
            input_tokens: 100,
        };
        let stream_events = Mutex::new(Vec::new());
        let on_event = |event| {
            if let ChatEvent::StreamDelta { content, .. } = event {
                stream_events.lock().unwrap().push(content);
            }
        };
        let mut stream = NativeStream::default();
        let mut accept = |value| {
            stream
                .accept(serde_json::from_value(value).unwrap(), &turn, &on_event)
                .unwrap()
        };
        accept(
            serde_json::json!({"type":"session","session_id":"native","turn_id":"turn","attempt_id":"attempt","profile":{"fingerprint":"fingerprint"}}),
        );
        for (record, message, index, text, parent) in [
            ("z-record", Some("message"), 0, "old", None),
            ("z-record", Some("message"), 0, "first", None),
            ("z-record", Some("message"), 1, " second", None),
            ("a-record", Some("message"), 0, " third", None),
            ("z-record", Some("message"), 1, " revised", None),
            ("nested-record", Some("nested-message"), 0, "nested", Some("parent")),
            ("z-record", Some("message"), 0, "first", None),
            ("a-record", Some("new-message"), 0, "new", None),
            ("z-fallback", None, 1, "tail", None),
            ("z-fallback", None, 0, "head ", None),
            ("z-fallback", None, 1, "replacement", None),
            ("a-fallback", None, 0, "other fallback", None),
        ] {
            let mut frame = serde_json::json!({"type":"text","record_id":record,"parent_call_id":parent,
                "block_index":index,"mode":"replace","text":text});
            if let Some(message) = message {
                frame["message_id"] = serde_json::json!(message);
            }
            accept(frame);
        }
        for message in ["earlier", "latest"] {
            accept(serde_json::json!({"type":"usage","message_id":message,"usage":counts()}));
        }
        assert_eq!(
            accept(serde_json::json!({"type":"complete","session_id":"native","text":"exact"})),
            Some("exact".into())
        );
        assert_eq!(
            *stream_events.lock().unwrap(),
            ["old", "first", "first second", "first second third", "first revised third",
             "first revised third", "new", "tail", "head tail", "head replacement", "other fallback"]
        );
        assert_eq!(stream.usage.unwrap().message_id, "latest");
        let mut status: NativeStatus = serde_json::from_value(serde_json::json!({
            "session_id":"native", "writer_idle":true,
            "turn":{"session_id":"native","turn_id":"turn","attempt_id":"attempt","purpose":"reply",
                "status":"complete","submission":"recorded","initialized":true,"text":"exact"}
        }))
        .unwrap();
        assert!(status.matches_turn(turn.turn_id, turn.attempt_id, turn.purpose, Some("native")));
        assert!(!status.matches_turn(
            turn.turn_id,
            turn.attempt_id,
            turn.purpose,
            Some("other-session")
        ));
        status.turn.as_mut().unwrap().attempt_id = "older-attempt".into();
        assert!(!status.matches_turn(turn.turn_id, turn.attempt_id, turn.purpose, Some("native")));
        status.turn.as_mut().unwrap().attempt_id = "attempt".into();
        status.turn.as_mut().unwrap().turn_id = "other-turn".into();
        assert!(!status.matches_turn(turn.turn_id, turn.attempt_id, turn.purpose, Some("native")));
        assert!(serde_json::from_value::<NativeUsage>(serde_json::json!({
            "message_id":"", "usage":counts()
        }))
        .is_err());
        let _: Frame = serde_json::from_value(
            serde_json::json!({"type":"tool_call","unused":"observation only"}),
        )
        .unwrap();
    }

    async fn mock_chat(
        mock: web::Data<MockNative>,
        request: HttpRequest,
        body: web::Json<Value>,
    ) -> HttpResponse {
        assert_eq!(body["profile_fingerprint"], "fingerprint");
        if body["purpose"] == "reply" {
            mock.reply_calls.fetch_add(1, Ordering::Relaxed);
        }
        if matches!(
            mock.mode,
            "success" | "route" | "retry" | "continuation_refused"
        ) {
            assert_eq!(
                body["prompt_tokens"].as_u64().unwrap() as usize,
                atomic_core::chunking::count_tokens(body["prompt"].as_str().unwrap()) + 8
            );
            if body["purpose"] == "recap" {
                assert_eq!(body["overhead_tokens"], 0, "recap has no tool schemas");
            } else {
                assert!(
                    body["overhead_tokens"].as_u64().unwrap() > 0,
                    "the runner must receive preparation's schema-inclusive token count"
                );
            }
        }
        *mock.attempt_id.lock().unwrap() = body["attempt_id"].as_str().unwrap().into();
        *mock.turn_id.lock().unwrap() = body["turn_id"].as_str().unwrap().into();
        *mock.purpose.lock().unwrap() = body["purpose"].as_str().unwrap().into();
        if body["purpose"] == "recap" {
            assert_eq!(body["operation"], "continue");
            assert!(body["system_prompt"].is_null() && body["bridge_url"].is_null());
            assert!(!request.headers().contains_key("authorization"));
        } else {
            *mock.credential.lock().unwrap() = request
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            if matches!(mock.mode, "retry" | "continuation_refused") {
                assert_eq!(body["operation"], "retry");
                assert!(body["system_prompt"].is_null());
            } else {
                assert_eq!(body["operation"], "new");
                assert!(body["system_prompt"]
                    .as_str()
                    .unwrap()
                    .ends_with("saved presence"));
            }
            assert!(body["bridge_url"]
                .as_str()
                .unwrap()
                .ends_with(CHAT_TOOLS_PATH));
        }
        if mock.mode == "refused" {
            return HttpResponse::Conflict()
                .json(serde_json::json!({"detail":"Profile changed; reload before sending"}));
        }
        if matches!(mock.mode, "server_refused" | "continuation_refused") {
            return HttpResponse::BadGateway()
                .json(serde_json::json!({"detail":"Atomic scoped tool discovery failed"}));
        }
        let mut frames = vec![
            serde_json::json!({"type":"session","session_id":"native-id","turn_id":body["turn_id"],"attempt_id":body["attempt_id"],"profile":{"fingerprint":"fingerprint"}}),
            serde_json::json!({"type":"text","record_id":"record","message_id":"message","parent_call_id":null,
                "block_index":0,"mode":"replace","text":"é first"}),
            serde_json::json!({"type":"text","record_id":"record","message_id":"message","parent_call_id":null,
                "block_index":0,"mode":"replace","text":"é replaced"}),
            serde_json::json!({"type":"tool_call","record_id":"record","message_id":"message","parent_call_id":null,
                "block_index":1,"call_id":"observed-only","name":"focus_atom","input":{"atom_id":"never-executed"}}),
            serde_json::json!({"type":"usage","message_id":"message","usage":counts()}),
        ];
        match mock.mode {
            "route" if body["purpose"] == "recap" => frames.push(serde_json::json!({"type":"complete","session_id":"native-id","text":"Exact recap"})),
            "success" | "route" => frames.push(serde_json::json!({"type":"complete","session_id":"native-id","text":"Exact terminal response"})),
            "recap" => frames.push(serde_json::json!({"type":"complete","session_id":"native-id","text":"Exact recap"})),
            "mismatch" => frames.push(serde_json::json!({"type":"complete","session_id":"native-id","text":"Not the saved answer"})),
            _ => {} // EOF is not completion; status decides what was recorded.
        }
        let bytes = frames
            .iter()
            .map(|frame| format!("{frame}\n"))
            .collect::<String>()
            .into_bytes();
        let chunks = bytes
            .chunks(3)
            .map(|part| Ok::<_, actix_web::Error>(web::Bytes::copy_from_slice(part)))
            .collect::<Vec<_>>();
        HttpResponse::Ok()
            .content_type("application/x-ndjson")
            .streaming(futures::stream::iter(chunks))
    }

    async fn mock_maintenance(mock: web::Data<MockNative>, core: web::Data<atomic_core::AtomicCore>, body: web::Json<Value>) -> HttpResponse {
        assert_eq!(mock.mode, "route");
        let purge = body["action"] == "purge";
        if purge {
            mock.purge_calls.fetch_add(1, Ordering::Relaxed);
            let publications = mock.publications.lock().unwrap().clone();
            assert_eq!(publications.len(), 1, "publish before purging native history");
            let id = publications[0]["conversation_id"].as_str().unwrap().strip_prefix("chat:atomic-").unwrap();
            assert_eq!(core.get_conversation(id).await.unwrap().unwrap().conversation.atomic_chat_state.unwrap().phase, Phase::Closed);
        } else {
            assert_eq!(body["action"], "compact");
        }
        assert_eq!(body["native_attempt_id"], *mock.attempt_id.lock().unwrap());
        assert_eq!(body["turn_id"], *mock.turn_id.lock().unwrap());
        let receipt = serde_json::json!({"attempt_id":body["attempt_id"],"native_attempt_id":body["native_attempt_id"],
            "turn_id":body["turn_id"],"action":body["action"],
            "status":if purge { "failed" } else { "complete" },
            "error":if purge { Some("Fictional storage removal failed") } else { None }});
        let mut saved = mock.maintenance.lock().unwrap();
        if !purge {
            assert!(saved.is_none(), "successful reply triggers cleanup only once");
        }
        *saved = Some(receipt.clone());
        HttpResponse::Ok().json(serde_json::json!({"maintenance":receipt,
            "context":{"estimated_tokens":90000,"overhead_tokens":50,"system_tokens":25}}))
    }

    async fn mock_session_end(mock: web::Data<MockNative>, core: web::Data<atomic_core::AtomicCore>, body: web::Json<Value>) -> HttpResponse {
        let id = body["conversation_id"].as_str().unwrap().strip_prefix("chat:atomic-").unwrap();
        let saved = core.get_conversation(id).await.unwrap().unwrap();
        assert_eq!(saved.conversation.atomic_chat_state.unwrap().phase, Phase::Closing);
        assert_eq!(body["activity_recap"], "Exact recap");
        let transcript = body["transcript"].as_array().unwrap();
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0]["role"], "user");
        assert_eq!(transcript[1]["role"], "assistant");
        assert_eq!(transcript[1]["content"], "Exact terminal response");
        if mock.publication_calls.fetch_add(1, Ordering::Relaxed) == 0 {
            *mock.admission_error.lock().unwrap() = "Repair Atomic's session helper".into();
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({"error":"Fictional publication failure"}));
        }
        mock.publications.lock().unwrap().push(body.into_inner());
        HttpResponse::Ok().json(serde_json::json!({"ok":true}))
    }

    async fn mock_profile(mock: web::Data<MockNative>) -> HttpResponse {
        HttpResponse::Ok().json(serde_json::json!({"backend":"claude_code","cli":{
            "model":"fictional","context_window_tokens":128000,"timeout_seconds":1,
            "effort":"medium","fingerprint":"fingerprint","memu_response_sentences":5,
            "memu_log_prompts":false,"admission_error":mock.admission_error.lock().unwrap().clone()}}))
    }

    async fn mock_status(
        mock: web::Data<MockNative>,
        query: web::Query<HashMap<String, String>>,
        request: HttpRequest,
    ) -> HttpResponse {
        let turn_id = query.get("turn_id").cloned().unwrap_or_else(|| mock.turn_id.lock().unwrap().clone());
        if turn_id.is_empty() {
            return HttpResponse::Ok().json(
                serde_json::json!({"session_id":null,"writer_idle":true,"turn":null,
                    "context":{"estimated_tokens":0,"overhead_tokens":0,"system_tokens":0}}),
            );
        }
        if matches!(mock.mode, "retry" | "continuation_refused")
            && mock.credential.lock().unwrap().is_empty()
        {
            return HttpResponse::Ok().json(serde_json::json!({
                "session_id":"native-id", "writer_idle":true,
                "context":{"estimated_tokens":500,"overhead_tokens":50,"system_tokens":25}, "turn":{
                    "session_id":"native-id", "turn_id":turn_id,
                    "attempt_id":mock.attempt_id.lock().unwrap().clone(),
                    "purpose":"reply", "status":"failed", "submission":"recorded",
                    "initialized":true, "input_recorded":true
                }
            }));
        }
        if matches!(
            mock.mode,
            "refused_later" | "recap_refused" | "continuation_refused" | "missing_receipt" | "recap_missing_receipt"
        ) {
            return HttpResponse::Conflict().json(serde_json::json!({"detail":{
                "code":"atomic_native_turn_missing", "session_id":"native-id", "turn":null, "writer_idle":true,
                "context":{"estimated_tokens":500,"overhead_tokens":50,"system_tokens":25},
            }}));
        }
        if matches!(mock.mode, "refused" | "server_refused" | "missing_unbound") {
            return HttpResponse::Ok().json(
                serde_json::json!({"session_id":null,"writer_idle":true,"turn":null,
                    "context":{"estimated_tokens":0,"overhead_tokens":0,"system_tokens":0}}),
            );
        }
        let first = mock.status_reads.fetch_add(1, Ordering::Relaxed) == 0;
        let active = mock.mode == "active" && first;
        let older_attempt = mock.mode == "retry" && first;
        if active || older_attempt {
            let url = format!(
                "http://{}{CHAT_TOOLS_PATH}",
                request.connection_info().host()
            );
            let credential = mock.credential.lock().unwrap().clone();
            let response = reqwest::Client::new()
                .post(url)
                .header("Authorization", credential)
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap();
            assert_ne!(
                response.status().as_u16(),
                401,
                "delivery loss must not revoke the live writer's lease"
            );
        }
        let status = match mock.mode {
            "success" | "route" | "recover" | "recap" | "mismatch" | "reload" => "complete",
            "retry" if !older_attempt => "complete",
            "active" if !active => "complete",
            "failed" | "retry" | "reload_failed" => "failed",
            _ => "uncertain",
        };
        let mut receipt_counts = counts();
        receipt_counts["cache_read_input_tokens"] = serde_json::json!(321);
        let purpose = mock.purpose.lock().unwrap().clone();
        let maintenance = {
            let mut saved = mock.maintenance.lock().unwrap();
            if let Some(receipt) = saved.as_mut().filter(|receipt|
                receipt["action"] == "purge" && receipt["status"] == "failed") {
                // MCP status retries interrupted filesystem removal, not publication.
                receipt["status"] = serde_json::json!("complete");
                receipt["error"] = Value::Null;
            }
            saved.clone()
        };
        HttpResponse::Ok().json(serde_json::json!({"session_id":"native-id","writer_idle":!active,
            "maintenance":maintenance,
            "context":{"estimated_tokens":if mock.mode == "route" { mock.context_tokens.load(Ordering::Relaxed) } else if active || older_attempt { 9000 } else { 700 },"overhead_tokens":50,"system_tokens":25},
            "turn":{"session_id":"native-id","turn_id":turn_id,"attempt_id":if older_attempt { "older-attempt".to_string() } else { mock.attempt_id.lock().unwrap().clone() },"purpose":purpose,
                "status":status,"submission":"recorded","initialized":true,
                "usage":{"message_id":"message","usage":receipt_counts},
                "error":if status == "failed" { Some("Saved native failure") } else { None },
                "text":if purpose == "recap" { Some("Exact recap") }
                    else if matches!(mock.mode, "success" | "route") { Some("Exact terminal response") }
                    else if status == "complete" { Some("Recovered terminal response") } else { None }}}))
    }

    #[actix_web::test]
    async fn native_runner_preserves_terminal_text_usage_and_uncertainty_then_revokes_tools() {
        for mode in [
            "success",
            "route",
            "failed",
            "uncertain",
            "recover",
            "refused",
            "server_refused",
            "continuation_refused",
            "active",
            "recap",
            "retry",
            "mismatch",
            "reload",
            "lost",
            "recap_lost",
            "reload_failed",
            "refused_later",
            "recap_refused",
            "recap_unknown",
            "recap_missing_receipt",
            "missing_receipt",
            "missing_unbound",
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let manager = Arc::new(atomic_core::DatabaseManager::new(dir.path()).unwrap());
            let core = manager.active_core().await.unwrap();
            let db = Db(core, manager.active_id().unwrap());
            let state = AppState {
                manager,
                event_tx: tokio::sync::broadcast::channel(32).0,
                public_url: None,
                log_buffer: crate::log_buffer::LogBuffer::new(4),
                memu_session: None,
                export_jobs: crate::export_jobs::ExportJobManager::for_tests(
                    dir.path().join("exports"),
                ),
                setup_token: None,
                dangerously_skip_setup_token: false,
                setup_claim_lock: tokio::sync::Mutex::new(()),
                setup_claim_limiter: crate::state::SetupClaimLimiter::new(),
            };
            let bridge = ChatToolBridge::default();
            let mock = web::Data::new(MockNative {
                mode,
                credential: Mutex::new(String::new()),
                attempt_id: Mutex::new(String::new()),
                turn_id: Mutex::new(String::new()),
                maintenance: Mutex::new(None),
                status_reads: AtomicUsize::new(0),
                purpose: Mutex::new(if mode.starts_with("recap") { "recap" } else { "reply" }.into()),
                context_tokens: AtomicUsize::new(90000),
                publications: Mutex::new(Vec::new()),
                purge_calls: AtomicUsize::new(0),
                reply_calls: AtomicUsize::new(0),
                publication_calls: AtomicUsize::new(0),
                admission_error: Mutex::new(String::new()),
            });
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            bridge.set_listeners(&[listener.local_addr().unwrap()]);
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let factory_bridge = bridge.clone();
            let factory_mock = mock.clone();
            let factory_core = db.0.clone();
            let server = HttpServer::new(move || {
                App::new()
                    .app_data(web::Data::new(factory_bridge.clone()))
                    .app_data(factory_mock.clone())
                    .app_data(web::Data::new(factory_core.clone()))
                    .route("/integration/atomic/chat", web::post().to(mock_chat))
                    .route("/integration/atomic/session_end", web::post().to(mock_session_end))
                    .route("/owner", web::get().to(|| async { HttpResponse::Ok().json(serde_json::json!({"user_id":"TestOwner"})) }))
                    .route("/souls", web::get().to(|| async { HttpResponse::Ok().json(serde_json::json!({"souls":["TestSoul"]})) }))
                    .route("/integration/atomic/chat_profile", web::get().to(mock_profile))
                    .route("/integration/atomic/chat/maintenance", web::post().to(mock_maintenance))
                    .route(
                        "/integration/atomic/chat/status",
                        web::get().to(mock_status),
                    )
                    .route(CHAT_TOOLS_PATH, web::to(crate::mcp::chat::chat_tools))
            })
            .workers(1)
            .listen(listener)
            .unwrap()
            .run();
            let handle = server.handle();
            actix_web::rt::spawn(server);
            let scope = MemuScope {
                base_url: base_url.clone(),
                user_id: "TestOwner".into(),
                soul_id: "TestSoul".into(),
            };
            let conv =
                db.0.create_conversation(&[], None, Some((&scope.user_id, &scope.soul_id)))
                    .await
                    .unwrap();
            let id = &conv.conversation.id;
            let profile = ClaudeChatProfile {
                model: "fictional".into(),
                context_window_tokens: 128000,
                timeout_seconds: 1,
                effort: "medium".into(),
                fingerprint: "fingerprint".into(),
                memu_response_sentences: 5,
                memu_log_prompts: false,
                admission_error: String::new(),
            };
            let request = ChatRequest::New {
                content: "fictional input".into(),
                page_context: None,
                canvas_context: None,
            };
            let mut prepared = None;
            let mut _prepared_work = None;
            if matches!(mode, "success" | "route") {
                // Creation binds the backend before saving the first input.
                db.0.set_chat_phase(
                    id,
                    &scope.user_id,
                    &scope.soul_id,
                    &Backend::ClaudeCode,
                    &Phase::Open,
                )
                .await
                .unwrap();
                db.0.save_message(id, "system", "saved presence")
                    .await
                    .unwrap();
                let empty = read_chat(&db, id).await.unwrap();
                assert!(!needs_activity_recap(
                    &empty,
                    &rows_for_saved_history(&empty)
                ));
                db.0.set_setting(
                    "chat_prompt",
                    "Local override must not leak into native input",
                )
                .await
                .unwrap();
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let before = serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap();
                assert!(AtomicChatProfileResponse::Api {
                    settings: HashMap::new()
                }
                .check_binding(&read_chat(&db, id).await.unwrap())
                .is_err());
                let first = prepare_input(&db, &scope, id, &request, &profile, &work)
                    .await
                    .unwrap();
                assert_eq!(first.operation, Operation::New);
                assert!(!first.continuation);
                assert_eq!(first.context_tokens, Some(first.input_tokens));
                assert!(first
                    .input
                    .system_prompt
                    .as_deref()
                    .unwrap()
                    .ends_with("saved presence"));
                assert!(!first
                    .input
                    .system_prompt
                    .as_deref()
                    .unwrap()
                    .contains("Local override"));
                assert!(first.input.prompt.contains("fictional input"));
                assert_eq!(
                    serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap(),
                    before
                );
                prepared = Some(first);
                _prepared_work = Some(work);
            }
            if mode == "route" {
                let mut state = state;
                state.memu_session = Some(crate::state::MemuSessionConfig { base_url: base_url.clone() });
                let state = web::Data::new(state);
                let mut small = profile.clone();
                small.context_window_tokens = 20_001;
                let work = _prepared_work.take().unwrap();
                let before = serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap();
                let error = chat_work_with_profile(
                    state.clone(),
                    Db(db.0.clone(), db.1.clone()),
                    scope.clone(),
                    id.clone(),
                    request.clone(),
                    web::Data::new(bridge.clone()),
                    AtomicChatProfileResponse::ClaudeCode { cli: small },
                    work,
                )
                .await
                .unwrap_err();
                assert!(error.1["error"]
                    .as_str()
                    .unwrap()
                    .contains("too long even for a fresh sitting"));
                assert_eq!(
                    serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap(),
                    before
                );
                let mut unavailable = profile.clone();
                unavailable.admission_error = "Install or repair Atomic's Claude session helper".into();
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let error = chat_work_with_profile(
                    state.clone(), Db(db.0.clone(), db.1.clone()), scope.clone(), id.clone(),
                    request.clone(), web::Data::new(bridge.clone()),
                    AtomicChatProfileResponse::ClaudeCode { cli: unavailable }, work,
                ).await.unwrap_err();
                assert!(error.1["error"].as_str().unwrap().contains("Install or repair"));
                assert_eq!(serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap(), before);
                assert!(mock.attempt_id.lock().unwrap().is_empty());
                let result = chat_work(
                    state.clone(),
                    Db(db.0.clone(), db.1.clone()),
                    scope.clone(),
                    id.clone(),
                    request,
                    web::Data::new(bridge.clone()),
                )
                .await
                .unwrap();
                assert_eq!(result.message.content, "Exact terminal response");
                let saved = read_chat(&db, id).await.unwrap();
                let lifecycle = saved.conversation.atomic_chat_state.as_ref().unwrap();
                assert_eq!(lifecycle.backend, Backend::ClaudeCode);
                assert_eq!(lifecycle.native_context_tokens, Some(90000));
                let maintenance = lifecycle.maintenance.as_ref().unwrap();
                assert!(maintenance.awaiting_measurement);
                assert_eq!(maintenance.status, atomic_core::AtomicChatMaintenanceStatus::Complete);
                assert_eq!(lifecycle.context_window_tokens, Some(128000));
                assert!(lifecycle.active.is_none());
                assert_eq!(saved.messages.len(), 3);

                // Reload must return saved state without queueing behind a live worker.
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let reads = mock.status_reads.load(Ordering::Relaxed);
                let request = actix_web::test::TestRequest::get()
                    .insert_header(("X-OpenAlma-User", "TestOwner"))
                    .insert_header(("X-OpenAlma-Soul", "TestSoul"))
                    .to_http_request();
                let response = tokio::time::timeout(Duration::from_secs(2), get_conversation(
                    request, state.clone(), Db(db.0.clone(), db.1.clone()),
                    web::Path::from(id.clone()), web::Data::new(bridge.clone()),
                )).await.expect("reload queued behind the sitting lock");
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = actix_web::body::to_bytes(response.into_body()).await.unwrap();
                let reloaded: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(reloaded["atomic_chat_state"]["phase"], "open");
                assert_eq!(reloaded["context_meter"]["estimated_tokens"], 90000);
                assert_eq!(mock.status_reads.load(Ordering::Relaxed), reads);
                drop(work);

                // Crossing the reserve boundary closes admission without saving the draft
                // or starting another ordinary cleanup/model turn.
                mock.context_tokens.store(108000, Ordering::Relaxed);
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let error = chat_work_with_profile(
                    state.clone(), Db(db.0.clone(), db.1.clone()), scope.clone(), id.clone(),
                    ChatRequest::New { content: "unsent next draft".into(), page_context: None, canvas_context: None },
                    web::Data::new(bridge.clone()),
                    AtomicChatProfileResponse::ClaudeCode { cli: profile.clone() }, work,
                ).await.unwrap_err();
                assert!(error.1["error"].as_str().unwrap().contains("sitting is closing"));
                let closing = read_chat(&db, id).await.unwrap();
                assert_eq!(closing.conversation.atomic_chat_state.as_ref().unwrap().phase, Phase::Closing);
                assert!(closing.messages.iter().all(|row| row.message.content != "unsent next draft"));
                assert_eq!(rows_for_saved_history(&closing).len(), rows_for_saved_history(&saved).len());
                assert_eq!(mock.reply_calls.load(Ordering::Relaxed), 1);

                tokio::time::timeout(Duration::from_secs(10), atomic_core::executor::wait_idle()).await.unwrap();
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let unpublished = read_chat(&db, id).await.unwrap();
                assert_eq!(unpublished.conversation.atomic_chat_state.as_ref().unwrap().phase, Phase::Closing);
                assert_eq!(existing_recap(&unpublished).as_deref(), Some("Exact recap"));
                let recap_attempt = mock.attempt_id.lock().unwrap().clone();
                assert!(mock.publications.lock().unwrap().is_empty());
                assert!(!mock.admission_error.lock().unwrap().is_empty());
                // Publication of the saved recap needs no helper or new model call.
                finish_memu_session(state.clone(), Db(db.0.clone(), db.1.clone()), scope.clone(), id, &bridge, &work).await.unwrap();
                assert_eq!(*mock.attempt_id.lock().unwrap(), recap_attempt);
                assert_eq!(mock.publication_calls.load(Ordering::Relaxed), 2);
                let closed = read_chat(&db, id).await.unwrap();
                let lifecycle = closed.conversation.atomic_chat_state.as_ref().unwrap();
                assert_eq!(lifecycle.phase, Phase::Closed);
                assert_eq!(existing_recap(&closed).as_deref(), Some("Exact recap"));
                assert_eq!(lifecycle.maintenance.as_ref().unwrap().kind, atomic_core::AtomicChatMaintenanceKind::Purge);
                assert_eq!(lifecycle.maintenance.as_ref().unwrap().status, atomic_core::AtomicChatMaintenanceStatus::Failed);
                assert!(lifecycle.maintenance.as_ref().unwrap().error.as_deref().unwrap().contains("storage removal"));
                assert_eq!(mock.publications.lock().unwrap().len(), 1);
                let recap_attempt = mock.attempt_id.lock().unwrap().clone();
                let ended = finish_memu_session(
                    state.clone(), Db(db.0.clone(), db.1.clone()), scope.clone(), id, &bridge, &work,
                ).await.unwrap();
                assert_eq!(ended["status"], "already_ended");
                let recovered = read_chat(&db, id).await.unwrap();
                let lifecycle = recovered.conversation.atomic_chat_state.as_ref().unwrap();
                assert_eq!(lifecycle.phase, Phase::Closed);
                assert_eq!(lifecycle.maintenance.as_ref().unwrap().status, atomic_core::AtomicChatMaintenanceStatus::Complete);
                assert_eq!(*mock.attempt_id.lock().unwrap(), recap_attempt);
                assert_eq!(mock.purge_calls.load(Ordering::Relaxed), 1);
                assert_eq!(mock.publications.lock().unwrap().len(), 1);
                let reads = mock.status_reads.load(Ordering::Relaxed);
                finish_memu_session(state.clone(), Db(db.0.clone(), db.1.clone()), scope.clone(), id, &bridge, &work).await.unwrap();
                assert_eq!(mock.status_reads.load(Ordering::Relaxed), reads);
                drop(work);
                let original_attempt = mock.attempt_id.lock().unwrap().clone();
                let user = saved
                    .messages
                    .iter()
                    .find(|row| row.message.role == "user")
                    .unwrap();
                handle.stop(true).await;
                let recovered = chat_work(
                    state,
                    Db(db.0.clone(), db.1.clone()),
                    scope.clone(),
                    id.clone(),
                    ChatRequest::Retry {
                        user_message_id: user.message.id.clone(),
                    },
                    web::Data::new(bridge.clone()),
                )
                .await
                .unwrap();
                assert_eq!(recovered.message.id, result.message.id);
                assert_eq!(*mock.attempt_id.lock().unwrap(), original_attempt);
                continue;
            }
            let mut input =
                db.0.admit_chat_turn(
                    id,
                    &scope.user_id,
                    &scope.soul_id,
                    &Backend::ClaudeCode,
                    &request,
                    128000,
                )
                .await
                .unwrap();
            if mode.starts_with("recap") {
                db.0.fail_chat_turn(
                    id,
                    &input.id,
                    "Recorded failure",
                    mode == "recap_lost",
                    None,
                )
                .await
                .unwrap();
                db.0.set_chat_phase(
                    id,
                    &scope.user_id,
                    &scope.soul_id,
                    &Backend::ClaudeCode,
                    &Phase::Closing,
                )
                .await
                .unwrap();
                input =
                    db.0.admit_chat_turn(
                        id,
                        &scope.user_id,
                        &scope.soul_id,
                        &Backend::ClaudeCode,
                        &ChatRequest::Recap {
                            instruction: "Recap control".into(),
                        },
                        128000,
                    )
                    .await
                    .unwrap();
            }
            if matches!(mode, "retry" | "continuation_refused") {
                let previous_attempt = read_chat(&db, id)
                    .await
                    .unwrap()
                    .conversation
                    .atomic_chat_state
                    .unwrap()
                    .active
                    .unwrap()
                    .native_attempt_id
                    .unwrap();
                let previous = AtomicChatNativeOutcome {
                    attempt_id: previous_attempt.clone(),
                    usage: None,
                    input_recorded: Some(true),
                    context_tokens: Some(500),
                };
                db.0.fail_chat_turn(
                    id,
                    &input.id,
                    "Previous attempt failed",
                    false,
                    Some(&previous),
                )
                .await
                .unwrap();
                *mock.attempt_id.lock().unwrap() = previous_attempt;
                let retry = ChatRequest::Retry {
                    user_message_id: input.id.clone(),
                };
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                prepared = Some(
                    prepare_input(&db, &scope, id, &retry, &profile, &work)
                        .await
                        .unwrap(),
                );
                assert!(prepared.as_ref().unwrap().continuation);
                assert!(prepared
                    .as_ref()
                    .unwrap()
                    .input
                    .prompt
                    .contains(RETRY_CONTINUATION));
                assert!(!prepared
                    .as_ref()
                    .unwrap()
                    .input
                    .prompt
                    .contains("fictional input"));
                _prepared_work = Some(work);
                input =
                    db.0.admit_chat_turn(
                        id,
                        &scope.user_id,
                        &scope.soul_id,
                        &Backend::ClaudeCode,
                        &retry,
                        128000,
                    )
                    .await
                    .unwrap();
            }
            let session =
                db.0.chat_tool_session(
                    id,
                    &input.id,
                    HashMap::new(),
                    tools_for(&scope),
                    None,
                    None,
                    |_| {},
                )
                .await
                .unwrap();
            let lifecycle =
                db.0.get_conversation(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .conversation
                    .atomic_chat_state
                    .unwrap();
            let attempt = if mode.starts_with("recap") {
                lifecycle.recap.unwrap().native_attempt_id.unwrap()
            } else {
                lifecycle.active.unwrap().native_attempt_id.unwrap()
            };
            if matches!(
                mode,
                "reload"
                    | "lost"
                    | "recap_lost"
                    | "reload_failed"
                    | "refused_later"
                    | "recap_refused"
                    | "recap_missing_receipt"
                    | "missing_receipt"
                    | "missing_unbound"
            ) {
                *mock.attempt_id.lock().unwrap() = attempt;
                let _ = session.finish();
                if matches!(mode, "lost" | "missing_receipt" | "missing_unbound") {
                    let saved = db.0.get_conversation(id).await.unwrap().unwrap();
                    let row = saved
                        .messages
                        .iter()
                        .find(|row| row.message.id == input.id)
                        .unwrap();
                    let mut records = atomic_core::agent::ChatToolRecords::from_saved_message(row);
                    records.tool_calls.push(atomic_core::ChatToolCall {
                        id: "stranded-tool".into(),
                        message_id: input.id.clone(),
                        tool_name: "focus_atom".into(),
                        tool_input: serde_json::json!({}),
                        tool_output: None,
                        status: "running".into(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        completed_at: None,
                    });
                    db.0.save_chat_tool_records(id, &input.id, &records)
                        .await
                        .unwrap();
                }
                if matches!(mode, "reload_failed" | "refused_later" | "recap_refused") {
                    let refused = AtomicChatNativeOutcome {
                        attempt_id: mock.attempt_id.lock().unwrap().clone(),
                        usage: None,
                        input_recorded: Some(false),
                        context_tokens: Some(123),
                    };
                    db.0.fail_chat_turn(
                        id,
                        &input.id,
                        "Earlier delivery failed",
                        mode == "reload_failed",
                        if mode == "reload_failed" {
                            None
                        } else {
                            Some(&refused)
                        },
                    )
                    .await
                    .unwrap();
                }
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let other_work = lock_conversation(&db, "different-sitting", None)
                    .await
                    .unwrap();
                assert!(reconcile(&state, &db, &scope, &bridge, id, &other_work)
                    .await
                    .is_err());
                let mut events = state.event_tx.subscribe();
                let mut foreign = scope.clone();
                foreign.soul_id = "OtherSoul".into();
                assert!(reconcile(&state, &db, &foreign, &bridge, id, &work)
                    .await
                    .is_err());
                reconcile(&state, &db, &scope, &bridge, id, &work)
                    .await
                    .unwrap();
                if mode == "refused_later" {
                    let retry = ChatRequest::Retry {
                        user_message_id: input.id.clone(),
                    };
                    let PreparedInput {
                        operation,
                        continuation,
                        input: projected,
                        ..
                    } = prepare_input(&db, &scope, id, &retry, &profile, &work)
                        .await
                        .unwrap();
                    assert_eq!(operation, Operation::Retry);
                    assert!(!continuation);
                    assert!(projected.system_prompt.is_none());
                    assert!(projected.prompt.contains("fictional input"));
                    assert!(!projected.prompt.contains(RETRY_CONTINUATION));
                }
                let recovered = db.0.get_conversation(id).await.unwrap().unwrap();
                let saved = recovered.conversation.atomic_chat_state.unwrap();
                assert_eq!(
                    saved.native_context_tokens,
                    if matches!(mode, "missing_receipt" | "missing_unbound" | "recap_missing_receipt") {
                        None
                    } else {
                        Some(if matches!(mode, "refused_later" | "recap_refused") {
                            123
                        } else {
                            700
                        })
                    }
                );
                if mode == "reload" {
                    assert!(saved.active.is_none());
                    assert_eq!(recovered.messages.len(), 2);
                    assert_eq!(
                        recovered.messages[1].message.content,
                        "Recovered terminal response"
                    );
                } else if matches!(mode, "recap_lost" | "recap_refused" | "recap_missing_receipt") {
                    assert!(matches!(
                        saved.recap.unwrap().outcome,
                        RecapOutcome::Failed { .. }
                    ));
                    assert_eq!(
                        saved.active.unwrap().status,
                        if mode == "recap_lost" {
                            TurnStatus::Uncertain
                        } else {
                            TurnStatus::Failed
                        }
                    );
                    assert_eq!(recovered.messages.len(), 2);
                    assert!(db
                        .0
                        .validate_chat_request(
                            id,
                            &scope.user_id,
                            &scope.soul_id,
                            &Backend::ClaudeCode,
                            &ChatRequest::Recap {
                                instruction: "Retry the recap".into()
                            },
                            128000
                        )
                        .await
                        .is_ok());
                } else if matches!(mode, "reload_failed" | "refused_later") {
                    assert_eq!(saved.active.unwrap().status, TurnStatus::Failed);
                    assert!(db
                        .0
                        .validate_chat_request(
                            id,
                            &scope.user_id,
                            &scope.soul_id,
                            &Backend::ClaudeCode,
                            &ChatRequest::Retry {
                                user_message_id: input.id.clone()
                            },
                            128000
                        )
                        .await
                        .is_ok());
                } else {
                    assert_eq!(saved.active.unwrap().status, TurnStatus::Uncertain);
                    assert_eq!(recovered.messages.len(), 1);
                    assert_eq!(recovered.messages[0].tool_calls[0].status, "failed");
                    assert!(recovered.messages[0].tool_calls[0].outcome_unknown());
                }
                if matches!(mode, "missing_receipt" | "missing_unbound") {
                    assert!(
                        db.0.validate_chat_request(
                            id,
                            &scope.user_id,
                            &scope.soul_id,
                            &Backend::ClaudeCode,
                            &ChatRequest::Retry {
                                user_message_id: input.id.clone()
                            },
                            128000
                        )
                        .await
                        .is_err(),
                        "writer exit does not authorize replay"
                    );
                    let recap = ChatRequest::Recap {
                        instruction: "Recap the existing sitting".into(),
                    };
                    let prepared = prepare_input(&db, &scope, id, &recap, &profile, &work).await;
                    if mode == "missing_receipt" {
                        let prepared = prepared.unwrap();
                        assert_eq!(prepared.operation, Operation::Continue);
                        assert!(prepared.input.system_prompt.is_none());
                        assert_eq!(prepared.input.prompt, "Recap the existing sitting\n\n**respond with 5 sentences or fewer**");
                    } else {
                        assert!(
                            prepared.is_err(),
                            "missing native history must not initialize a replacement for recap"
                        );
                    }
                }
                assert!(
                    mock.credential.lock().unwrap().is_empty(),
                    "reconciliation must not call the native producer"
                );
                while events.try_recv().is_ok() {}
                reconcile(&state, &db, &scope, &bridge, id, &work)
                    .await
                    .unwrap();
                assert!(
                    events.try_recv().is_err(),
                    "reload reconciliation must not create an event loop"
                );
                assert_eq!(
                    db.0.get_conversation(id)
                        .await
                        .unwrap()
                        .unwrap()
                        .messages
                        .len(),
                    recovered.messages.len()
                );
                handle.stop(true).await;
                continue;
            }
            let turn = NativeTurn {
                conversation_id: id,
                turn_id: &input.id,
                attempt_id: &attempt,
                operation: if mode.starts_with("recap") {
                    Operation::Continue
                } else if matches!(mode, "retry" | "continuation_refused") {
                    Operation::Retry
                } else {
                    Operation::New
                },
                purpose: if mode.starts_with("recap") {
                    Purpose::Recap
                } else {
                    Purpose::Reply
                },
                continuation: matches!(mode, "retry" | "continuation_refused"),
                input: ClaudeChatInput {
                    prompt: input.content.clone(),
                    system_prompt: (!mode.starts_with("recap")
                        && !matches!(mode, "retry" | "continuation_refused"))
                    .then(|| "saved presence".into()),
                },
                input_tokens: 100,
            };
            if mode == "recover" {
                *mock.attempt_id.lock().unwrap() = attempt.clone();
                let mut expired_stream = NativeStream::default();
                let recovered = observe(&scope, &turn, &mut expired_stream, Duration::ZERO)
                    .await
                    .unwrap();
                assert_eq!(recovered.turn.unwrap().status, "complete");
                assert_eq!(expired_stream.usage.unwrap().cache_read_input_tokens, 321);
            }
            let mut events = state.event_tx.subscribe();
            let result = if let Some(prepared) = prepared {
                let _ = session.finish();
                execute_prepared(
                    &state,
                    &db,
                    &scope,
                    &profile,
                    &bridge,
                    &base_url,
                    id,
                    &input.id,
                    Purpose::Reply,
                    prepared,
                )
                .await
            } else {
                execute(
                    &state, &db, &scope, &profile, &bridge, &base_url, turn, session,
                )
                .await
            };
            if mode == "failed" {
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let retry = ChatRequest::Retry {
                    user_message_id: input.id.clone(),
                };
                let before = serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap();
                let PreparedInput {
                    operation,
                    continuation,
                    input: projected,
                    input_tokens: tokens,
                    ..
                } = prepare_input(&db, &scope, id, &retry, &profile, &work)
                    .await
                    .unwrap();
                assert_eq!(operation, Operation::Retry);
                assert!(continuation);
                assert!(projected.system_prompt.is_none());
                assert!(projected.prompt.contains(RETRY_CONTINUATION));
                assert!(!projected.prompt.contains("fictional input"));
                assert!(projected
                    .prompt
                    .ends_with("**respond with 5 sentences or fewer**"));
                assert!(tokens > atomic_core::chunking::count_tokens(&projected.prompt));
                assert_eq!(
                    serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap(),
                    before
                );
                assert!(
                    prepare_input(&db, &scope, "another-sitting", &retry, &profile, &work)
                        .await
                        .is_err()
                );
            }
            let saved = db.0.get_conversation(id).await.unwrap().unwrap();
            if matches!(mode, "refused" | "server_refused") {
                assert!(!needs_activity_recap(
                    &saved,
                    &rows_for_saved_history(&saved)
                ));
            } else if matches!(mode, "failed" | "uncertain") {
                assert!(
                    needs_activity_recap(&saved, &rows_for_saved_history(&saved)),
                    "recorded native work needs a recap even without a saved assistant row"
                );
            }
            let lifecycle = saved.conversation.atomic_chat_state.unwrap();
            assert_eq!(
                lifecycle.native_context_tokens,
                Some(if matches!(mode, "refused" | "server_refused") {
                    0
                } else if mode == "continuation_refused" {
                    500
                } else {
                    700
                })
            );
            if matches!(mode, "refused" | "server_refused") {
                assert!(lifecycle.native_usage.is_none());
                assert_eq!(
                    lifecycle.active.as_ref().unwrap().native_input_recorded,
                    Some(false)
                );
            } else if mode == "continuation_refused" {
                assert!(lifecycle.native_usage.is_none());
                assert_eq!(
                    lifecycle.active.as_ref().unwrap().native_input_recorded,
                    Some(true)
                );
            } else {
                assert_eq!(lifecycle.native_usage.unwrap().cache_read_input_tokens, 321);
            }
            assert!(saved.messages.iter().all(|row| row.tool_calls.is_empty()));
            match mode {
                "recap" | "recap_unknown" => {
                    if mode == "recap_unknown" {
                        assert!(result.is_err());
                        assert!(matches!(
                            lifecycle.recap.unwrap().outcome,
                            RecapOutcome::Failed { .. }
                        ));
                    } else {
                        assert_eq!(result.unwrap().message.content, "Exact recap");
                        assert!(matches!(
                            lifecycle.recap.unwrap().outcome,
                            RecapOutcome::Complete { .. }
                        ));
                    }
                    while let Ok(event) = events.try_recv() {
                        assert!(!matches!(
                            event,
                            ServerEvent::ChatStreamDelta { .. } | ServerEvent::ChatComplete { .. }
                        ));
                    }
                }
                "success" | "recover" | "active" | "retry" | "mismatch" => {
                    let expected = if mode == "success" {
                        "Exact terminal response"
                    } else {
                        "Recovered terminal response"
                    };
                    assert_eq!(result.unwrap().message.content, expected);
                    assert_eq!(saved.messages.last().unwrap().message.content, expected);
                    assert!(lifecycle.active.is_none());
                }
                _ => {
                    if mode == "failed" {
                        assert_eq!(result.unwrap_err().1["error"], "Saved native failure");
                    } else {
                        assert!(result.is_err());
                    }
                    assert_eq!(saved.messages.len(), 1);
                    assert_eq!(
                        lifecycle.active.unwrap().status,
                        if mode == "uncertain" {
                            TurnStatus::Uncertain
                        } else {
                            TurnStatus::Failed
                        }
                    );
                }
            }
            if !mode.starts_with("recap") {
                let credential = mock.credential.lock().unwrap().clone();
                let revoked = reqwest::Client::new()
                    .post(format!("{base_url}{CHAT_TOOLS_PATH}"))
                    .header("Authorization", credential)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(revoked.status().as_u16(), 401);
            }
            handle.stop(true).await;
        }
    }
}
