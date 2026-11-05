//! Chat / Conversation routes

mod native;

use crate::db_extractor::Db;
use crate::error::{
    core_json_error, json_error_response, ok_or_error, ApiErrorResponse, JsonError,
};
use crate::event_bridge::chat_event_callback;
use crate::mcp::chat::ChatToolBridge;
use crate::routes::memu_proxy::{self, memu_error_text, MemuScope};
use crate::state::{AppState, ServerEvent};
use actix_web::{http::StatusCode, web, HttpRequest, HttpResponse};
use atomic_core::chat::is_atomic_recap_prompt;
use atomic_core::{
    AtomicChatBackend as Backend, AtomicChatPhase as Phase, AtomicChatRecapOutcome as RecapOutcome,
    AtomicChatRequest as ChatRequest, AtomicChatTurnStatus as TurnStatus,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::Duration,
};
use utoipa::{IntoParams, ToSchema};

type ConversationKey = (String, String);
pub(crate) struct ConversationWork {
    key: ConversationKey,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl ConversationWork {
    pub(crate) fn conversation_id(&self) -> &str { &self.key.1 }
    fn matches(&self, db: &Db, id: &str) -> bool { self.key.0 == db.1 && self.key.1 == id }
}
const LEGACY_CONTEXT_ERROR: &str = "This chat exceeds the context limit and cannot be closed out. Your saved history is unchanged. Start a new sitting, or contact help@openalma.org for assistance.";
static CONVERSATION_WORK: LazyLock<
    tokio::sync::Mutex<HashMap<ConversationKey, Arc<tokio::sync::Mutex<()>>>>,
> = LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

fn lost_chat_worker() -> HttpResponse {
    HttpResponse::InternalServerError().json(ApiErrorResponse {
        error:
            "Chat worker stopped before returning a result. Reload saved history before retrying."
                .to_string(),
    })
}

fn end_work_result(
    conversation_id: &str,
    result: Result<serde_json::Value, JsonError>,
) -> Result<serde_json::Value, JsonError> {
    if let Err((status, body)) = &result {
        tracing::error!(conversation_id, status = %status, error = %body, "Atomic End Session failed");
    }
    result
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateConversationBody {
    /// Tag IDs to scope the conversation
    #[serde(default)]
    pub tag_ids: Vec<String>,
    /// Optional conversation title
    pub title: Option<String>,
}

#[derive(Serialize)]
struct AtomicSessionStartRequest<'a> {
    user_id: &'a str,
    soul_id: &'a str,
    conversation_id: String,
}

#[derive(Deserialize)]
struct AtomicSessionStartResponse {
    snapshot_text: String,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
enum AtomicChatProfileResponse {
    Api { settings: HashMap<String, String> },
    ClaudeCode { cli: ClaudeChatProfile },
}

#[derive(Clone, Deserialize)]
struct ClaudeChatProfile {
    model: String,
    context_window_tokens: usize,
    timeout_seconds: u64,
    effort: String,
    fingerprint: String,
    memu_response_sentences: usize,
    memu_log_prompts: bool,
    admission_error: String,
}

impl ClaudeChatProfile {
    fn common_settings(&self) -> HashMap<String, String> {
        HashMap::from([
            ("memu_response_sentences".into(), self.memu_response_sentences.to_string()),
            ("memu_log_prompts".into(), self.memu_log_prompts.to_string()),
        ])
    }
}

impl AtomicChatProfileResponse {
    fn check_admission(&self) -> Result<(), JsonError> {
        if let Self::ClaudeCode { cli } = self {
            if !cli.admission_error.is_empty() {
                return Err((StatusCode::CONFLICT, serde_json::json!({"error":cli.admission_error})));
            }
        }
        Ok(())
    }

    fn backend(&self) -> Backend {
        match self {
            Self::Api { .. } => Backend::Api,
            Self::ClaudeCode { .. } => Backend::ClaudeCode,
        }
    }

    fn capacity(&self) -> Result<usize, JsonError> {
        match self {
            Self::Api { settings } => api_capacity(settings),
            Self::ClaudeCode { cli } if cli.context_window_tokens > 20_000 => {
                Ok(cli.context_window_tokens)
            }
            Self::ClaudeCode { .. } => Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":"Set claude_code_context_window_tokens above 20000 to the account's usable allowance"}),
            )),
        }
    }

    fn check_binding(&self, conv: &atomic_core::ConversationWithMessages) -> Result<(), JsonError> {
        let backend = conv
            .conversation
            .atomic_chat_state
            .as_ref()
            .map(|saved| saved.backend)
            .unwrap_or(Backend::Api);
        if backend != self.backend() {
            return Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":format!(
                "This sitting requires claude_code={}. Restore its original backend to continue/end it, or start a new sitting.",
                backend == Backend::ClaudeCode)}),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct AtomicSessionEndRequest {
    user_id: String,
    soul_id: String,
    conversation_id: String,
    activity_recap: Option<String>,
    transcript: Vec<AtomicTranscriptRow>,
}

#[derive(Clone, Serialize)]
struct AtomicTranscriptRow {
    role: String,
    content: String,
    created_at: String,
}

#[derive(Deserialize, ToSchema)]
pub struct EndMemuSessionBody {
    conversation_id: String,
}

fn atomic_recap_instruction(user_id: &str) -> String {
    format!(
        "This Atomic session is ending. Write a recap of your activity with {user_id}: what you looked at, what you changed (edits you made and why), ideas rejected or deferred, and follow-ups you want to remember. Write it as yourself, for yourself."
    )
}

async fn fetch_atomic_snapshot(
    config: &MemuScope,
    atomic_conversation_id: &str,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("memU session_start client failed: {e}"))?;
    let response = client
        .post(format!(
            "{}/integration/atomic/session_start",
            config.base_url
        ))
        .json(&AtomicSessionStartRequest {
            user_id: &config.user_id,
            soul_id: &config.soul_id,
            conversation_id: format!("chat:atomic-{atomic_conversation_id}"),
        })
        .send()
        .await
        .map_err(|e| format!("memU session_start request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("memU session_start failed ({status}): {body}"));
    }
    let body = response
        .json::<AtomicSessionStartResponse>()
        .await
        .map_err(|e| format!("memU session_start returned invalid JSON: {e}"))?;
    let snapshot = body.snapshot_text.trim().to_string();
    if snapshot.is_empty() {
        return Err("memU session_start returned empty snapshot_text".to_string());
    }
    Ok(snapshot)
}

async fn fetch_chat_profile(
    config: &MemuScope,
) -> Result<AtomicChatProfileResponse, JsonError> {
    let client = memu_proxy::client_data()?;
    let value = memu_proxy::memu_json_data(
        client
            .get(format!(
                "{}/integration/atomic/chat_profile",
                config.base_url
            ))
            .query(&[("user_id", &config.user_id), ("soul_id", &config.soul_id)]),
        "memU chat_profile",
    )
    .await?;
    let body = serde_json::from_value::<AtomicChatProfileResponse>(value).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            serde_json::json!({"error": format!("memU chat_profile returned invalid JSON: {e}")}),
        )
    })?;
    Ok(body)
}

