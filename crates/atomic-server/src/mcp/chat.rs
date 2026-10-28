//! A turn credential selects one captured chat dispatcher, never a database.

use super::AtomicMcpTransport;
use actix_web::{http::header, web, HttpMessage, HttpRequest, HttpResponse};
use atomic_core::{
    agent::{ChatToolRecords, ChatToolSession},
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
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

pub const CHAT_TOOLS_PATH: &str = "/openalma/chat-tools/mcp";

#[derive(Clone, Default)]
pub struct ChatToolBridge {
    turns: Arc<Mutex<HashMap<String, (AtomicMcpTransport<ChatMcpServer>, Arc<ChatTurn>)>>>,
}

/// Finish the lease before publishing its tool state. Drop is abandonment cleanup.
pub struct ChatToolLease {
    credential: String,
    bridge: ChatToolBridge,
    server: ChatMcpServer,
    transport: AtomicMcpTransport<ChatMcpServer>,
    finished: bool,
}

struct ChatTurn {
    binding: (String, String, String, String),
    accepting: Mutex<bool>,
    work: WorkTracker,
    state: AsyncMutex<Option<ChatToolSession>>,
    tools: Vec<Tool>,
}

#[derive(Clone)]
struct ChatMcpServer(Arc<ChatTurn>);

impl ChatToolBridge {
    pub fn register(&self, session: ChatToolSession) -> Result<ChatToolLease, String> {
        let (owner, soul, conversation, message) = session.binding()
            .ok_or("Scoped tools require a saved owner/Soul/user turn")?;
        let binding = (owner.into(), soul.into(), conversation.into(), message.into());
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
            binding,
            accepting: Mutex::new(true),
            work: WorkTracker::default(),
            state: AsyncMutex::new(Some(session)),
            tools,
        }));
        let transport = AtomicMcpTransport::with_server(server.clone(), Duration::from_secs(30));
        let credential = Uuid::new_v4().to_string();
        self.turns
            .lock()
            .expect("chat tool registry poisoned")
            .insert(credential.clone(), (transport.clone(), Arc::clone(&server.0)));
        Ok(ChatToolLease {
            credential,
            bridge: self.clone(),
            server,
            transport,
            finished: false,
        })
    }

    /// Caller owns the sitting lock and has proved native writer exit. Drop's
    /// orphan cleanup can still be draining after that worker released its lock.
    pub async fn drain_sitting(&self, owner: &str, soul: &str, conversation: &str)
        -> Vec<(String, ChatToolRecords)> {
        let turns: Vec<_> = self.turns.lock().expect("chat tool registry poisoned").iter()
            .filter(|(_, (_, turn))| turn.binding.0 == owner && turn.binding.1 == soul && turn.binding.2 == conversation)
            .map(|(credential, (transport, turn))| (credential.clone(), transport.clone(), Arc::clone(turn)))
            .collect();
        let mut records = Vec::new();
        for (credential, transport, turn) in turns {
            *turn.accepting.lock().expect("chat tool admission poisoned") = false;
            drain_turn(self.clone(), credential, Arc::clone(&turn), transport).await;
            if let Some(session) = turn.state.lock().await.take() {
                records.push((turn.binding.3.clone(), session.finish()));
            }
        }
        records
    }
}

impl ChatToolLease {
    pub fn credential(&self) -> &str {
        &self.credential
    }

    fn stop_admission(&self) {
        *self
            .server
            .0
            .accepting
            .lock()
            .expect("chat tool admission poisoned") = false;
    }

    pub async fn finish(mut self) -> ChatToolRecords {
        self.stop_admission();
        drain_turn(
            self.bridge.clone(),
            self.credential.clone(),
            Arc::clone(&self.server.0),
            self.transport.clone(),
        )
        .await;
        let records = self
            .server
            .0
            .state
            .lock()
            .await
            .take()
            .expect("finished tool session")
            .finish();
        self.finished = true;
        records
    }
}

async fn drain_turn(
    bridge: ChatToolBridge,
    credential: String,
    turn: Arc<ChatTurn>,
    transport: AtomicMcpTransport<ChatMcpServer>,
) {
    turn.work.wait_idle().await;
    bridge
        .turns
        .lock()
        .expect("chat tool registry poisoned")
        .remove(&credential);
    transport.close_idle_streams();
    transport.shutdown().await;
}

impl Drop for ChatToolLease {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.stop_admission();
        let credential = self.credential.clone();
        let bridge = self.bridge.clone();
        let turn = Arc::clone(&self.server.0);
        let transport = self.transport.clone();
        executor::spawn(async move {
            drain_turn(bridge, credential, turn, transport).await;
        });
    }
}

