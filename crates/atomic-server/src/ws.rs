//! WebSocket endpoint for real-time event streaming

use crate::routes::memu_proxy;
use crate::state::{AppState, ServerEvent};
use actix_web::{web, HttpRequest, HttpResponse};
use tokio::sync::broadcast;

/// WebSocket upgrade handler
/// Auth via query param: /ws?token=xxx
pub async fn ws_handler(
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
    query: web::Query<WsQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    // Authenticate via query param
    let core = state
        .manager
        .active_core()
        .await
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to get database"))?;
    match core.verify_api_token(&query.token).await {
        Ok(Some(_)) => {}
        _ => return Ok(HttpResponse::Unauthorized().finish()),
    }
    let scope = if state.memu_session.is_some() {
        match memu_proxy::validate_scope(
            &state,
            query.user_id.clone().unwrap_or_default(),
            query.soul_id.clone().unwrap_or_default(),
        )
        .await
        {
            Ok(scope) => Some(scope),
            Err(response) => return Ok(response),
        }
    } else {
        None
    };

    let (response, mut session, _msg_stream) = actix_ws::handle(&req, stream)?;

    // Subscribe to broadcast channel
    let mut rx = state.event_tx.subscribe();

    // Spawn task to forward broadcast events to this WebSocket client
    actix_web::rt::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let (Some(conversation_id), Some(scope)) =
                        (event.conversation_id(), scope.as_ref())
                    {
                        let owned = core
                            .get_conversation(conversation_id)
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|conv| {
                                conv.conversation.user_id.as_deref() == Some(scope.user_id.as_str())
                                    && conv.conversation.soul_id.as_deref()
                                        == Some(scope.soul_id.as_str())
                            });
                        if !owned {
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
    pub user_id: Option<String>,
    pub soul_id: Option<String>,
}
