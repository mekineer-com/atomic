//! Chat / Conversation routes

use crate::db_extractor::Db;
use crate::error::{ok_or_error, ApiErrorResponse};
use crate::event_bridge::chat_event_callback;
use crate::routes::memu_proxy::memu_error_text;
use crate::state::{AppState, MemuSessionConfig};
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};
use utoipa::{IntoParams, ToSchema};

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
    format!("This Atomic session is ending. Write a recap of your activity with {user_id}: what you looked at, what you changed (edits you made and why), ideas rejected or deferred, and follow-ups you want to remember. Write it as yourself, for yourself.")
}

fn atomic_recap_prompt(user_id: &str, rows: &[AtomicTranscriptRow]) -> String {
    let mut lines = vec![atomic_recap_instruction(user_id), String::new(), "Full Atomic transcript:".to_string()];
    for row in rows {
        lines.push(format!("{}: {}", row.role, row.content));
    }
    lines.join("\n")
}

async fn fetch_atomic_snapshot(
    config: &MemuSessionConfig,
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

async fn fetch_atomic_chat_profile(
    config: &MemuSessionConfig,
) -> Result<HashMap<String, String>, String> {
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
                created_at: call.completed_at.clone().unwrap_or_else(|| call.created_at.clone()),
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

fn rows_for_saved_history(rows: &[AtomicTranscriptRow], recap_instruction: &str) -> Vec<AtomicTranscriptRow> {
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
            && is_atomic_recap_prompt(row.content.trim().lines().next().unwrap_or(""), recap_instruction)
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

fn existing_recap(conv: &atomic_core::ConversationWithMessages, recap_instruction: &str) -> Option<String> {
    let messages = &conv.messages;
    for (idx, message) in messages.iter().enumerate().rev() {
        let first_line = message.message.content.trim().lines().next().unwrap_or("");
        if message.message.role == "user" && is_atomic_recap_prompt(first_line, recap_instruction) {
            let assistant_idx = messages
                .iter()
                .enumerate()
                .skip(idx + 1)
                .find(|(_, m)| m.message.role == "assistant" && !m.message.content.trim().is_empty())
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
    config: &MemuSessionConfig,
    atomic_conversation_id: &str,
    recap: Option<String>,
    transcript: Vec<AtomicTranscriptRow>,
) -> Result<serde_json::Value, HttpResponse> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| HttpResponse::InternalServerError().json(serde_json::json!({"error": format!("memU client failed: {e}")})))?;
    let response = client
        .post(format!("{}/integration/atomic/session_end", config.base_url))
        .json(&AtomicSessionEndRequest {
            user_id: config.user_id.clone(),
            soul_id: config.soul_id.clone(),
            conversation_id: format!("chat:atomic-{atomic_conversation_id}"),
            activity_recap: recap,
            transcript,
        })
        .send()
        .await
        .map_err(|e| HttpResponse::BadGateway().json(serde_json::json!({"error": format!("memU session_end request failed: {e}")})))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(HttpResponse::build(
            actix_web::http::StatusCode::from_u16(status.as_u16()).unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY),
        )
        .json(serde_json::json!({"error": memu_error_text(&body)})));
    }
    response.json::<serde_json::Value>().await.map_err(|e| {
        HttpResponse::BadGateway()
            .json(serde_json::json!({"error": format!("memU session_end returned invalid JSON: {e}")}))
    })
}

#[utoipa::path(post, path = "/api/conversations", request_body = CreateConversationBody, responses((status = 201, description = "Created conversation", body = atomic_core::ConversationWithTags)), tag = "chat")]
pub async fn create_conversation(
    db: Db,
    state: web::Data<AppState>,
    body: web::Json<CreateConversationBody>,
) -> HttpResponse {
    let Some(memu_session) = state.memu_session.clone() else {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "MEMU_SERVER_URL, MEMU_USER_ID, and MEMU_SOUL_ID are required"
        }));
    };
    let req = body.into_inner();
    let conv = match db
        .0
        .create_conversation(&req.tag_ids, req.title.as_deref())
        .await
    {
        Ok(conv) => conv,
        Err(e) => return crate::error::error_response(e),
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
pub async fn get_conversations(db: Db, query: web::Query<GetConversationsQuery>) -> HttpResponse {
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    ok_or_error(
        db.0.get_conversations(query.filter_tag_id.as_deref(), limit, offset)
            .await,
    )
}

#[utoipa::path(get, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), responses((status = 200, description = "Conversation with messages", body = atomic_core::ConversationWithMessages), (status = 404, description = "Not found", body = ApiErrorResponse)), tag = "chat")]
pub async fn get_conversation(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    match db.0.get_conversation(&id).await {
        Ok(Some(conv)) => HttpResponse::Ok().json(hide_system_messages(conv)),
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
    db: Db,
    path: web::Path<String>,
    body: web::Json<UpdateConversationBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let req = body.into_inner();
    ok_or_error(
        db.0.update_conversation(&id, req.title.as_deref(), req.is_archived)
            .await,
    )
}

#[utoipa::path(delete, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), responses((status = 200, description = "Conversation deleted")), tag = "chat")]
pub async fn delete_conversation(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
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
    db: Db,
    path: web::Path<String>,
    body: web::Json<SetScopeBody>,
) -> HttpResponse {
    let id = path.into_inner();
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
    db: Db,
    path: web::Path<String>,
    body: web::Json<AddTagBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let tag_id = body.into_inner().tag_id;
    ok_or_error(db.0.add_tag_to_scope(&id, &tag_id).await)
}

#[utoipa::path(delete, path = "/api/conversations/{id}/scope/tags/{tag_id}", params(("id" = String, Path, description = "Conversation ID"), ("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Tag removed from scope")), tag = "chat")]
pub async fn remove_tag_from_scope(db: Db, path: web::Path<(String, String)>) -> HttpResponse {
    let (id, tag_id) = path.into_inner();
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
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    body: web::Json<SendMessageBody>,
) -> HttpResponse {
    let conversation_id = path.into_inner();
    let body = body.into_inner();
    let on_event = chat_event_callback(state.event_tx.clone());
    let Some(memu_session) = state.memu_session.clone() else {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "MEMU_SERVER_URL, MEMU_USER_ID, and MEMU_SOUL_ID are required"
        }));
    };
    let settings = match fetch_atomic_chat_profile(&memu_session).await {
        Ok(settings) => settings,
        Err(e) => return HttpResponse::BadGateway().json(serde_json::json!({ "error": e })),
    };
    let memu_tools = atomic_core::MemuToolConfig {
        base_url: memu_session.base_url,
        user_id: memu_session.user_id,
        soul_id: memu_session.soul_id,
    };

    let result = db
        .0
        .send_chat_message_with_external_settings(
            &conversation_id,
            &body.content,
            on_event,
            settings,
            Some(memu_tools),
            body.canvas_context,
            body.page_context,
        )
        .await;

    match result {
        Ok(message) => HttpResponse::Ok().json(message),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/memu/session/end", responses((status = 200, description = "Ended memU Atomic session")), tag = "chat")]
pub async fn end_memu_session(
    state: web::Data<AppState>,
    db: Db,
    body: web::Json<EndMemuSessionBody>,
) -> HttpResponse {
    let Some(memu_session) = state.memu_session.clone() else {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "MEMU_SERVER_URL, MEMU_USER_ID, and MEMU_SOUL_ID are required"
        }));
    };
    let conversation_id = body.conversation_id.trim();
    if conversation_id.is_empty() {
        return HttpResponse::BadRequest().json(serde_json::json!({"error": "conversation_id is required"}));
    }

    let mut conv = match db.0.get_conversation(conversation_id).await {
        Ok(Some(conv)) => conv,
        Ok(None) => return HttpResponse::NotFound().json(serde_json::json!({"error": "Conversation not found"})),
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
        let on_event = chat_event_callback(state.event_tx.clone());
        let recap_prompt = atomic_recap_prompt(&memu_session.user_id, &transcript_before_recap);
        if let Err(e) = db
            .0
            .send_chat_message_with_external_settings(
                conversation_id,
                &recap_prompt,
                on_event,
                settings,
                None,
                None,
                None,
            )
            .await
        {
            return crate::error::error_response(e);
        }
        conv = match db.0.get_conversation(conversation_id).await {
            Ok(Some(conv)) => conv,
            Ok(None) => return HttpResponse::NotFound().json(serde_json::json!({"error": "Conversation not found"})),
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