async fn fetch_atomic_chat_profile(
    config: &MemuScope,
) -> Result<AtomicChatProfileResponse, JsonError> {
    let profile = fetch_chat_profile(config).await?;
    profile.capacity()?;
    let settings = match &profile {
        AtomicChatProfileResponse::Api { settings } => settings,
        AtomicChatProfileResponse::ClaudeCode { .. } => return Ok(profile),
    };
    let (model_key, url_key) = match settings.get("provider").map(String::as_str) {
        Some("openai_compat") => ("openai_compat_llm_model", Some("openai_compat_base_url")),
        Some("ollama") => ("ollama_llm_model", Some("ollama_host")),
        _ => ("chat_model", None),
    };
    if settings
        .get(model_key)
        .is_none_or(|model| model.trim().is_empty())
    {
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":"Configure a non-empty llm.chat_model in memU before continuing."}),
        ));
    }
    if let Some(key) = url_key {
        if !settings
            .get(key)
            .and_then(|value| reqwest::Url::parse(value).ok())
            .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        {
            return Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":"Configure a valid HTTP(S) llm.base_url in memU before continuing."}),
            ));
        }
    }
    let provider = atomic_core::providers::ProviderConfig::from_settings(settings);
    atomic_core::providers::create_streaming_llm_provider(&provider).map_err(|error| {
        (
            StatusCode::CONFLICT,
            serde_json::json!({"error":error.to_string()}),
        )
    })?;
    Ok(profile)
}

fn api_capacity(settings: &HashMap<String, String>) -> Result<usize, JsonError> {
    let key = match settings.get("provider").map(String::as_str) {
        Some("openai_compat") => "openai_compat_context_length",
        Some("ollama") => "ollama_context_length",
        Some("openrouter") => "openrouter_context_length",
        _ => {
            return Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":"Unsupported API chat provider in memU profile"}),
            ));
        }
    };
    settings.get(key).and_then(|value| value.parse::<usize>().ok()).filter(|value| *value > 20_000)
        .ok_or_else(|| (StatusCode::CONFLICT, serde_json::json!({"error":"Set llm.context_window_tokens above 20000 to the provider/account's usable allowance"})))
}

fn tools_for(scope: &MemuScope) -> atomic_core::MemuToolConfig {
    atomic_core::MemuToolConfig {
        base_url: scope.base_url.clone(),
        user_id: scope.user_id.clone(),
        soul_id: scope.soul_id.clone(),
    }
}

fn chat_changed(state: &AppState, db: &Db, id: &str) {
    let _ = state.event_tx.send(ServerEvent::ChatStateChanged {
        database_id: db.1.clone(),
        conversation_id: id.into(),
    });
}

async fn read_chat(db: &Db, id: &str) -> Result<atomic_core::ConversationWithMessages, JsonError> {
    db.0.get_conversation(id)
        .await
        .map_err(core_json_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                serde_json::json!({"error":"Conversation not found"}),
            )
        })
}

fn unknown_tools(row: &atomic_core::ChatMessageWithContext) -> bool {
    row.tool_calls.iter().any(atomic_core::ChatToolCall::outcome_unknown)
}

async fn change_phase(
    state: &AppState,
    db: &Db,
    scope: &MemuScope,
    id: &str,
    backend: &Backend,
    phase: Phase,
) -> Result<(), JsonError> {
    db.0.set_chat_phase(id, &scope.user_id, &scope.soul_id, backend, &phase)
        .await
        .map_err(core_json_error)?;
    chat_changed(state, db, id);
    Ok(())
}

// Only call under the scoped lock after the preceding local worker has drained.
async fn reconcile_api(state: &AppState, db: &Db, id: &str) -> Result<(), JsonError> {
    let conv = read_chat(db, id).await?;
    let saved = conv.conversation.atomic_chat_state.as_ref();
    if saved.is_some_and(|saved| saved.phase == Phase::Closed) {
        return Ok(());
    }
    let active = saved.and_then(|saved| saved.active.as_ref());
    let running = active.filter(|turn| turn.status == TurnStatus::Running);
    let user = if let Some(turn) = active {
        conv.messages
            .iter()
            .find(|row| row.message.id == turn.user_message_id)
    } else {
        conv.messages
            .iter()
            .rev()
            .find(|row| matches!(row.message.role.as_str(), "user" | "assistant"))
            .filter(|row| row.message.role == "user")
    };
    if let Some(user) = user {
        let mut records = atomic_core::agent::ChatToolRecords::from_saved_message(user);
        let mut changed = false;
        for call in &mut records.tool_calls {
            if call.status == "running" {
                let previous = call.tool_output.take();
                call.status = "failed".into();
                call.tool_output =
                    Some(serde_json::json!({"outcome_unknown":true,"previous_output":previous}));
                call.completed_at = Some(chrono::Utc::now().to_rfc3339());
                changed = true;
            }
        }
        if changed {
            db.0.save_chat_tool_records(id, &user.message.id, &records)
                .await
                .map_err(core_json_error)?;
            chat_changed(state, db, id);
        }
        if running.is_some() {
            db.0.fail_chat_turn(id, &user.message.id, "Previous API worker ended before recording completion. Retry known work or close out from saved history.", unknown_tools(user), None)
                .await.map_err(core_json_error)?;
            chat_changed(state, db, id);
        }
    }
    if let Some(recap) = saved
        .and_then(|saved| saved.recap.as_ref())
        .filter(|recap| matches!(recap.outcome, RecapOutcome::Running))
    {
        db.0.fail_chat_turn(
            id,
            &recap.request_message_id,
            "Previous recap worker ended before recording completion. Retry close-out.",
            false, None,
        )
        .await
        .map_err(core_json_error)?;
        chat_changed(state, db, id);
    }
    Ok(())
}

async fn fail_execution(
    state: &AppState,
    db: &Db,
    id: &str,
    input_id: &str,
    error: &atomic_core::AtomicCoreError,
) -> Result<(), JsonError> {
    tracing::error!(conversation_id = id, error = %error, "Atomic chat execution failed");
    let conv = read_chat(db, id).await?;
    let recap = conv
        .conversation
        .atomic_chat_state
        .as_ref()
        .and_then(|state| state.recap.as_ref())
        .is_some_and(|recap| recap.request_message_id == input_id);
    let uncertain = !recap
        && conv
            .messages
            .iter()
            .find(|row| row.message.id == input_id)
            .is_some_and(unknown_tools);
    db.0.fail_chat_turn(id, input_id, &error.to_string(), uncertain, None)
        .await
        .map_err(core_json_error)?;
    chat_changed(state, db, id);
    Ok(())
}

fn hide_system_messages(
    mut conv: atomic_core::ConversationWithMessages,
) -> atomic_core::ConversationWithMessages {
    conv.messages.retain(|m| m.message.role != "system");
    conv
}

