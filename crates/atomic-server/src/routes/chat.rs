//! Chat / Conversation routes

use crate::db_extractor::Db;
use crate::error::{ok_or_error, ApiErrorResponse};
use crate::event_bridge::chat_event_callback;
use crate::state::{AppState, MemuSessionConfig};
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use std::time::Duration;
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

fn hide_system_messages(
    mut conv: atomic_core::ConversationWithMessages,
) -> atomic_core::ConversationWithMessages {
    conv.messages.retain(|m| m.message.role != "system");
    conv
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
