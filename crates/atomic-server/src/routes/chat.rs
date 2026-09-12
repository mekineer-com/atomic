//! Chat / Conversation routes

use crate::db_extractor::Db;
use crate::error::{ApiErrorResponse, ok_or_error};
use crate::event_bridge::chat_event_callback;
use crate::routes::memu_proxy::{self, MemuScope, memu_error_text};
use crate::state::AppState;
use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::Duration,
};
use utoipa::{IntoParams, ToSchema};

type ConversationKey = (String, String);
static CONVERSATION_WORK: LazyLock<
    tokio::sync::Mutex<HashMap<ConversationKey, Arc<tokio::sync::Mutex<()>>>>,
> = LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

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

#[derive(Deserialize)]
struct AtomicChatProfileResponse {
    settings: HashMap<String, String>,
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

fn atomic_recap_prompt(user_id: &str, rows: &[AtomicTranscriptRow]) -> String {
    let mut lines = vec![
        atomic_recap_instruction(user_id),
        String::new(),
        "Full Atomic transcript:".to_string(),
    ];
    for row in rows {
        lines.push(format!("{}: {}", row.role, row.content));
    }
    lines.join("\n")
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

async fn fetch_atomic_chat_profile(config: &MemuScope) -> Result<HashMap<String, String>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("memU chat_profile client failed: {e}"))?;
    let response = client
        .get(format!(
            "{}/integration/atomic/chat_profile",
            config.base_url
        ))
        .send()
        .await
        .map_err(|e| format!("memU chat_profile request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("memU chat_profile failed ({status})"));
    }
    let body = response
        .json::<AtomicChatProfileResponse>()
        .await
        .map_err(|e| format!("memU chat_profile returned invalid JSON: {e}"))?;
    if body.settings.is_empty() {
        return Err("memU chat_profile returned empty settings".to_string());
    }
    Ok(body.settings)
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
) -> Result<(), HttpResponse> {
    let Some(scope) = scope else {
        return Ok(());
    };
    match db.0.get_conversation(id).await {
        Ok(Some(conv)) if conversation_matches(&conv, scope) => Ok(()),
        Ok(_) => {
            Err(HttpResponse::NotFound()
                .json(serde_json::json!({"error": "Conversation not found"})))
        }
        Err(error) => Err(crate::error::error_response(error)),
    }
}

async fn lock_conversation(
    db: &Db,
    id: &str,
    scope: Option<&MemuScope>,
) -> Result<tokio::sync::OwnedMutexGuard<()>, HttpResponse> {
    require_conversation_owner(db, id, scope).await?;
    let key = (db.1.clone(), id.to_string());
    let lock = {
        let mut locks = CONVERSATION_WORK.lock().await;
        // ponytail: prune idle locks on demand; use an eviction cache if this map becomes hot.
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    let guard = lock.lock_owned().await;
    require_conversation_owner(db, id, scope).await?;
    Ok(guard)
}

fn transcript_rows(conv: &atomic_core::ConversationWithMessages) -> Vec<AtomicTranscriptRow> {
    let mut rows = Vec::new();
    for m in conv.messages.iter().filter(|m| m.message.role != "system") {
        for call in &m.tool_calls {
            let mut content = format!("{} {}", call.tool_name, call.status);
            if !call.tool_input.is_null() {
                content.push_str(&format!("\ninput: {}", call.tool_input));
            }
            if let Some(output) = &call.tool_output {
                content.push_str(&format!("\noutput: {output}"));
            }
            rows.push(AtomicTranscriptRow {
                role: "tool".to_string(),
                content,
                created_at: call
                    .completed_at
                    .clone()
                    .unwrap_or_else(|| call.created_at.clone()),
            });
        }
        if !m.message.content.trim().is_empty() {
            rows.push(AtomicTranscriptRow {
                role: m.message.role.clone(),
                content: m.message.content.clone(),
                created_at: m.message.created_at.clone(),
            });
        }
    }
    rows
}

fn has_user_assistant_interchange(rows: &[AtomicTranscriptRow]) -> bool {
    rows.iter().any(|m| m.role == "user") && rows.iter().any(|m| m.role == "assistant")
}

fn rows_for_saved_history(
    rows: &[AtomicTranscriptRow],
    recap_instruction: &str,
) -> Vec<AtomicTranscriptRow> {
    let mut cleaned = Vec::new();
    let mut skip_next_assistant = false;
    for row in rows {
        if row.role == "tool" {
            continue;
        }
        if skip_next_assistant && row.role == "assistant" {
            skip_next_assistant = false;
            continue;
        }
        skip_next_assistant = false;
        if row.role == "user"
            && is_atomic_recap_prompt(
                row.content.trim().lines().next().unwrap_or(""),
                recap_instruction,
            )
        {
            skip_next_assistant = true;
            continue;
        }
        cleaned.push(row.clone());
    }
    cleaned
}

fn is_atomic_recap_prompt(first_line: &str, recap_instruction: &str) -> bool {
    first_line == recap_instruction
        || first_line.starts_with("This Atomic session is ending. Write a recap of your activity")
}

fn existing_recap(
    conv: &atomic_core::ConversationWithMessages,
    recap_instruction: &str,
) -> Option<String> {
    let messages = &conv.messages;
    for (idx, message) in messages.iter().enumerate().rev() {
        let first_line = message.message.content.trim().lines().next().unwrap_or("");
        if message.message.role == "user" && is_atomic_recap_prompt(first_line, recap_instruction) {
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
) -> Result<serde_json::Value, HttpResponse> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| {
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": format!("memU client failed: {e}")}))
        })?;
    let response = client
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
        })
        .send()
        .await
        .map_err(|e| {
            HttpResponse::BadGateway()
                .json(serde_json::json!({"error": format!("memU session_end request failed: {e}")}))
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(HttpResponse::build(
            actix_web::http::StatusCode::from_u16(status.as_u16())
                .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY),
        )
        .json(serde_json::json!({"error": memu_error_text(&body)})));
    }
    response.json::<serde_json::Value>().await.map_err(|e| {
        HttpResponse::BadGateway().json(
            serde_json::json!({"error": format!("memU session_end returned invalid JSON: {e}")}),
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
    let req = body.into_inner();
    let conv = match db
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
    match db.0.save_message(&conv_id, "system", &snapshot).await {
        Ok(_) => HttpResponse::Created().json(conv),
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
) -> HttpResponse {
    let scope = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let id = path.into_inner();
    match db.0.get_conversation(&id).await {
        Ok(Some(conv))
            if scope
                .as_ref()
                .map_or(true, |scope| conversation_matches(&conv, scope)) =>
        {
            HttpResponse::Ok().json(hide_system_messages(conv))
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
        Err(response) => return response,
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
        Err(response) => return response,
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
        Err(response) => return response,
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
        Err(response) => return response,
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
        Err(response) => return response,
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

#[utoipa::path(post, path = "/api/conversations/{id}/messages", params(("id" = String, Path, description = "Conversation ID")), request_body = SendMessageBody, responses((status = 200, description = "Assistant response (streaming events via WebSocket)", body = atomic_core::ChatMessageWithContext)), tag = "chat")]
pub async fn send_chat_message(
    request: HttpRequest,
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    body: web::Json<SendMessageBody>,
) -> HttpResponse {
    let conversation_id = path.into_inner();
    let body = body.into_inner();
    let memu_session = match request_scope(&state, &request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let _work = match lock_conversation(&db, &conversation_id, memu_session.as_ref()).await {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    let event_owner = memu_session
        .as_ref()
        .map(|scope| (scope.user_id.clone(), scope.soul_id.clone()));
    let on_event = chat_event_callback(state.event_tx.clone(), db.1.clone(), event_owner);
    let result = if let Some(memu_session) = memu_session {
        let settings = match fetch_atomic_chat_profile(&memu_session).await {
            Ok(settings) => settings,
            Err(e) => return HttpResponse::BadGateway().json(serde_json::json!({ "error": e })),
        };
        let memu_tools = atomic_core::MemuToolConfig {
            base_url: memu_session.base_url,
            user_id: memu_session.user_id,
            soul_id: memu_session.soul_id,
        };
        db.0.send_chat_message_with_external_settings(
            &conversation_id,
            &body.content,
            on_event,
            settings,
            Some(memu_tools),
            body.canvas_context,
            body.page_context,
        )
        .await
    } else if body.canvas_context.is_some() || body.page_context.is_some() {
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
    let _work = match lock_conversation(&db, conversation_id, Some(&memu_session)).await {
        Ok(guard) => guard,
        Err(response) => return response,
    };

    let mut conv = match db.0.get_conversation(conversation_id).await {
        Ok(Some(conv)) => conv,
        Ok(None) => {
            return HttpResponse::NotFound()
                .json(serde_json::json!({"error": "Conversation not found"}));
        }
        Err(e) => return crate::error::error_response(e),
    };
    let mut rows = transcript_rows(&conv);
    let recap_instruction = atomic_recap_instruction(&memu_session.user_id);
    let mut recap = existing_recap(&conv, &recap_instruction);

    if recap.is_none() && has_user_assistant_interchange(&rows) {
        let transcript_before_recap = std::mem::take(&mut rows);
        let settings = match fetch_atomic_chat_profile(&memu_session).await {
            Ok(settings) => settings,
            Err(e) => return HttpResponse::BadGateway().json(serde_json::json!({ "error": e })),
        };
        let on_event = chat_event_callback(
            state.event_tx.clone(),
            db.1.clone(),
            Some((memu_session.user_id.clone(), memu_session.soul_id.clone())),
        );
        let memu_tools = atomic_core::MemuToolConfig {
            base_url: memu_session.base_url.clone(),
            user_id: memu_session.user_id.clone(),
            soul_id: memu_session.soul_id.clone(),
        };
        let recap_prompt = atomic_recap_prompt(&memu_session.user_id, &transcript_before_recap);
        if let Err(e) =
            db.0.send_chat_message_with_external_settings(
                conversation_id,
                &recap_prompt,
                on_event,
                settings,
                Some(memu_tools),
                None,
                None,
            )
            .await
        {
            return crate::error::error_response(e);
        }
        conv = match db.0.get_conversation(conversation_id).await {
            Ok(Some(conv)) => conv,
            Ok(None) => {
                return HttpResponse::NotFound()
                    .json(serde_json::json!({"error": "Conversation not found"}));
            }
            Err(e) => return crate::error::error_response(e),
        };
        recap = existing_recap(&conv, &recap_instruction);
        rows = transcript_before_recap;
    }
    rows = rows_for_saved_history(&rows, &recap_instruction);

    match post_atomic_session_end(&memu_session, conversation_id, recap, rows).await {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}
