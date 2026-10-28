//! Dormant native execution; admission/recovery stays in the sitting lifecycle.

use super::*;
use crate::mcp::chat::{ChatToolBridge, CHAT_TOOLS_PATH};
use atomic_core::{
    agent::{ChatToolSession, ClaudeChatInput},
    AtomicChatUsage, ChatEvent, ChatMessageWithContext,
};
use serde_json::Value;

#[derive(Clone, Copy, Serialize)]
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
    pub operation: Operation,
    pub purpose: Purpose,
    pub input: ClaudeChatInput,
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
    message_id: String,
    usage: UsageCounts,
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
}

#[derive(Deserialize)]
pub(super) struct NativeReceipt {
    pub session_id: String,
    pub turn_id: String,
    pub purpose: String,
    pub status: String,
    pub submission: String,
    pub initialized: bool,
    pub usage: Option<NativeUsage>,
    pub text: Option<String>,
    pub error: Option<String>,
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
        return Err(format!("Native status failed ({})", response.status()));
    }
    response
        .json()
        .await
        .map_err(|e| format!("Invalid native status: {e}"))
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
        profile: Value,
    },
    Text {
        #[serde(flatten)]
        identity: BlockIdentity,
        mode: String,
        text: String,
    },
    ToolCall {
        #[serde(flatten)]
        _identity: BlockIdentity,
        call_id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        #[serde(flatten)]
        _identity: BlockIdentity,
        call_id: String,
        content: Value,
        is_error: bool,
    },
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
        fingerprint: &str,
        on_event: &impl Fn(ChatEvent),
    ) -> Result<Option<String>, String> {
        match frame {
            Frame::Session {
                session_id,
                turn_id,
                profile,
            } => {
                if turn_id != turn.turn_id
                    || profile["fingerprint"].as_str() != Some(fingerprint)
                    || self.session_id.as_ref().is_some_and(|id| id != &session_id)
                {
                    return Err("Native session/profile does not match this turn".into());
                }
                self.session_id = Some(session_id);
            }
            Frame::Text {
                identity,
                mode,
                text,
            } => {
                if mode != "replace" {
                    return Err("Invalid native text snapshot mode".into());
                }
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
                if turn.purpose == Purpose::Recap && text.trim().is_empty() {
                    return Err("The recap was empty. Retry close-out.".into());
                }
                return Ok(Some(text));
            }
            Frame::Error { error } => return Err(error),
            Frame::ToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let _ = (call_id, name, input);
            }
            Frame::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                let _ = (call_id, content, is_error);
            }
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
    let mut request = client
        .post(format!("{}/integration/atomic/chat", scope.base_url))
        .json(&serde_json::json!({
            "user_id": scope.user_id, "soul_id": scope.soul_id,
            "conversation_id": turn.conversation_id, "turn_id": turn.turn_id,
            "operation": turn.operation, "purpose": turn.purpose,
            "prompt": turn.input.prompt, "system_prompt": turn.input.system_prompt,
            "bridge_url": bridge_url, "profile_fingerprint": profile.fingerprint,
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
        stream.refused = code.is_client_error();
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
            if let Some(text) = stream.accept(frame, turn, &profile.fingerprint, on_event)? {
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
                db.0.fail_chat_turn(turn.conversation_id, turn.turn_id, &error, false, None)
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
    let evidence = loop {
        let evidence = status(scope, turn.conversation_id, turn.turn_id).await;
        if stream.refused
            || evidence.as_ref().is_ok_and(|e| e.writer_idle == Some(true))
            || started.elapsed() >= observation_budget
        {
            break evidence;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    // Stop admission and drain actual tools on every path, before terminal publication.
    let records = match lease {
        Some(lease) => lease.finish().await,
        None => recap_records.expect("recap records"),
    };
    let result = {
        let error = match &result {
            Ok(_) => "Native completion could not be reconciled".to_string(),
            Err(error) => error.clone(),
        };
        // Stream loss/overflow can leave the native producer running. Never infer
        // writer exit from this Rust worker or its drained tool lease.
        match evidence {
            Ok(status) => match status.turn {
                Some(receipt)
                    if receipt.turn_id == turn.turn_id
                        && Some(&receipt.session_id) == status.session_id.as_ref()
                        && stream
                            .session_id
                            .as_ref()
                            .is_none_or(|id| id == &receipt.session_id)
                        && receipt.purpose
                            == if turn.purpose == Purpose::Reply {
                                "reply"
                            } else {
                                "recap"
                            } =>
                {
                    if let Some(usage) = receipt.usage {
                        stream.usage = Some(usage.snapshot(turn.turn_id));
                    }
                    if status.writer_idle == Some(true)
                        && receipt.status == "complete"
                        && !(turn.purpose == Purpose::Recap
                            && receipt
                                .text
                                .as_ref()
                                .is_some_and(|text| text.trim().is_empty()))
                    {
                        // Prefer the persisted terminal result on stream recovery;
                        // direct completion must agree with that same receipt.
                        match (result, receipt.text) {
                            (Ok(text), Some(saved)) if text == saved => Ok(text),
                            (Err(_), Some(saved)) => Ok(saved),
                            _ => Err((error, true)),
                        }
                    } else {
                        Err((
                            error,
                            !(status.writer_idle == Some(true) && receipt.status == "failed"),
                        ))
                    }
                }
                _ => Err((error, !stream.refused)),
            },
            Err(_) => Err((error, !stream.refused)),
        }
    };
    let text = match result {
        Ok(text) => text,
        Err((error, uncertain)) => {
            // Finish can know an outcome whose per-tool checkpoint failed.
            db.0.save_chat_tool_records(turn.conversation_id, turn.turn_id, &records)
                .await
                .map_err(core_json_error)?;
            let uncertain = uncertain
                || records.tool_calls.iter().any(|call| {
                    call.status == "running"
                        || call
                            .tool_output
                            .as_ref()
                            .and_then(|output| output.get("outcome_unknown"))
                            .and_then(Value::as_bool)
                            == Some(true)
                });
            db.0.fail_chat_turn(
                turn.conversation_id,
                turn.turn_id,
                &error,
                uncertain,
                stream.usage.as_ref(),
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
            stream.usage.as_ref(),
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
        status_reads: AtomicUsize,
    }

    fn counts() -> Value {
        serde_json::json!({"input_tokens":20,"cache_creation_input_tokens":30,
            "cache_read_input_tokens":100,"output_tokens":5})
    }

    #[test]
    fn snapshots_replace_blocks_and_usage_without_mixing_nested_messages() {
        let turn = NativeTurn {
            conversation_id: "conversation",
            turn_id: "turn",
            operation: Operation::Continue,
            purpose: Purpose::Reply,
            input: ClaudeChatInput {
                prompt: "new input".into(),
                system_prompt: None,
            },
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
                .accept(
                    serde_json::from_value(value).unwrap(),
                    &turn,
                    "fingerprint",
                    &on_event,
                )
                .unwrap()
        };
        accept(
            serde_json::json!({"type":"session","session_id":"native","turn_id":"turn","profile":{"fingerprint":"fingerprint"}}),
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
    }

    async fn mock_chat(
        mock: web::Data<MockNative>,
        request: HttpRequest,
        body: web::Json<Value>,
    ) -> HttpResponse {
        assert_eq!(body["profile_fingerprint"], "fingerprint");
        if mock.mode == "recap" {
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
            assert_eq!(body["operation"], "new");
            assert_eq!(body["system_prompt"], "saved presence");
            assert!(body["bridge_url"]
                .as_str()
                .unwrap()
                .ends_with(CHAT_TOOLS_PATH));
        }
        if mock.mode == "refused" {
            return HttpResponse::Conflict()
                .json(serde_json::json!({"detail":"Profile changed; reload before sending"}));
        }
        let mut frames = vec![
            serde_json::json!({"type":"session","session_id":"native-id","turn_id":body["turn_id"],"profile":{"fingerprint":"fingerprint"}}),
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
            "failed" => frames.push(serde_json::json!({"type":"error","error":"Recorded provider error"})),
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
        if mock.mode == "refused" {
            return HttpResponse::Ok()
                .json(serde_json::json!({"session_id":null,"writer_idle":true,"turn":null}));
        }
        let active =
            mock.mode == "active" && mock.status_reads.fetch_add(1, Ordering::Relaxed) == 0;
        if active {
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
            "success" | "recover" | "recap" => "complete",
            "active" if !active => "complete",
            "failed" => "failed",
            _ => "uncertain",
        };
        HttpResponse::Ok().json(serde_json::json!({"session_id":"native-id","writer_idle":!active,
            "turn":{"session_id":"native-id","turn_id":query["turn_id"],"purpose":if mock.mode == "recap" { "recap" } else { "reply" },
                "status":status,"submission":"recorded","initialized":true,
                "usage":{"message_id":"message","usage":counts()},
                "text":if mock.mode == "recap" { Some("Exact recap") }
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
            "active",
            "recap",
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
            let mut input =
                db.0.admit_chat_turn(
                    id,
                    &scope.user_id,
                    &scope.soul_id,
                    &Backend::ClaudeCode,
                    &ChatRequest::New {
                        content: "fictional input".into(),
                        page_context: None,
                        canvas_context: None,
                    },
                    128000,
                )
                .await
                .unwrap();
            if mode == "recap" {
                db.0.fail_chat_turn(id, &input.id, "Recorded failure", false, None)
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
            let profile = ClaudeChatProfile {
                model: "fictional".into(),
                context_window_tokens: 128000,
                timeout_seconds: 1,
                effort: "medium".into(),
                fingerprint: "fingerprint".into(),
                memu_response_sentences: 0,
                memu_log_prompts: false,
            };
            let turn = NativeTurn {
                conversation_id: id,
                turn_id: &input.id,
                operation: if mode == "recap" {
                    Operation::Continue
                } else {
                    Operation::New
                },
                purpose: if mode == "recap" {
                    Purpose::Recap
                } else {
                    Purpose::Reply
                },
                input: ClaudeChatInput {
                    prompt: input.content.clone(),
                    system_prompt: (mode != "recap").then(|| "saved presence".into()),
                },
            };
            let mut events = state.event_tx.subscribe();
            let result = execute(
                &state, &db, &scope, &profile, &bridge, &base_url, turn, session,
            )
            .await;
            let saved = db.0.get_conversation(id).await.unwrap().unwrap();
            let lifecycle = saved.conversation.atomic_chat_state.unwrap();
            if mode == "refused" {
                assert!(lifecycle.native_usage.is_none());
            } else {
                assert_eq!(lifecycle.native_usage.unwrap().cache_read_input_tokens, 100);
            }
            assert!(saved.messages.iter().all(|row| row.tool_calls.is_empty()));
            match mode {
                "recap" => {
                    assert_eq!(result.unwrap().message.content, "Exact recap");
                    assert!(matches!(
                        lifecycle.recap.unwrap().outcome,
                        RecapOutcome::Complete { .. }
                    ));
                    while let Ok(event) = events.try_recv() {
                        assert!(!matches!(
                            event,
                            ServerEvent::ChatStreamDelta { .. } | ServerEvent::ChatComplete { .. }
                        ));
                    }
                }
                "success" | "recover" | "active" => {
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
                    assert!(result.is_err());
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
            if mode != "recap" {
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
