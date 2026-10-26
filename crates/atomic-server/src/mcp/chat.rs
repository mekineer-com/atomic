//! A turn credential selects one captured chat dispatcher, never a database.

use super::AtomicMcpTransport;
use actix_web::{http::header, web, HttpMessage, HttpRequest, HttpResponse};
use atomic_core::{
    agent::ChatToolSession,
    executor::{self, WorkTracker},
};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    ErrorData, RoleServer, ServerHandler,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use uuid::Uuid;

pub const CHAT_TOOLS_PATH: &str = "/openalma/chat-tools/mcp";

#[derive(Clone, Default)]
pub struct ChatToolBridge {
    turns: Arc<Mutex<HashMap<String, AtomicMcpTransport<ChatMcpServer>>>>,
}

/// The admitted turn owns this lease through native completion and persistence.
/// Dropping it stops tool admission, drains accepted calls, then revokes access.
pub struct ChatToolLease {
    credential: String,
    bridge: ChatToolBridge,
    server: ChatMcpServer,
    transport: AtomicMcpTransport<ChatMcpServer>,
}

struct ChatTurn {
    accepting: Mutex<bool>,
    work: WorkTracker,
    state: AsyncMutex<ChatToolSession>,
    tools: Vec<Tool>,
}

#[derive(Clone)]
struct ChatMcpServer(Arc<ChatTurn>);