fn conversation_matches(conv: &atomic_core::ConversationWithMessages, scope: &MemuScope) -> bool {
    conv.conversation.user_id.as_deref() == Some(scope.user_id.as_str())
        && conv.conversation.soul_id.as_deref() == Some(scope.soul_id.as_str())
}

async fn request_scope(
    state: &web::Data<AppState>,
    request: &HttpRequest,
) -> Result<Option<MemuScope>, HttpResponse> {
    if state.memu_session.is_none() {
        return Ok(None);
    }
    memu_proxy::session(state, request).await.map(Some)
}

async fn require_conversation_owner(
    db: &Db,
    id: &str,
    scope: Option<&MemuScope>,
) -> Result<(), JsonError> {
    let Some(scope) = scope else {
        return Ok(());
    };
    match db.0.get_conversation(id).await {
        Ok(Some(conv)) if conversation_matches(&conv, scope) => Ok(()),
        Ok(_) => Err((
            StatusCode::NOT_FOUND,
            serde_json::json!({"error": "Conversation not found"}),
        )),
        Err(error) => Err(core_json_error(error)),
    }
}

pub(crate) async fn lock_conversation(
    db: &Db,
    id: &str,
    scope: Option<&MemuScope>,
) -> Result<ConversationWork, JsonError> {
    require_conversation_owner(db, id, scope).await?;
    let (key, lock) = conversation_lock(db, id).await;
    let guard = lock.lock_owned().await;
    require_conversation_owner(db, id, scope).await?;
    Ok(ConversationWork { key, _guard: guard })
}

async fn conversation_lock(db: &Db, id: &str) -> (ConversationKey, Arc<tokio::sync::Mutex<()>>) {
    let key = (db.1.clone(), id.to_string());
    let lock = {
        let mut locks = CONVERSATION_WORK.lock().await;
        // ponytail: prune idle locks on demand; use an eviction cache if this map becomes hot.
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    (key, lock)
}

fn rows_for_saved_history(
    conv: &atomic_core::ConversationWithMessages,
) -> Vec<AtomicTranscriptRow> {
    atomic_core::chat::dialogue_messages(conv, None)
        .into_iter()
        .filter(|row| row.message.role != "system" && !row.message.content.trim().is_empty())
        .map(|m| AtomicTranscriptRow {
            role: m.message.role.clone(),
            content: m.message.content.clone(),
            created_at: m.message.created_at.clone(),
        })
        .collect()
}

fn has_user_assistant_interchange(rows: &[AtomicTranscriptRow]) -> bool {
    rows.iter().any(|m| m.role == "user") && rows.iter().any(|m| m.role == "assistant")
}

fn needs_activity_recap(
    conv: &atomic_core::ConversationWithMessages,
    rows: &[AtomicTranscriptRow],
) -> bool {
    has_user_assistant_interchange(rows)
        || conv
            .conversation
            .atomic_chat_state
            .as_ref()
            .is_some_and(|saved| {
                (saved.backend == Backend::ClaudeCode
                    && saved
                        .active
                        .as_ref()
                        .is_some_and(|turn| turn.native_input_recorded != Some(false)))
                    || conv.messages.iter().any(|row| !row.tool_calls.is_empty())
            })
}

async fn api_context_tokens(
    db: &Db,
    scope: &MemuScope,
    conv: &atomic_core::ConversationWithMessages,
    request: &ChatRequest,
    settings: &HashMap<String, String>,
) -> Result<usize, JsonError> {
    let id = &conv.conversation.id;
    let ordinary = db.0.estimate_chat_request_tokens(id, request, settings.clone(), tools_for(scope))
        .await.map_err(core_json_error)?;
    if conv.conversation.atomic_chat_state.is_none() || matches!(request, ChatRequest::Recap { .. }) {
        return Ok(ordinary);
    }
    let recap = ChatRequest::Recap { instruction: atomic_recap_instruction(&scope.user_id) };
    let mut closeout = db.0.estimate_chat_request_tokens(id, &recap, settings.clone(), tools_for(scope))
        .await.map_err(core_json_error)?;
    if let ChatRequest::New { content, .. } = request {
        closeout = closeout.saturating_add(atomic_core::chunking::count_tokens(content) + 8);
    }
    Ok(ordinary.max(closeout))
}

fn existing_recap(conv: &atomic_core::ConversationWithMessages) -> Option<String> {
    if let Some(saved) = conv.conversation.atomic_chat_state.as_ref() {
        return match saved.recap.as_ref().map(|recap| &recap.outcome) {
            Some(RecapOutcome::Complete { reply_message_id }) => conv
                .messages
                .iter()
                .find(|row| row.message.id == *reply_message_id)
                .map(|row| row.message.content.clone()),
            _ => None,
        };
    }
    let messages = &conv.messages;
    for (idx, message) in messages.iter().enumerate().rev() {
        if message.message.role == "user"
            && is_atomic_recap_prompt(&message.message.content)
        {
            let assistant_idx = messages
                .iter()
                .enumerate()
                .skip(idx + 1)
                .find(|(_, m)| {
                    m.message.role == "assistant" && !m.message.content.trim().is_empty()
                })
                .map(|(assistant_idx, m)| (assistant_idx, m.message.content.trim().to_string()));
            if let Some((assistant_idx, recap)) = assistant_idx {
                let later_chat = messages
                    .iter()
                    .skip(assistant_idx + 1)
                    .any(|m| matches!(m.message.role.as_str(), "user" | "assistant"));
                return (!later_chat).then_some(recap);
            }
            return None;
        }
    }
    None
}

async fn post_atomic_session_end(
    config: &MemuScope,
    atomic_conversation_id: &str,
    recap: Option<String>,
    transcript: Vec<AtomicTranscriptRow>,
) -> Result<serde_json::Value, JsonError> {
    let request = memu_proxy::client_data()?
        .post(format!(
            "{}/integration/atomic/session_end",
            config.base_url
        ))
        .json(&AtomicSessionEndRequest {
            user_id: config.user_id.clone(),
            soul_id: config.soul_id.clone(),
            conversation_id: format!("chat:atomic-{atomic_conversation_id}"),
            activity_recap: recap,
            transcript,
        });
    memu_proxy::memu_json_data(request, "memU session_end")
        .await
        .map_err(|(status, body)| {
            // End's existing contract renders remote errors as text, unlike other proxies.
            (
                status,
                serde_json::json!({"error": memu_error_text(&body.to_string())}),
            )
        })
}

#[utoipa::path(post, path = "/api/conversations", request_body = CreateConversationBody, responses((status = 201, description = "Created conversation", body = atomic_core::ConversationWithTags)), tag = "chat")]
pub async fn create_conversation(
    request: HttpRequest,
    db: Db,
    state: web::Data<AppState>,
    body: web::Json<CreateConversationBody>,
) -> HttpResponse {
    let memu_session = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let backend = if let Some(scope) = &memu_session {
        match fetch_atomic_chat_profile(scope).await {
            Ok(profile) => {
                if let Err(error) = profile.check_admission() {
                    return json_error_response(error);
                }
                profile.backend()
            },
            Err(error) => return json_error_response(error),
        }
    } else {
        Backend::Api
    };
    let req = body.into_inner();
    let mut conv = match db
        .0
        .create_conversation(
            &req.tag_ids,
            req.title.as_deref(),
            memu_session
                .as_ref()
                .map(|scope| (scope.user_id.as_str(), scope.soul_id.as_str())),
        )
        .await
    {
        Ok(conv) => conv,
        Err(e) => return crate::error::error_response(e),
    };

    let Some(memu_session) = memu_session else {
        return HttpResponse::Created().json(conv);
    };

    let conv_id = conv.conversation.id.clone();
    let snapshot = match fetch_atomic_snapshot(&memu_session, &conv_id).await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            if let Err(cleanup_err) = db.0.delete_conversation(&conv_id).await {
                tracing::warn!(conversation_id = %conv_id, error = %cleanup_err, "failed to clean up conversation after memU snapshot failure");
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": e,
                    "cleanup_error": cleanup_err.to_string()
                }));
            }
            return HttpResponse::BadGateway().json(serde_json::json!({ "error": e }));
        }
    };
    let initialized = async {
        db.0.save_message(&conv_id, "system", &snapshot).await?;
        db.0.set_chat_phase(
            &conv_id,
            &memu_session.user_id,
            &memu_session.soul_id,
            &backend,
            &Phase::Open,
        )
        .await?;
        db.0.get_conversation(&conv_id).await?.ok_or_else(|| {
            atomic_core::AtomicCoreError::Validation("Conversation not found".into())
        })
    }
    .await;
    match initialized {
        Ok(saved) => {
            conv.conversation = saved.conversation;
            chat_changed(&state, &db, &conv_id);
            HttpResponse::Created().json(conv)
        }
        Err(e) => {
            if let Err(cleanup_err) = db.0.delete_conversation(&conv_id).await {
                tracing::warn!(conversation_id = %conv_id, error = %cleanup_err, "failed to clean up conversation after snapshot save failure");
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": e.to_string(),
                    "cleanup_error": cleanup_err.to_string()
                }));
            }
            crate::error::error_response(e)
        }
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetConversationsQuery {
    /// Filter by tag ID
    pub filter_tag_id: Option<String>,
    /// Max results (default: 50)
    pub limit: Option<i32>,
    /// Offset for pagination
    pub offset: Option<i32>,
}