impl ChatMcpServer {
    async fn execute(
        &self,
        call_id: String,
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
        // The turn's executor owns work even if a caller drops this future;
        // do not make that guarantee depend on rmcp's detached-handler policy.
        let receive = executor::spawn_with_result(async move {
            let _work = work;
            let result = turn
                .state
                .lock()
                .await
                .as_mut()
                .expect("active tool session")
                .execute(&call_id, &name, arguments)
                .await;
            match result {
                Ok(result) => {
                    let content = vec![Content::text(result.content)];
                    Ok(if result.is_error {
                        CallToolResult::error(content)
                    } else {
                        CallToolResult::success(content)
                    })
                }
                Err(error) => Err(ErrorData::internal_error(error, None)),
            }
        });
        receive
            .await
            .map_err(|_| ErrorData::internal_error("Chat tool worker failed", None))?
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
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let call_id = context
            .meta
            .get("claudecode/toolUseId")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ErrorData::invalid_request("Claude tool call is missing claudecode/toolUseId", None)
            })?;
        self.execute(
            call_id.to_string(),
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
            .map(|(transport, _)| transport.clone())
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
    transport.handle(req, body).await
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
        let mut other_owner = scope.clone();
        other_owner.user_id = "OtherOwner".to_string();
        assert!(core
            .chat_tool_session(
                &conversation.conversation.id,
                &message.id,
                HashMap::new(),
                other_owner,
                None,
                None,
                |_| {}
            )
            .await
            .is_err());
        let assistant = core
            .save_message(&conversation.conversation.id, "assistant", "Earlier reply")
            .await
            .unwrap();
        assert!(core
            .chat_tool_session(
                &conversation.conversation.id,
                &assistant.id,
                HashMap::new(),
                scope.clone(),
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
                .service(
                    web::resource(CHAT_TOOLS_PATH)
                        .route(web::post().to(chat_tools))
                        .route(web::get().to(chat_tools))
                        .route(web::delete().to(chat_tools)),
                )
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
        let missing_meta = request(lease.credential(), Some(&id), json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "focus_atom", "arguments": {}}})).to_request();
        let response = test::call_service(&app, missing_meta).await;
        let body = to_bytes(response.into_body()).await.unwrap();
        assert!(String::from_utf8(body.to_vec())
            .unwrap()
            .contains("missing claudecode/toolUseId"));
        let unauthorized_get = test::TestRequest::get().uri(CHAT_TOOLS_PATH).to_request();
        assert_eq!(
            test::call_service(&app, unauthorized_get).await.status(),
            401
        );
        let idle = test::TestRequest::get()
            .uri(CHAT_TOOLS_PATH)
            .insert_header((
                header::AUTHORIZATION,
                format!("Bearer {}", lease.credential()),
            ))
            .insert_header((header::ACCEPT, "text/event-stream"))
            .insert_header(("mcp-session-id", id.clone()))
            .to_request();
        let response = test::call_service(&app, idle).await;
        assert_eq!(response.status(), 200);
        let mut idle_body = Box::pin(to_bytes(response.into_body()));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), idle_body.as_mut())
                .await
                .is_err()
        );
        let missing_delete = test::TestRequest::delete()
            .uri(CHAT_TOOLS_PATH)
            .insert_header((
                header::AUTHORIZATION,
                format!("Bearer {}", lease.credential()),
            ))
            .insert_header(("mcp-session-id", "unknown-session"))
            .to_request();
        assert_eq!(test::call_service(&app, missing_delete).await.status(), 404);
        for (name, error) in [("focus_atom", false), ("create_atom", true)] {
            let call = request(lease.credential(), Some(&id), json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": name, "arguments": {"atom_id": "fictional-atom"}, "_meta": {"claudecode/toolUseId": format!("toolu-{name}")}}})).to_request();
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
        let records = lease.finish().await;
        assert_eq!(records.tool_calls.len(), 2);
        assert_eq!(records.tool_calls[0].id, "toolu-focus_atom");
        assert_eq!(records.tool_calls[1].id, "toolu-create_atom");
        assert_eq!(records.tool_calls[1].status, "failed");
        assert!(records.citations.is_empty());
        tokio::time::timeout(Duration::from_secs(5), idle_body)
            .await
            .unwrap()
            .unwrap();
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
        let binding = server.0.binding.clone();
        let held = server.0.state.lock().await;
        let mut call = Box::pin(server.execute(
            "toolu-detached".to_string(),
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
        assert!(bridge.drain_sitting("TestOwner", "OtherSoul", &binding.2).await.is_empty());
        let mut drain = Box::pin(bridge.drain_sitting(&binding.0, &binding.1, &binding.2));
        assert!(tokio::time::timeout(Duration::from_millis(10), drain.as_mut()).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), server.0.work.wait_idle())
                .await
                .is_err()
        );
        drop(held);
        let recovered = tokio::time::timeout(Duration::from_secs(5), drain).await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].0, binding.3);
        assert_eq!(recovered[0].1.tool_calls[0].id, "toolu-detached");
        tokio::time::timeout(Duration::from_secs(5), executor::wait_idle())
            .await
            .unwrap();
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ChatEvent::CanvasAction { .. })));
        assert!(server
            .execute(
                "toolu-after-close".to_string(),
                "focus_atom".to_string(),
                json!({})
            )
            .await
            .is_err());
        assert!(bridge.turns.lock().unwrap().is_empty());
    }

    #[actix_web::test]
    async fn finish_waits_for_tools_then_returns_records_and_revokes_the_credential() {
        let dir = tempfile::TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let bridge = ChatToolBridge::default();
        let lease = bridge
            .register(session(&core, Arc::new(Mutex::new(Vec::new()))).await)
            .unwrap();
        let token = lease.credential().to_string();
        let server = lease.server.clone();
        let held = server.0.state.lock().await;
        let mut call = Box::pin(server.execute(
            "toolu-finish".to_string(),
            "focus_atom".to_string(),
            json!({"atom_id": "fictional-atom"}),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), call.as_mut())
                .await
                .is_err()
        );
        drop(call);
        let mut finish = Box::pin(lease.finish());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), finish.as_mut())
                .await
                .is_err()
        );
        assert!(bridge.turns.lock().unwrap().contains_key(&token));
        drop(held);
        let records = tokio::time::timeout(Duration::from_secs(5), finish)
            .await
            .unwrap();
        assert_eq!(records.tool_calls.len(), 1);
        assert_eq!(records.tool_calls[0].id, "toolu-finish");
        assert_eq!(records.tool_calls[0].status, "complete");
        assert!(bridge.turns.lock().unwrap().is_empty());
    }
}