impl ChatToolBridge {
    pub fn register(&self, session: ChatToolSession) -> Result<ChatToolLease, String> {
        let tools = session
            .tools()
            .iter()
            .map(|tool| {
                let schema = tool
                    .parameters
                    .as_object()
                    .cloned()
                    .ok_or_else(|| format!("Chat tool {} has no object schema", tool.name))?;
                Ok(Tool::new(
                    tool.name.clone(),
                    tool.description.clone(),
                    schema,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let server = ChatMcpServer(Arc::new(ChatTurn {
            accepting: Mutex::new(true),
            work: WorkTracker::default(),
            state: AsyncMutex::new(session),
            tools,
        }));
        let transport = AtomicMcpTransport::with_server(server.clone(), Duration::from_secs(30));
        let credential = Uuid::new_v4().to_string();
        self.turns
            .lock()
            .expect("chat tool registry poisoned")
            .insert(credential.clone(), transport.clone());
        Ok(ChatToolLease {
            credential,
            bridge: self.clone(),
            server,
            transport,
        })
    }
}

impl ChatToolLease {
    pub fn credential(&self) -> &str {
        &self.credential
    }
}

impl Drop for ChatToolLease {
    fn drop(&mut self) {
        *self
            .server
            .0
            .accepting
            .lock()
            .expect("chat tool admission poisoned") = false;
        let credential = self.credential.clone();
        let bridge = self.bridge.clone();
        let turn = Arc::clone(&self.server.0);
        let transport = self.transport.clone();
        executor::spawn(async move {
            turn.work.wait_idle().await;
            bridge
                .turns
                .lock()
                .expect("chat tool registry poisoned")
                .remove(&credential);
            transport.close_idle_streams();
            transport.shutdown().await;
        });
    }
}

impl ChatMcpServer {
    async fn execute(
        &self,
        name: String,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let work = {
            let accepting = self
                .0
                .accepting
                .lock()
                .expect("chat tool admission poisoned");
            if !*accepting {
                return Err(ErrorData::invalid_request("This chat turn has ended", None));
            }
            self.0.work.start()
        };
        let turn = Arc::clone(&self.0);
        let (send, receive) = oneshot::channel();
        // Neither HTTP-body loss nor MCP cancellation owns the actual tool work.
        executor::spawn(async move {
            let _work = work;
            let result = turn
                .state
                .lock()
                .await
                .execute(&Uuid::new_v4().to_string(), &name, arguments)
                .await;
            let content = vec![Content::text(result.content)];
            let result = if result.is_error {
                CallToolResult::error(content)
            } else {
                CallToolResult::success(content)
            };
            let _ = send.send(result);
        });
        receive
            .await
            .map_err(|_| ErrorData::internal_error("Chat tool worker failed", None))
    }
}

impl ServerHandler for ChatMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: self.0.tools.clone(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.execute(
            request.name.into_owned(),
            serde_json::Value::Object(request.arguments.unwrap_or_default()),
        )
        .await
    }
}

pub async fn chat_tools(
    req: HttpRequest,
    body: web::Bytes,
    bridge: web::Data<ChatToolBridge>,
) -> actix_web::Result<HttpResponse> {
    let transport = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| {
            bridge
                .turns
                .lock()
                .expect("chat tool registry poisoned")
                .get(token)
                .cloned()
        });
    let Some(transport) = transport else {
        return Ok(HttpResponse::Unauthorized().finish());
    };
    if !req.query_string().is_empty() {
        return Ok(HttpResponse::BadRequest().body("Chat tools do not accept database selection"));
    }
    // A live credential belongs to work admitted before graceful drain began.
    req.extensions_mut()
        .insert(crate::shutdown::AdmittedRequest);
    transport.post(req, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{body::to_bytes, middleware, test, App};
    use atomic_core::{
        agent::{CanvasContext, ChatEvent, MemuToolConfig},
        AtomicCore,
    };
    use serde_json::{json, Value};

    async fn session(core: &AtomicCore, events: Arc<Mutex<Vec<ChatEvent>>>) -> ChatToolSession {
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let message = core
            .save_message(&conversation.conversation.id, "user", "Look at this canvas")
            .await
            .unwrap();
        let scope = MemuToolConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            user_id: "TestOwner".to_string(),
            soul_id: "TestSoul".to_string(),
        };
        let mut other_scope = scope.clone();
        other_scope.soul_id = "OtherSoul".to_string();
        assert!(core
            .chat_tool_session(
                &conversation.conversation.id,
                &message.id,
                HashMap::new(),
                other_scope,
                None,
                None,
                |_| {}
            )
            .await
            .is_err());
        assert!(core
            .chat_tool_session(
                &conversation.conversation.id,
                "another-turn",
                HashMap::new(),
                scope.clone(),
                None,
                None,
                |_| {}
            )
            .await
            .is_err());
        core.chat_tool_session(
            &conversation.conversation.id,
            &message.id,
            HashMap::new(),
            scope,
            None,
            Some(CanvasContext {
                clusters: Vec::new(),
            }),
            move |event| events.lock().unwrap().push(event),
        )
        .await
        .unwrap()
    }

    fn request(token: &str, id: Option<&str>, body: Value) -> test::TestRequest {
        let req = test::TestRequest::post()
            .uri(CHAT_TOOLS_PATH)
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .insert_header((header::ACCEPT, "application/json, text/event-stream"))
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_payload(body.to_string());
        match id {
            Some(id) => req.insert_header(("mcp-session-id", id)),
            None => req,
        }
    }

    #[actix_web::test]
    async fn http_bridge_is_turn_scoped_and_survives_drain() {
        let dir = tempfile::TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("first.db")).unwrap();
        let other = AtomicCore::open_or_create(dir.path().join("second.db")).unwrap();
        let bridge = web::Data::new(ChatToolBridge::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = session(&core, events.clone()).await;
        let focus_schema = captured
            .tools()
            .iter()
            .find(|tool| tool.name == "focus_atom")
            .unwrap()
            .parameters
            .clone();
        let lease = bridge.register(captured).unwrap();
        let other_lease = bridge
            .register(session(&other, Arc::new(Mutex::new(Vec::new()))).await)
            .unwrap();
        let shutdown = web::Data::new(crate::shutdown::Shutdown::default());
        let app = test::init_service(
            App::new()
                .app_data(bridge.clone())
                .app_data(shutdown.clone())
                .wrap(middleware::from_fn(crate::shutdown::admission))
                .route(CHAT_TOOLS_PATH, web::post().to(chat_tools))
                .route(
                    "/ordinary",
                    web::post().to(|| async { HttpResponse::Ok().finish() }),
                ),
        )
        .await;

        let invalid = request("ordinary-api-token", None, json!({})).to_request();
        assert_eq!(test::call_service(&app, invalid).await.status(), 401);
        let selected_db = request(lease.credential(), None, json!({}))
            .uri(&format!("{CHAT_TOOLS_PATH}?db=other"))
            .to_request();
        assert_eq!(test::call_service(&app, selected_db).await.status(), 400);
        let initialize = request(
            lease.credential(),
            None,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "fictional-client", "version": "0"},
                }
            }),
        )
        .to_request();
        let response = test::call_service(&app, initialize).await;
        assert_eq!(response.status(), 200);
        let id = response
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        to_bytes(response.into_body()).await.unwrap();
        let initialized = request(
            lease.credential(),
            Some(&id),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .to_request();
        assert_eq!(test::call_service(&app, initialized).await.status(), 202);
        let list = request(
            lease.credential(),
            Some(&id),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .to_request();
        let response = test::call_service(&app, list).await;
        let body = to_bytes(response.into_body()).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let frame: Value = serde_json::from_str(
            text.lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        let tools = frame["result"]["tools"].as_array().unwrap();
        assert!(!tools.iter().any(|tool| tool["name"] == "create_atom"));
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == "focus_atom")
                .unwrap()["inputSchema"],
            focus_schema
        );

        let crossed = request(
            other_lease.credential(),
            Some(&id),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        )
        .to_request();
        assert_eq!(test::call_service(&app, crossed).await.status(), 404);
        shutdown.begin();
        assert_eq!(
            test::call_service(
                &app,
                test::TestRequest::post().uri("/ordinary").to_request()
            )
            .await
            .status(),
            503
        );
        for (name, error) in [("focus_atom", false), ("create_atom", true)] {
            let call = request(lease.credential(), Some(&id), json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": name, "arguments": {"atom_id": "fictional-atom"}}})).to_request();
            let response = test::call_service(&app, call).await;
            assert_eq!(response.status(), 200);
            let body = to_bytes(response.into_body()).await.unwrap();
            let text = String::from_utf8(body.to_vec()).unwrap();
            let frame: Value = serde_json::from_str(
                text.lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(frame["result"]["isError"], error);
        }
        assert!(events.lock().unwrap().iter().any(|event| matches!(event, ChatEvent::CanvasAction { action, .. } if action == "focus_atom")));
        let token = lease.credential().to_string();
        drop(lease);
        drop(other_lease);
        tokio::time::timeout(Duration::from_secs(5), executor::wait_idle())
            .await
            .unwrap();
        let revoked = request(&token, Some(&id), json!({})).to_request();
        assert_eq!(test::call_service(&app, revoked).await.status(), 401);
    }

    #[actix_web::test]
    async fn caller_loss_and_lease_close_do_not_cancel_queued_tool_work() {
        let dir = tempfile::TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let bridge = ChatToolBridge::default();
        let lease = bridge
            .register(session(&core, events.clone()).await)
            .unwrap();
        let server = lease.server.clone();
        let held = server.0.state.lock().await;
        let mut call = Box::pin(server.execute(
            "focus_atom".to_string(),
            json!({"atom_id": "fictional-atom"}),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), call.as_mut())
                .await
                .is_err()
        );
        drop(call);
        drop(lease);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), server.0.work.wait_idle())
                .await
                .is_err()
        );
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), executor::wait_idle())
            .await
            .unwrap();
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ChatEvent::CanvasAction { .. })));
        assert!(server
            .execute("focus_atom".to_string(), json!({}))
            .await
            .is_err());
        assert!(bridge.turns.lock().unwrap().is_empty());
    }
}