#[utoipa::path(get, path = "/api/conversations", params(GetConversationsQuery), responses((status = 200, description = "List of conversations", body = Vec<atomic_core::ConversationWithTags>)), tag = "chat")]
pub async fn get_conversations(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    query: web::Query<GetConversationsQuery>,
) -> HttpResponse {
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    ok_or_error(
        db.0.get_conversations(
            query.filter_tag_id.as_deref(),
            limit,
            offset,
            scope
                .as_ref()
                .map(|scope| (scope.user_id.as_str(), scope.soul_id.as_str())),
        )
        .await,
    )
}

#[utoipa::path(get, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), responses((status = 200, description = "Conversation with messages", body = atomic_core::ConversationWithMessages), (status = 404, description = "Not found", body = ApiErrorResponse)), tag = "chat")]
pub async fn get_conversation(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    bridge: web::Data<ChatToolBridge>,
) -> HttpResponse {
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let id = path.into_inner();
    match db.0.get_conversation(&id).await {
        Ok(Some(mut conv))
            if scope
                .as_ref()
                .map_or(true, |scope| conversation_matches(&conv, scope)) =>
        {
            let mut recovery_error = None;
            if let Some(scope) = scope.as_ref().filter(|_| {
                conv.conversation
                    .atomic_chat_state
                    .as_ref()
                    .is_some_and(|saved| {
                        saved.backend == Backend::ClaudeCode
                    })
            }) {
                let (key, lock) = conversation_lock(&db, &id).await;
                // A read must not queue behind or drain the active sitting worker.
                if let Ok(guard) = lock.try_lock_owned() {
                    let work = ConversationWork { key, _guard: guard };
                    if let Err((_, error)) =
                        native::reconcile(&state, &db, scope, &bridge, &id, &work).await
                    {
                        recovery_error = Some(error);
                    }
                    conv = match read_chat(&db, &id).await {
                        Ok(saved) => saved,
                        Err(error) => return json_error_response(error),
                    };
                    if !conversation_matches(&conv, scope) {
                        return HttpResponse::NotFound()
                            .json(serde_json::json!({"error":"Conversation not found"}));
                    }
                }
            }
            let mut value = serde_json::to_value(hide_system_messages(conv.clone()))
                .expect("serializable conversation");
            if let Some(scope) = scope.filter(|_| {
                !conv
                    .conversation
                    .atomic_chat_state
                    .as_ref()
                    .is_some_and(|saved| saved.phase == Phase::Closed)
            }) {
                let meter: Result<Option<serde_json::Value>, JsonError> = async {
                    let profile = fetch_atomic_chat_profile(&scope).await?;
                    profile.check_binding(&conv)?;
                    let capacity = profile.capacity()?;
                    let tokens = match profile {
                        AtomicChatProfileResponse::Api { settings } => api_context_tokens(
                            &db, &scope, &conv, &standing_request(&conv, &scope.user_id), &settings).await?,
                        AtomicChatProfileResponse::ClaudeCode { .. } => {
                            let Some(tokens) = conv.conversation.atomic_chat_state.as_ref()
                                .and_then(|saved| saved.native_context_tokens) else { return Ok(None); };
                            tokens
                        },
                    };
                    Ok(Some(serde_json::json!({"estimated_tokens":tokens,"limit_tokens":capacity - 20_000,"usable_tokens":capacity})))
                }.await;
                match meter {
                    Ok(Some(meter)) => value["context_meter"] = meter,
                    Ok(None) => {},
                    Err((_, error)) => {
                        value["context_error"] =
                            serde_json::json!(memu_error_text(&error.to_string()))
                    }
                }
            }
            if let Some(error) = recovery_error {
                let recovery = memu_error_text(&error.to_string());
                value["context_error"] = serde_json::json!(match value["context_error"].as_str() {
                    Some(meter) => format!("{meter}\n{recovery}"),
                    None => recovery,
                });
            }
            HttpResponse::Ok().json(value)
        }
        Ok(Some(_)) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": "Conversation not found"}))
        }
        Ok(None) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": "Conversation not found"}))
        }
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct UpdateConversationBody {
    /// Updated title
    pub title: Option<String>,
    /// Archive/unarchive
    pub is_archived: Option<bool>,
}

#[utoipa::path(put, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), request_body = UpdateConversationBody, responses((status = 200, description = "Updated conversation")), tag = "chat")]
pub async fn update_conversation(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    body: web::Json<UpdateConversationBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let _work = match lock_conversation(&db, &id, scope.as_ref()).await {
        Ok(guard) => guard,
        Err(error) => return json_error_response(error),
    };
    let req = body.into_inner();
    ok_or_error(
        db.0.update_conversation(&id, req.title.as_deref(), req.is_archived)
            .await,
    )
}

