//! Dormant native execution; admission/recovery stays in the sitting lifecycle.

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
    let response = reqwest::Client::new()
        .get(format!("{}/integration/atomic/chat/status", scope.base_url))
        .query(&[
            ("user_id", scope.user_id.as_str()),
            ("soul_id", scope.soul_id.as_str()),
            ("conversation_id", id),
            ("turn_id", turn_id),
        ])
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
/// Dormant until N2c route wiring; no model request or new admission here.
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
    if saved.phase == Phase::Closed
        || saved
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
    // An explicit pre-submission refusal can have no matching MCP receipt.
    if was_failed && !matched && idle {
        return Ok(());
    }
    if observed
        .as_ref()
        .is_ok_and(|status| status.writer_idle == Some(false))
    {
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":"Native work is still running. Wait, or open a new independent sitting; this history is retained."}),
        ));
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
    if idle && matched {
        let mut changed_tools = false;
        for (message_id, records) in bridge
            .drain_sitting(&scope.user_id, &scope.soul_id, work)
            .await
        {
            db.0.save_chat_tool_records(id, &message_id, &records)
                .await
                .map_err(core_json_error)?;
            changed_tools = true;
        }
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
    blocks: std::collections::BTreeMap<usize, String>,
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
                    let key = identity.message_id.unwrap_or(identity.record_id);
                    if self.text_message.as_ref() != Some(&key) {
                        self.blocks.clear();
                        self.text_message = Some(key);
                    }
                    self.blocks.insert(identity.block_index, text);
                    on_event(ChatEvent::StreamDelta {
                        conversation_id: turn.conversation_id.into(),
                        content: self.blocks.values().cloned().collect::<Vec<_>>().join(""),
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
/// Production entry points remain gated until N4; N2c supplies reconciled input.
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
                    context_tokens: read_chat(db, turn.conversation_id)
                        .await?
                        .conversation
                        .atomic_chat_state
                        .and_then(|state| state.native_context_tokens),
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
        status_reads: AtomicUsize,
    }

    fn counts() -> Value {
        serde_json::json!({"input_tokens":20,"cache_creation_input_tokens":30,
            "cache_read_input_tokens":100,"output_tokens":5})
    }

    #[test]
    fn context_projection_adds_only_uncovered_input_and_overhead_growth() {
        let mut input = ClaudeChatInput {
            prompt: "a new turn".into(),
            system_prompt: None,
        };
        let prompt = atomic_core::chunking::count_tokens(&input.prompt) + 8;
        let context = NativeContext {
            estimated_tokens: 1000,
            overhead_tokens: 100,
            system_tokens: 25,
        };
        assert_eq!(context.project(&input, prompt + 100), 1000 + prompt);
        assert_eq!(context.project(&input, prompt + 150), 1050 + prompt);
        input.system_prompt = Some("original Presence".into());
        let system = atomic_core::chunking::count_tokens(input.system_prompt.as_deref().unwrap());
        let same = NativeContext {
            system_tokens: system,
            ..context
        };
        assert_eq!(same.project(&input, prompt + system + 100), 1000 + prompt);
        let fresh = NativeContext {
            estimated_tokens: 0,
            overhead_tokens: 0,
            system_tokens: 0,
        };
        assert_eq!(
            fresh.project(&input, prompt + system + 100),
            prompt + system + 100
        );
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
        for (index, text, parent) in [
            (0, "old", None),
            (0, "first", None),
            (1, " second", None),
            (0, "nested", Some("parent")),
        ] {
            accept(
                serde_json::json!({"type":"text","record_id":"record","message_id":"message","parent_call_id":parent,
                "block_index":index,"mode":"replace","text":text}),
            );
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
            ["old", "first", "first second"]
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
        *mock.attempt_id.lock().unwrap() = body["attempt_id"].as_str().unwrap().into();
        if mock.mode.starts_with("recap") {
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
            "success" => frames.push(serde_json::json!({"type":"complete","session_id":"native-id","text":"Exact terminal response"})),
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

    async fn mock_status(
        mock: web::Data<MockNative>,
        query: web::Query<HashMap<String, String>>,
        request: HttpRequest,
    ) -> HttpResponse {
        if query["turn_id"].is_empty() {
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
                    "session_id":"native-id", "turn_id":query["turn_id"],
                    "attempt_id":mock.attempt_id.lock().unwrap().clone(),
                    "purpose":"reply", "status":"failed", "submission":"recorded",
                    "initialized":true, "input_recorded":true
                }
            }));
        }
        if matches!(
            mock.mode,
            "refused_later" | "recap_refused" | "continuation_refused"
        ) {
            return HttpResponse::Conflict().json(serde_json::json!({"detail":{
                "code":"atomic_native_turn_missing", "session_id":"native-id", "turn":null, "writer_idle":true,
                "context":{"estimated_tokens":500,"overhead_tokens":50,"system_tokens":25},
            }}));
        }
        if matches!(mock.mode, "refused" | "server_refused") {
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
            "success" | "recover" | "recap" | "mismatch" | "reload" => "complete",
            "retry" if !older_attempt => "complete",
            "active" if !active => "complete",
            "failed" | "retry" | "reload_failed" => "failed",
            _ => "uncertain",
        };
        let mut receipt_counts = counts();
        receipt_counts["cache_read_input_tokens"] = serde_json::json!(321);
        HttpResponse::Ok().json(serde_json::json!({"session_id":"native-id","writer_idle":!active,
            "context":{"estimated_tokens":500,"overhead_tokens":50,"system_tokens":25},
            "turn":{"session_id":"native-id","turn_id":query["turn_id"],"attempt_id":if older_attempt { "older-attempt".to_string() } else { mock.attempt_id.lock().unwrap().clone() },"purpose":if mock.mode.starts_with("recap") { "recap" } else { "reply" },
                "status":status,"submission":"recorded","initialized":true,
                "usage":{"message_id":"message","usage":receipt_counts},
                "error":if status == "failed" { Some("Saved native failure") } else { None },
                "text":if mock.mode.starts_with("recap") { Some("Exact recap") }
                    else if mock.mode == "success" { Some("Exact terminal response") }
                    else if status == "complete" { Some("Recovered terminal response") } else { None }}}))
    }

    #[actix_web::test]
    async fn native_runner_preserves_terminal_text_usage_and_uncertainty_then_revokes_tools() {
        for mode in [
            "success",
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
                status_reads: AtomicUsize::new(0),
            });
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let factory_bridge = bridge.clone();
            let factory_mock = mock.clone();
            let server = HttpServer::new(move || {
                App::new()
                    .app_data(web::Data::new(factory_bridge.clone()))
                    .app_data(factory_mock.clone())
                    .route("/integration/atomic/chat", web::post().to(mock_chat))
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
            };
            let request = ChatRequest::New {
                content: "fictional input".into(),
                page_context: None,
                canvas_context: None,
            };
            let mut prepared = None;
            let mut _prepared_work = None;
            if mode == "success" {
                // Native creation must bind its backend before first Send (remaining route work).
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
                db.0.set_setting(
                    "chat_prompt",
                    "Local override must not leak into native input",
                )
                .await
                .unwrap();
                let work = lock_conversation(&db, id, Some(&scope)).await.unwrap();
                let before = serde_json::to_value(read_chat(&db, id).await.unwrap()).unwrap();
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
            ) {
                *mock.attempt_id.lock().unwrap() = attempt;
                let _ = session.finish();
                if mode == "lost" {
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
                        context_tokens: Some(500),
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
                if mode == "reload" {
                    assert!(saved.active.is_none());
                    assert_eq!(recovered.messages.len(), 2);
                    assert_eq!(
                        recovered.messages[1].message.content,
                        "Recovered terminal response"
                    );
                } else if matches!(mode, "recap_lost" | "recap_refused") {
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
            let mut turn = NativeTurn {
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
            if let Some(prepared) = prepared {
                turn.operation = prepared.operation;
                turn.continuation = prepared.continuation;
                turn.input = prepared.input;
                turn.input_tokens = prepared.input_tokens;
            }
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
            let result = execute(
                &state, &db, &scope, &profile, &bridge, &base_url, turn, session,
            )
            .await;
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
            let lifecycle = saved.conversation.atomic_chat_state.unwrap();
            assert_eq!(
                lifecycle.native_context_tokens,
                Some(if matches!(mode, "refused" | "server_refused") {
                    0
                } else {
                    500
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
