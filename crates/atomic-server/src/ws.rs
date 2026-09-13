//! WebSocket endpoint for real-time event streaming

use crate::routes::memu_proxy;
use crate::state::{AppState, ServerEvent};
use actix_web::{web, HttpRequest, HttpResponse};
use std::collections::HashMap;
use tokio::sync::broadcast;

/// WebSocket upgrade handler
/// Auth via query param: /ws?token=xxx
pub async fn ws_handler(
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
    query: web::Query<WsQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let scope = if state.memu_session.is_some() {
        let Some(user_id) = query.user_id.clone().filter(|value| !value.is_empty()) else {
            return Ok(HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "user_id is required"})));
        };
        let Some(soul_id) = query.soul_id.clone().filter(|value| !value.is_empty()) else {
            return Ok(HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "soul_id is required"})));
        };
        match memu_proxy::validate_scope(&state, user_id, soul_id).await {
            Ok(scope) => Some(scope),
            Err(response) => return Ok(response),
        }
    } else {
        None
    };
    let (core, fixed_database) = if let Some(scope) = scope.as_ref() {
        match state
            .resolve_workspace_core(&scope.user_id, &scope.soul_id, query.db.as_deref())
            .await
        {
            Ok((core, database_id)) => (core, Some(database_id)),
            Err(error) => return Ok(crate::error::error_response(error)),
        }
    } else {
        let core = match query.db.as_deref() {
            Some(id) => state.manager.get_core(id).await,
            None => state.manager.active_core().await,
        }
        .map_err(|_| actix_web::error::ErrorBadRequest("Database not found"))?;
        (core, query.db.clone())
    };
    match core.verify_api_token(&query.token).await {
        Ok(Some(_)) => {}
        _ => return Ok(HttpResponse::Unauthorized().finish()),
    }

    let (response, mut session, _msg_stream) = actix_ws::handle(&req, stream)?;

    // Subscribe to broadcast channel
    let mut rx = state.event_tx.subscribe();
    let manager = state.manager.clone();

    // Spawn task to forward broadcast events to this WebSocket client
    actix_web::rt::spawn(async move {
        let mut ownership = HashMap::<(String, String), bool>::new();
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(scope) = scope.as_ref() {
                        let selected_database =
                            fixed_database.clone().or_else(|| manager.active_id().ok());
                        if event.database_id().is_some()
                            && event.database_id() != selected_database.as_deref()
                        {
                            continue;
                        }
                        if let Some((conversation_id, database_id)) =
                            event.conversation_id().zip(event.database_id())
                        {
                            let key = (database_id.to_string(), conversation_id.to_string());
                            let owned = if let Some(owned) = ownership.get(&key) {
                                *owned
                            } else {
                                let Ok(core) = manager.get_core(database_id).await else {
                                    continue;
                                };
                                let Ok(Some(conv)) = core.get_conversation(conversation_id).await
                                else {
                                    continue;
                                };
                                let owned = conv.conversation.user_id.as_deref()
                                    == Some(scope.user_id.as_str())
                                    && conv.conversation.soul_id.as_deref()
                                        == Some(scope.soul_id.as_str());
                                ownership.insert(key, owned);
                                owned
                            };
                            if !owned {
                                continue;
                            }
                        } else if event.owner()
                            != Some((scope.user_id.as_str(), scope.soul_id.as_str()))
                        {
                            // Integrated sockets deny events without explicit provenance.
                            continue;
                        }
                    }
                    if let Ok(json) = serde_json::to_string(&event) {
                        if session.text(json).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("WebSocket client lagged, skipped {} events", n);
                    let event = ServerEvent::EventsLagged { skipped: n };
                    if let Ok(json) = serde_json::to_string(&event) {
                        if session.text(json).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct WsQuery {
    pub token: String,
    pub db: Option<String>,
    pub user_id: Option<String>,
    pub soul_id: Option<String>,
}