#[utoipa::path(delete, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), responses((status = 200, description = "Conversation deleted")), tag = "chat")]
pub async fn delete_conversation(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let _work = match lock_conversation(&db, &id, scope.as_ref()).await {
        Ok(guard) => guard,
        Err(error) => return json_error_response(error),
    };
    ok_or_error(db.0.delete_conversation(&id).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SetScopeBody {
    /// Tag IDs for the conversation scope
    #[serde(default)]
    pub tag_ids: Vec<String>,
}

#[utoipa::path(put, path = "/api/conversations/{id}/scope", params(("id" = String, Path, description = "Conversation ID")), request_body = SetScopeBody, responses((status = 200, description = "Scope updated")), tag = "chat")]
pub async fn set_conversation_scope(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    body: web::Json<SetScopeBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let _work = match lock_conversation(&db, &id, scope.as_ref()).await {
        Ok(guard) => guard,
        Err(error) => return json_error_response(error),
    };
    let tag_ids = body.into_inner().tag_ids;
    ok_or_error(db.0.set_conversation_scope(&id, &tag_ids).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct AddTagBody {
    /// Tag ID to add to scope
    pub tag_id: String,
}

#[utoipa::path(post, path = "/api/conversations/{id}/scope/tags", params(("id" = String, Path, description = "Conversation ID")), request_body = AddTagBody, responses((status = 200, description = "Tag added to scope")), tag = "chat")]
pub async fn add_tag_to_scope(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    body: web::Json<AddTagBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let _work = match lock_conversation(&db, &id, scope.as_ref()).await {
        Ok(guard) => guard,
        Err(error) => return json_error_response(error),
    };
    let tag_id = body.into_inner().tag_id;
    ok_or_error(db.0.add_tag_to_scope(&id, &tag_id).await)
}

#[utoipa::path(delete, path = "/api/conversations/{id}/scope/tags/{tag_id}", params(("id" = String, Path, description = "Conversation ID"), ("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Tag removed from scope")), tag = "chat")]
pub async fn remove_tag_from_scope(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (id, tag_id) = path.into_inner();
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let _work = match lock_conversation(&db, &id, scope.as_ref()).await {
        Ok(guard) => guard,
        Err(error) => return json_error_response(error),
    };
    ok_or_error(db.0.remove_tag_from_scope(&id, &tag_id).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SendMessageBody {
    /// Message content
    pub content: String,
    /// Optional canvas context for canvas-aware chat tools
    #[serde(default)]
    pub canvas_context: Option<atomic_core::CanvasContext>,
    /// Optional current UI context for page-aware chat tools
    #[serde(default)]
    pub page_context: Option<atomic_core::PageContext>,
}

#[derive(Deserialize, ToSchema)]
pub struct RetryMessageBody {
    user_message_id: String,
}

fn schedule_close_out(
    state: web::Data<AppState>,
    db: Db,
    scope: MemuScope,
    id: String,
    bridge: web::Data<ChatToolBridge>,
) {
    atomic_core::executor::spawn(async move {
        let result = async {
            let work = lock_conversation(&db, &id, Some(&scope)).await?;
            finish_memu_session(state, db, scope, &id, &bridge, &work).await
        }
        .await;
        let _ = end_work_result(&id, result);
    });
}

async fn chat_work(
    state: web::Data<AppState>,
    db: Db,
    scope: MemuScope,
    id: String,
    request: ChatRequest,
    bridge: web::Data<ChatToolBridge>,
) -> Result<atomic_core::ChatMessageWithContext, JsonError> {
    let work = lock_conversation(&db, &id, Some(&scope)).await?;
    if let Some(reply) = saved_retry_reply(&read_chat(&db, &id).await?, &request) {
        return Ok(reply);
    }
    let profile = fetch_atomic_chat_profile(&scope).await?;
    chat_work_with_profile(state, db, scope, id, request, bridge, profile, work).await
}

// The captured profile and sitting guard survive browser delivery loss together.
async fn chat_work_with_profile(
    state: web::Data<AppState>,
    db: Db,
    scope: MemuScope,
    id: String,
    request: ChatRequest,
    bridge: web::Data<ChatToolBridge>,
    profile: AtomicChatProfileResponse,
    work: ConversationWork,
) -> Result<atomic_core::ChatMessageWithContext, JsonError> {
    let conv = read_chat(&db, &id).await?;
    let capacity = profile.capacity()?;
    let backend = profile.backend();
    profile.check_binding(&conv)?;
    let bridge_url = match &profile {
        AtomicChatProfileResponse::Api { .. } => {
            reconcile_api(&state, &db, &id).await?;
            None
        }
        AtomicChatProfileResponse::ClaudeCode { .. } => {
            let url = bridge
                .local_url()
                .map_err(|error| (StatusCode::CONFLICT, serde_json::json!({"error":error})))?;
            native::reconcile(&state, &db, &scope, &bridge, &id, &work).await?;
            Some(url)
        }
    };
    let conv = read_chat(&db, &id).await?;
    if let Some(reply) = saved_retry_reply(&conv, &request) {
        return Ok(reply);
    }
    profile.check_admission()?;
    // Validate using admission's pure rules before sizing or choosing close-out.
    db.0.validate_chat_request(
        &id,
        &scope.user_id,
        &scope.soul_id,
        &backend,
        &request,
        capacity,
    )
    .await
    .map_err(core_json_error)?;
    let prepared = match &profile {
        AtomicChatProfileResponse::ClaudeCode { cli } => {
            Some(native::prepare_input(&db, &scope, &id, &request, cli, &work).await?)
        }
        _ => None,
    };
    let projected = match &profile {
        AtomicChatProfileResponse::Api { settings } =>
            api_context_tokens(&db, &scope, &conv, &request, settings).await?,
        AtomicChatProfileResponse::ClaudeCode { .. } => prepared.as_ref().unwrap().context_tokens
            .ok_or_else(|| (StatusCode::CONFLICT, serde_json::json!({"error":
                "Native context size is unknown. Keep this history and close out in the same sitting, or reload after native work settles; the draft is retained."})))?,
    };
    if projected >= capacity - 20_000 {
        let previous = conv
            .conversation
            .atomic_chat_state
            .as_ref()
            .and_then(|state| state.context_window_tokens);
        if previous.is_some_and(|old| capacity < old && projected < old.saturating_sub(20_000)) {
            return Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":format!("The selected model/provider has too small a usable context window for this sitting. Restore a larger compatible model and {}; the draft and sitting are retained.", if backend == Backend::Api { "llm.context_window_tokens" } else { "claude_code_context_window_tokens" })}),
            ));
        }
        let fresh_tokens = if matches!(request, ChatRequest::New { .. }) {
            match &profile {
                AtomicChatProfileResponse::Api { settings } => {
                    db.0.estimate_fresh_chat_request_tokens(
                        &id,
                        &request,
                        settings.clone(),
                        tools_for(&scope),
                    )
                    .await
                    .map_err(core_json_error)?
                }
                AtomicChatProfileResponse::ClaudeCode { cli } => {
                    db.0.project_claude_chat_input(
                        &id,
                        &request,
                        cli.common_settings(),
                        tools_for(&scope),
                        true,
                        None,
                    )
                    .await
                    .map_err(core_json_error)?
                    .1
                }
            }
        } else {
            0
        };
        if fresh_tokens >= capacity - 20_000 {
            return Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":"This message is too long even for a fresh sitting with this model. Shorten it or choose a larger usable context window; the sitting stays open."}),
            ));
        }
        if conv.conversation.atomic_chat_state.is_none() {
            return Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":LEGACY_CONTEXT_ERROR}),
            ));
        }
        change_phase(&state, &db, &scope, &id, &backend, Phase::Closing).await?;
        schedule_close_out(state, db, scope, id, bridge);
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error":if matches!(request, ChatRequest::New { .. }) {
                "Context limit reached. This sitting is closing; keep this draft for Start new sitting."
            } else { "Context limit reached. This sitting is closing from its saved history." }}),
        ));
    }
    let input =
        db.0.admit_chat_turn(
            &id,
            &scope.user_id,
            &scope.soul_id,
            &backend,
            &request,
            capacity,
        )
        .await
        .map_err(core_json_error)?;
    chat_changed(&state, &db, &id);
    let on_event = chat_event_callback(
        state.event_tx.clone(),
        db.1.clone(),
        Some((scope.user_id.clone(), scope.soul_id.clone())),
    );
    let result = match &profile {
        AtomicChatProfileResponse::ClaudeCode { cli } => {
            native::execute_prepared(
                &state,
                &db,
                &scope,
                cli,
                &bridge,
                bridge_url.expect("native tool URL validated before admission"),
                &id,
                &input.id,
                native::Purpose::Reply,
                prepared.expect("prepared native input"),
            )
            .await
        }
        AtomicChatProfileResponse::Api { settings } => {
            let result =
                db.0.execute_saved_chat_turn(
                    &id,
                    &input.id,
                    on_event,
                    settings.clone(),
                    tools_for(&scope),
                )
                .await;
            match result {
                Ok(reply) => {
                    chat_changed(&state, &db, &id);
                    Ok(reply)
                }
                Err(error) => {
                    fail_execution(&state, &db, &id, &input.id, &error).await?;
                    Err(core_json_error(error))
                }
            }
        }
    };
    // The persisted dialogue/tool outcomes remain available on failed turns too.
    let measured: Result<usize, JsonError> = async {
        let saved = read_chat(&db, &id).await?;
        match &profile {
            AtomicChatProfileResponse::Api { settings } => {
                let standing = standing_request(&saved, &scope.user_id);
                api_context_tokens(&db, &scope, &saved, &standing, settings).await
            }
            AtomicChatProfileResponse::ClaudeCode { .. } => saved
                .conversation
                .atomic_chat_state
                .as_ref()
                .and_then(|saved| saved.native_context_tokens)
                .ok_or_else(|| {
                    (
                        StatusCode::CONFLICT,
                        serde_json::json!({"error":"Native context size is unknown"}),
                    )
                }),
        }
    }
    .await;
    let tokens = match measured {
        Ok(tokens) => tokens,
        Err(error) => {
            tracing::warn!(conversation_id = %id, error = ?error, "Could not size saved turn; preserving its actual result");
            return result;
        }
    };
    if tokens >= capacity - 20_000 {
        change_phase(&state, &db, &scope, &id, &backend, Phase::Closing).await?;
        schedule_close_out(state, db, scope, id, bridge);
    } else if backend == Backend::ClaudeCode && result.is_ok() {
        let saved = read_chat(&db, &id).await?;
        if saved.conversation.atomic_chat_state.as_ref().is_some_and(|saved| native::ordinary_due(saved, capacity, tokens)) {
            if let Err(error) = native::maintain(&state, &db, &scope, &bridge, &id,
                atomic_core::AtomicChatMaintenanceKind::Ordinary, &work).await {
                tracing::warn!(conversation_id = %id, error = ?error, "Native cleanup is unfinished; retaining its saved outcome");
            }
        }
    }
    result
}

fn saved_retry_reply(
    conv: &atomic_core::ConversationWithMessages,
    request: &ChatRequest,
) -> Option<atomic_core::ChatMessageWithContext> {
    let ChatRequest::Retry { user_message_id } = request else {
        return None;
    };
    let dialogue = atomic_core::chat::dialogue_messages(conv, None);
    let (index, _) = dialogue
        .iter()
        .enumerate()
        .rev()
        .find(|(_, row)| row.message.role == "user")
        .filter(|(_, row)| {
            row.message.id == *user_message_id
                && !(conv.conversation.atomic_chat_state.is_none()
                    && is_atomic_recap_prompt(&row.message.content))
        })?;
    dialogue
        .iter()
        .skip(index + 1)
        .find(|row| row.message.role == "assistant")
        .map(|reply| (**reply).clone())
}

fn standing_request(conv: &atomic_core::ConversationWithMessages, user_id: &str) -> ChatRequest {
    if let Some(active) = conv
        .conversation
        .atomic_chat_state
        .as_ref()
        .filter(|state| state.phase == Phase::Open)
        .and_then(|state| state.active.as_ref())
    {
        if active.status == TurnStatus::Failed {
            return ChatRequest::Retry {
                user_message_id: active.user_message_id.clone(),
            };
        }
        if active.status == TurnStatus::Uncertain {
            return ChatRequest::Recap {
                instruction: atomic_recap_instruction(user_id),
            };
        }
    }
    if conv
        .conversation
        .atomic_chat_state
        .as_ref()
        .is_some_and(|state| {
            state.phase == Phase::Closing
                && !state
                    .recap
                    .as_ref()
                    .is_some_and(|recap| matches!(recap.outcome, RecapOutcome::Complete { .. }))
        })
    {
        return ChatRequest::Recap {
            instruction: atomic_recap_instruction(user_id),
        };
    }
    ChatRequest::New {
        content: String::new(),
        page_context: None,
        canvas_context: None,
    }
}

#[utoipa::path(post, path = "/api/conversations/{id}/retry", params(("id" = String, Path)), request_body = RetryMessageBody,
    responses((status = 200, description = "Recovered assistant reply", body = atomic_core::ChatMessageWithContext)), tag = "chat")]
pub async fn retry_chat_message(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    body: web::Json<RetryMessageBody>,
    bridge: web::Data<ChatToolBridge>,
) -> HttpResponse {
    let scope = match memu_proxy::session(&state, &request).await {
        Ok(scope) => scope,
        Err(error) => return error,
    };
    let id = path.into_inner();
    let input = ChatRequest::Retry {
        user_message_id: body.user_message_id.clone(),
    };
    let result =
        atomic_core::executor::spawn_with_result(chat_work(state, db, scope, id, input, bridge))
            .await;
    match result {
        Ok(Ok(reply)) => HttpResponse::Ok().json(reply),
        Ok(Err(error)) => json_error_response(error),
        Err(_) => lost_chat_worker(),
    }
}

#[utoipa::path(post, path = "/api/conversations/{id}/messages", params(("id" = String, Path, description = "Conversation ID")), request_body = SendMessageBody, responses((status = 200, description = "Assistant response (streaming events via WebSocket)", body = atomic_core::ChatMessageWithContext)), tag = "chat")]
pub async fn send_chat_message(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    body: web::Json<SendMessageBody>,
    bridge: web::Data<ChatToolBridge>,
) -> HttpResponse {
    let conversation_id = path.into_inner();
    let body = body.into_inner();
    let memu_session = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Some(scope) = memu_session {
        let input = ChatRequest::New {
            content: body.content,
            page_context: body.page_context,
            canvas_context: body.canvas_context,
        };
        let result = atomic_core::executor::spawn_with_result(chat_work(
            state,
            db,
            scope,
            conversation_id,
            input,
            bridge,
        ))
        .await;
        return match result {
            Ok(Ok(reply)) => HttpResponse::Ok().json(reply),
            Ok(Err(error)) => json_error_response(error),
            Err(_) => lost_chat_worker(),
        };
    }
    let _work = match lock_conversation(&db, &conversation_id, None).await {
        Ok(guard) => guard,
        Err(error) => return json_error_response(error),
    };
    let on_event = chat_event_callback(state.event_tx.clone(), db.1.clone(), None);
    let result = if body.canvas_context.is_some() || body.page_context.is_some() {
        db.0.send_chat_message_with_canvas(
            &conversation_id,
            &body.content,
            on_event,
            body.canvas_context,
            body.page_context,
        )
        .await
    } else {
        db.0.send_chat_message(&conversation_id, &body.content, on_event)
            .await
    };

    match result {
        Ok(message) => HttpResponse::Ok().json(message),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/memu/session/end", responses((status = 200, description = "Ended memU Atomic session")), tag = "chat")]
pub async fn end_memu_session(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    body: web::Json<EndMemuSessionBody>,
    bridge: web::Data<ChatToolBridge>,
) -> HttpResponse {
    let memu_session = match memu_proxy::session(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let conversation_id = body.conversation_id.trim();
    if conversation_id.is_empty() {
        return HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "conversation_id is required"}));
    }
    let conversation_id = conversation_id.to_string();
    let result = atomic_core::executor::spawn_with_result(async move {
        // End intent survives browser loss even while an admitted reply holds the lock.
        let work = match lock_conversation(&db, &conversation_id, Some(&memu_session)).await {
            Ok(guard) => guard,
            Err(error) => return end_work_result(&conversation_id, Err(error)),
        };
        end_work_result(
            &conversation_id,
            finish_memu_session(state, db, memu_session, &conversation_id, &bridge, &work).await,
        )
    });
    match result.await {
        Ok(Ok(body)) => HttpResponse::Ok().json(body),
        Ok(Err(error)) => json_error_response(error),
        Err(_) => lost_chat_worker(),
    }
}

async fn finish_memu_session(
    state: web::Data<AppState>,
    db: Db,
    memu_session: MemuScope,
    conversation_id: &str,
    bridge: &ChatToolBridge,
    work: &ConversationWork,
) -> Result<serde_json::Value, JsonError> {
    let mut conv = read_chat(&db, conversation_id).await?;
    if conv
        .conversation
        .atomic_chat_state
        .as_ref()
        .is_some_and(|saved| saved.phase == Phase::Closed)
    {
        if conv.conversation.atomic_chat_state.as_ref().is_some_and(|saved| saved.backend == Backend::ClaudeCode) {
            if let Err(error) = native::reconcile(&state, &db, &memu_session, bridge, conversation_id, work).await {
                tracing::warn!(conversation_id, error = ?error, "Closed sitting still needs native storage cleanup");
            }
        }
        return Ok(serde_json::json!({"ok":true,"status":"already_ended"}));
    }
    let recap = existing_recap(&conv);
    // Completed pre-activation sittings retain their existing publication semantics.
    if conv.conversation.atomic_chat_state.is_none() && recap.is_some() {
        return post_atomic_session_end(
            &memu_session,
            conversation_id,
            recap,
            rows_for_saved_history(&conv),
        )
        .await;
    }
    let profile = fetch_atomic_chat_profile(&memu_session).await?;
    let instruction = atomic_recap_instruction(&memu_session.user_id);
    let request = ChatRequest::Recap { instruction: instruction.clone() };
    let capacity = profile.capacity()?;
    let backend = profile.backend();
    profile.check_binding(&conv)?;
    let bridge_url = match &profile {
        AtomicChatProfileResponse::Api { .. } => {
            reconcile_api(&state, &db, conversation_id).await?;
            None
        }
        AtomicChatProfileResponse::ClaudeCode { .. } => {
            let url = bridge
                .local_url()
                .map_err(|error| (StatusCode::CONFLICT, serde_json::json!({"error":error})))?;
            native::reconcile(&state, &db, &memu_session, bridge, conversation_id, work).await?;
            Some(url)
        }
    };
    // Recovery may have saved the recap or recovered an ordinary assistant row.
    conv = read_chat(&db, conversation_id).await?;
    let mut recap = existing_recap(&conv);
    let rows = rows_for_saved_history(&conv);
    let needs_recap = recap.is_none() && needs_activity_recap(&conv, &rows);
    if needs_recap {
        profile.check_admission()?;
        let projected = match &profile {
            AtomicChatProfileResponse::Api { settings } => Some(
                db.0.estimate_chat_request_tokens(
                    conversation_id,
                    &request,
                    settings.clone(),
                    tools_for(&memu_session),
                )
                .await
                .map_err(core_json_error)?,
            ),
            AtomicChatProfileResponse::ClaudeCode { .. } => None,
        };
        if projected.is_some_and(|tokens| {
            tokens.saturating_add(atomic_core::agent::CHAT_RESPONSE_MAX_TOKENS as usize) > capacity
        }) {
            return Err((
                StatusCode::CONFLICT,
                serde_json::json!({"error":if conv.conversation.atomic_chat_state.is_none() {
                    LEGACY_CONTEXT_ERROR
                } else { "The recap needs a model/provider with a larger usable context window. Increase llm.context_window_tokens with a compatible model; saved history is unchanged." }}),
            ));
        }
    }
    change_phase(
        &state,
        &db,
        &memu_session,
        conversation_id,
        &backend,
        Phase::Closing,
    )
    .await?;
    if needs_recap {
        // Recap delivery uses durable state, never an ordinary reply/stream bubble.
        match &profile {
            AtomicChatProfileResponse::ClaudeCode { cli } => {
                native::execute_recap(
                    &state,
                    &db,
                    &memu_session,
                    cli,
                    bridge,
                    bridge_url.expect("native tool URL validated before admission"),
                    conversation_id,
                    &request,
                    capacity,
                    work,
                )
                .await?;
            }
            AtomicChatProfileResponse::Api { settings } => {
                let control = db.0.admit_chat_turn(conversation_id, &memu_session.user_id,
                    &memu_session.soul_id, &backend, &request, capacity).await.map_err(core_json_error)?;
                chat_changed(&state, &db, conversation_id);
                match db
                    .0
                    .execute_saved_chat_recap(
                        conversation_id,
                        &control.id,
                        &instruction,
                        |_| {},
                        settings.clone(),
                        tools_for(&memu_session),
                    )
                    .await
                {
                    Ok(_) => chat_changed(&state, &db, conversation_id),
                    Err(error) => {
                        fail_execution(&state, &db, conversation_id, &control.id, &error).await?;
                        return Err(core_json_error(error));
                    }
                }
            }
        }
        conv = read_chat(&db, conversation_id).await?;
        recap = existing_recap(&conv);
    }
    let result = post_atomic_session_end(&memu_session, conversation_id, recap, rows).await;
    match result {
        Ok(result) => {
            change_phase(
                &state,
                &db,
                &memu_session,
                conversation_id,
                &backend,
                Phase::Closed,
            )
            .await?;
            if backend == Backend::ClaudeCode {
                if let Err(error) = native::maintain(&state, &db, &memu_session, bridge, conversation_id,
                    atomic_core::AtomicChatMaintenanceKind::Purge, work).await {
                    tracing::warn!(conversation_id, error = ?error, "Session published and Closed; native storage cleanup remains");
                }
            }
            Ok(result)
        }
        Err(error) => {
            chat_changed(&state, &db, conversation_id);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_web::test]
    async fn api_context_includes_recorded_actions_and_pending_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let manager = atomic_core::DatabaseManager::new(dir.path()).unwrap();
        let db = Db(manager.active_core().await.unwrap(), manager.active_id().unwrap());
        let scope = MemuScope { base_url: "http://127.0.0.1:1".into(), user_id: "TestOwner".into(), soul_id: "TestSoul".into() };
        let conv = db.0.create_conversation(&[], None, Some((&scope.user_id, &scope.soul_id))).await.unwrap();
        let id = &conv.conversation.id;
        db.0.set_chat_phase(id, &scope.user_id, &scope.soul_id, &Backend::Api, &Phase::Open).await.unwrap();
        db.0.save_message(id, "user", "Short question").await.unwrap();
        let answer = db.0.save_message(id, "assistant", "Short answer").await.unwrap();
        let saved = read_chat(&db, id).await.unwrap();
        let mut records = atomic_core::agent::ChatToolRecords::from_saved_message(saved.messages.last().unwrap());
        records.tool_calls = (0..300).map(|i| atomic_core::ChatToolCall {
            id: format!("recorded-{i}"), message_id: answer.id.clone(), tool_name: "search_atoms".into(),
            tool_input: serde_json::json!({"query":format!("Fictional query {i}: {}", "evidence ".repeat(18))}),
            tool_output: Some(serde_json::json!("completed")), status: "complete".into(),
            created_at: String::new(), completed_at: None,
        }).collect();
        db.0.save_chat_tool_records(id, &answer.id, &records).await.unwrap();
        let saved = read_chat(&db, id).await.unwrap();
        let settings: HashMap<String, String> = [("provider", "openai_compat"), ("openai_compat_base_url", "http://127.0.0.1:1"),
            ("openai_compat_api_key", "fictional"), ("openai_compat_llm_model", "fictional")]
            .into_iter().map(|(k,v)| (k.to_string(),v.to_string())).collect();
        let content = "Pending draft ".repeat(50);
        let request = ChatRequest::New { content: content.clone(), page_context: None, canvas_context: None };
        let ordinary = db.0.estimate_chat_request_tokens(id, &request, settings.clone(), tools_for(&scope)).await.unwrap();
        let recap = ChatRequest::Recap { instruction: atomic_recap_instruction(&scope.user_id) };
        let closeout = db.0.estimate_chat_request_tokens(id, &recap, settings.clone(), tools_for(&scope)).await.unwrap();
        assert!(closeout > ordinary, "short dialogue must not hide accumulated actions");
        assert_eq!(api_context_tokens(&db, &scope, &saved, &request, &settings).await.unwrap(),
            closeout + atomic_core::chunking::count_tokens(&content) + 8);
    }

    #[test]
    fn native_profile_parses_cli_metadata_without_api_settings() {
        let value = serde_json::json!({"backend":"claude_code", "cli":{
            "model":"fictional-cli", "context_window_tokens":128000, "timeout_seconds":90,
            "effort":"high", "fingerprint":"captured-profile", "memu_response_sentences":5,
            "memu_log_prompts":true, "admission_error":""
        }});
        let profile: AtomicChatProfileResponse = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(profile.backend(), Backend::ClaudeCode);
        assert_eq!(profile.capacity().unwrap(), 128000);
        assert!(profile.check_admission().is_ok());
        assert!(matches!(
            profile,
            AtomicChatProfileResponse::ClaudeCode { .. }
        ));
        let mut unavailable = value.clone();
        unavailable["cli"]["admission_error"] = serde_json::json!("Repair Atomic's helper");
        let profile: AtomicChatProfileResponse = serde_json::from_value(unavailable).unwrap();
        assert_eq!(profile.capacity().unwrap(), 128000);
        assert!(profile.check_admission().unwrap_err().1["error"].as_str().unwrap().contains("Repair"));
        let mut missing_readiness = value.clone();
        missing_readiness["cli"].as_object_mut().unwrap().remove("admission_error");
        assert!(serde_json::from_value::<AtomicChatProfileResponse>(missing_readiness).is_err());
        let mut too_small = value;
        too_small["cli"]["context_window_tokens"] = serde_json::json!(20000);
        let profile: AtomicChatProfileResponse = serde_json::from_value(too_small).unwrap();
        assert!(profile.capacity().unwrap_err().1["error"]
            .as_str()
            .unwrap()
            .contains("claude_code_context_window_tokens"));
        assert!(
            serde_json::from_value::<AtomicChatProfileResponse>(serde_json::json!({
                "backend":"claude_code", "settings":{"chat_model":"not-a-CLI-profile"}
            }))
            .is_err()
        );
    }

    #[test]
    fn end_errors_are_plain_data_and_logged_before_delivery() {
        let body = serde_json::json!({"error": {"code": "soul_paused"}});
        let logs = crate::log_buffer::LogBuffer::new(4);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(logs.make_writer())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            end_work_result(
                "test-conversation",
                Err((StatusCode::CONFLICT, body.clone())),
            )
        });
        assert_eq!(result.unwrap_err(), (StatusCode::CONFLICT, body));
        assert!(logs.dump().contains("Atomic End Session failed"));
        assert!(logs.dump().contains("soul_paused"));
    }
}
