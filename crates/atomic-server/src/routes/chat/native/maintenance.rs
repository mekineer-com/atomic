//! Maintenance handoff only; the sitting lifecycle chooses when to invoke it.
use super::*;
use atomic_core::{AtomicChatMaintenance as Maintenance, AtomicChatMaintenanceKind as Kind,
    AtomicChatMaintenanceStatus as Status, AtomicChatState};

#[derive(Deserialize)]
pub(crate) struct Receipt {
    attempt_id: String,
    native_attempt_id: String,
    turn_id: String,
    action: String,
    pub(super) status: Status,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Response {
    maintenance: Receipt,
    context: Option<NativeContext>,
}

fn failure(error: impl std::fmt::Display) -> JsonError {
    (StatusCode::CONFLICT, serde_json::json!({"error":error.to_string()}))
}

fn action(kind: Kind) -> &'static str {
    if kind == Kind::Purge { "purge" } else { "compact" }
}

async fn checkpoint(state: &AppState, db: &Db, scope: &MemuScope, id: &str,
                    saved: &AtomicChatState, next: &Maintenance, context: Option<usize>) -> Result<(), JsonError> {
    if saved.maintenance.as_ref() != Some(next) || saved.native_context_tokens != context {
        db.0.set_chat_maintenance(id, &scope.user_id, &scope.soul_id, saved, next, context)
            .await.map_err(core_json_error)?;
        chat_changed(state, db, id);
    }
    Ok(())
}

async fn apply(state: &AppState, db: &Db, scope: &MemuScope, id: &str,
               saved: &AtomicChatState, receipt: Receipt, context: Option<NativeContext>) -> Result<(), JsonError> {
    let pending = saved.maintenance.as_ref().expect("saved maintenance intent");
    if receipt.attempt_id != pending.attempt_id || receipt.native_attempt_id != pending.native_attempt_id
        || receipt.turn_id != pending.turn_id || receipt.action != action(pending.kind)
        || receipt.status == Status::Pending
    {
        return Err(failure("Native maintenance response does not match the saved attempt; keep this history and reload."));
    }
    let mut next = pending.clone();
    next.status = receipt.status;
    next.error = receipt.error;
    next.awaiting_measurement = next.kind == Kind::Ordinary && next.status == Status::Complete;
    checkpoint(state, db, scope, id, saved, &next,
               context.map(|context| context.estimated_tokens)).await?;
    if next.unresolved() || (next.kind == Kind::Purge && next.status != Status::Complete) {
        return Err(failure(next.error.as_deref().unwrap_or("Native maintenance is still pending; reload to recover the same attempt.")));
    }
    Ok(())
}

async fn submit(state: &AppState, db: &Db, scope: &MemuScope, id: &str,
                saved: &AtomicChatState) -> Result<(), JsonError> {
    let pending = saved.maintenance.as_ref().expect("saved maintenance intent");
    let response = reqwest::Client::new()
        .post(format!("{}/integration/atomic/chat/maintenance", scope.base_url))
        .json(&serde_json::json!({"user_id":scope.user_id, "soul_id":scope.soul_id,
            "conversation_id":id, "turn_id":pending.turn_id,
            "native_attempt_id":pending.native_attempt_id, "attempt_id":pending.attempt_id,
            "previous_attempt_id":pending.previous_attempt_id, "action":action(pending.kind)}))
        .timeout(Duration::from_secs(30)).send().await.map_err(failure)?;
    if !response.status().is_success() {
        let code = response.status();
        let error = format!("Native maintenance was not confirmed ({code}): {}. Reload to recover this attempt.",
                            response.text().await.map_err(failure)?);
        if code == reqwest::StatusCode::CONFLICT {
            // MCP's 409 is an admission refusal, not a lost completion response.
            let mut refused = pending.clone();
            refused.status = Status::Failed;
            refused.error = Some(error.clone());
            if refused.kind == Kind::Final { refused.final_used = false; }
            checkpoint(state, db, scope, id, saved, &refused, saved.native_context_tokens).await?;
        }
        return Err(failure(error));
    }
    let response: Response = response.json().await.map_err(failure)?;
    apply(state, db, scope, id, saved, response.maintenance, response.context).await
}

pub(super) async fn reconcile(state: &AppState, db: &Db, scope: &MemuScope,
    bridge: &ChatToolBridge, id: &str, work: &ConversationWork) -> Result<(), JsonError> {
    if !work.matches(db, id) {
        return Err(failure("Maintenance recovery requires this sitting's lock"));
    }
    let conv = read_chat(db, id).await?;
    if !conversation_matches(&conv, scope) { return Err(failure("Conversation owner/Soul changed")); }
    let Some(saved) = conv.conversation.atomic_chat_state.as_ref() else { return Ok(()); };
    let Some(pending) = saved.maintenance.as_ref() else { return Ok(()); };
    if !pending.unresolved() && !(pending.kind == Kind::Purge && pending.status == Status::Failed) {
        return Ok(());
    }
    let observed = fetch_status(scope, id, None).await.map_err(failure)?;
    if observed.writer_idle != Some(true) {
        return Err(failure("Native writer exit is not established; maintenance is retained."));
    }
    if let Some(receipt) = observed.maintenance {
        if receipt.attempt_id == pending.attempt_id {
            return apply(state, db, scope, id, saved, receipt, observed.context).await;
        }
        if Some(&receipt.attempt_id) != pending.previous_attempt_id.as_ref() {
            return Err(failure("Native maintenance changed outside this saved attempt; keep history and reload."));
        }
    } else if pending.previous_attempt_id.is_some() {
        return Err(failure("Previous native maintenance evidence is missing; keep history and reload."));
    }
    // No matching server claim exists: replay the saved handoff, not the cleanup
    // under a fresh ID. MCP's native/prior-attempt comparison owns admission.
    for (message_id, records) in bridge.drain_sitting(&scope.user_id, &scope.soul_id, work).await {
        db.0.save_chat_tool_records(id, &message_id, &records).await.map_err(core_json_error)?;
    }
    submit(state, db, scope, id, saved).await
}

pub(crate) async fn run(state: &AppState, db: &Db, scope: &MemuScope,
    bridge: &ChatToolBridge, id: &str, kind: Kind, work: &ConversationWork) -> Result<(), JsonError> {
    reconcile(state, db, scope, bridge, id, work).await?;
    let conv = read_chat(db, id).await?;
    if !conversation_matches(&conv, scope) { return Err(failure("Conversation owner/Soul changed")); }
    let saved = conv.conversation.atomic_chat_state.as_ref()
        .filter(|saved| saved.backend == Backend::ClaudeCode)
        .ok_or_else(|| failure("Maintenance requires a Claude Code sitting"))?;
    if kind == Kind::Purge && saved.phase == Phase::Closed
        && saved.maintenance.as_ref().is_some_and(|prior| prior.kind == Kind::Purge && prior.status == Status::Complete)
    {
        return Ok(());
    }
    let phase = match kind { Kind::Ordinary => Phase::Open, Kind::Final => Phase::Closing, Kind::Purge => Phase::Closed };
    if saved.phase != phase || saved.active.as_ref().is_some_and(|turn| turn.status == TurnStatus::Running)
        || saved.recap.as_ref().is_some_and(|recap| matches!(recap.outcome, RecapOutcome::Running))
        || (kind == Kind::Ordinary && saved.active.is_some())
    {
        return Err(failure("Maintenance requires the matching idle sitting phase"));
    }
    let previous = saved.maintenance.as_ref();
    if previous.is_some_and(|prior| prior.unresolved()
        || (kind == Kind::Ordinary && (prior.ordinary_disabled || prior.awaiting_measurement))
        || (kind == Kind::Final && prior.final_used)
        || (kind != Kind::Purge && prior.status == Status::Blocked)) {
        return Err(failure("This sitting has no eligible cleanup attempt; keep its saved maintenance outcome."));
    }
    let observed = fetch_status(scope, id, None).await.map_err(failure)?;
    if observed.writer_idle != Some(true) { return Err(failure("Native writer exit is not established")); }
    let Some(receipt) = observed.turn else {
        return if kind == Kind::Purge && observed.session_id.is_none() { Ok(()) }
            else { Err(failure("Native maintenance has no sitting/turn evidence")) };
    };
    if observed.session_id.as_deref() != Some(receipt.session_id.as_str()) {
        return Err(failure("Native maintenance sitting binding does not match"));
    }
    // Reuse the existing bridge drain; never infer tool exit from the HTTP stream.
    for (message_id, records) in bridge.drain_sitting(&scope.user_id, &scope.soul_id, work).await {
        db.0.save_chat_tool_records(id, &message_id, &records).await.map_err(core_json_error)?;
    }
    let next = Maintenance { attempt_id:uuid::Uuid::new_v4().to_string(),
        native_attempt_id:receipt.attempt_id, turn_id:receipt.turn_id,
        previous_attempt_id:observed.maintenance.as_ref().map(|prior| prior.attempt_id.clone()),
        kind, status:Status::Pending, error:None,
        ordinary_disabled:previous.is_some_and(|prior| prior.ordinary_disabled),
        awaiting_measurement:false, final_used:kind == Kind::Final || previous.is_some_and(|prior| prior.final_used) };
    checkpoint(state, db, scope, id, saved, &next, saved.native_context_tokens).await?;
    let mut admitted = saved.clone();
    admitted.maintenance = Some(next);
    submit(state, db, scope, id, &admitted).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpServer};
    use std::sync::Mutex;

    struct Mock {
        mode: &'static str,
        db: Db,
        id: String,
        requests: Mutex<Vec<Value>>,
        receipt: Mutex<Option<Value>>,
    }

    fn context() -> Value {
        serde_json::json!({"estimated_tokens":9000,"overhead_tokens":50,"system_tokens":25})
    }

    async fn observe(mock: web::Data<Mock>, query: web::Query<HashMap<String, String>>) -> HttpResponse {
        assert!(!query.contains_key("turn_id"), "maintenance reads the latest scoped basis");
        HttpResponse::Ok().json(serde_json::json!({"session_id":"native-id", "writer_idle":true,
            "context":context(), "maintenance":mock.receipt.lock().unwrap().clone(),
            "turn":{"session_id":"native-id","turn_id":"user-turn","attempt_id":"native-attempt",
                "purpose":"reply","status":"complete","initialized":true,"input_recorded":true}}))
    }

    async fn handoff(mock: web::Data<Mock>, body: web::Json<Value>) -> HttpResponse {
        let saved = read_chat(&mock.db, &mock.id).await.unwrap();
        let pending = saved.conversation.atomic_chat_state.unwrap().maintenance.unwrap();
        assert_eq!(pending.status, Status::Pending, "intent precedes the HTTP handoff");
        assert_eq!(body["attempt_id"], pending.attempt_id);
        let first = {
            let mut requests = mock.requests.lock().unwrap();
            requests.push((*body).clone());
            requests.len() == 1
        };
        if mock.mode == "refused" { return HttpResponse::Conflict().json(serde_json::json!({"detail":"helper unavailable"})); }
        if mock.mode == "unsubmitted" && first { return HttpResponse::InternalServerError().finish(); }
        let mut receipt = body.into_inner();
        receipt["status"] = serde_json::json!("complete");
        if mock.mode == "wrong_binding" { receipt["native_attempt_id"] = serde_json::json!("other-attempt"); }
        *mock.receipt.lock().unwrap() = Some(receipt.clone());
        if matches!(mock.mode, "lost" | "purge_lost") && first { return HttpResponse::InternalServerError().finish(); }
        HttpResponse::Ok().json(serde_json::json!({"maintenance":receipt,"context":context()}))
    }

    #[actix_web::test]
    async fn durable_handoff_recovers_without_new_ids_or_token_savings() {
        for mode in ["complete", "lost", "unsubmitted", "refused", "wrong_binding", "purge", "purge_lost"] {
            let dir = tempfile::TempDir::new().unwrap();
            let manager = Arc::new(atomic_core::DatabaseManager::new(dir.path()).unwrap());
            let db = Db(manager.active_core().await.unwrap(), manager.active_id().unwrap());
            let state = AppState { manager, event_tx:tokio::sync::broadcast::channel(32).0,
                public_url:None, log_buffer:crate::log_buffer::LogBuffer::new(4), memu_session:None,
                export_jobs:crate::export_jobs::ExportJobManager::for_tests(dir.path().join("exports")),
                setup_token:None, dangerously_skip_setup_token:false,
                setup_claim_lock:tokio::sync::Mutex::new(()), setup_claim_limiter:crate::state::SetupClaimLimiter::new() };
            let conv = db.0.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
            let id = conv.conversation.id;
            db.0.set_chat_phase(&id, "TestOwner", "TestSoul", &Backend::ClaudeCode, &Phase::Open).await.unwrap();
            let kind = if mode.starts_with("purge") { Kind::Purge } else { Kind::Ordinary };
            if kind == Kind::Purge {
                db.0.set_chat_phase(&id, "TestOwner", "TestSoul", &Backend::ClaudeCode, &Phase::Closing).await.unwrap();
                db.0.set_chat_phase(&id, "TestOwner", "TestSoul", &Backend::ClaudeCode, &Phase::Closed).await.unwrap();
            }
            let mock = web::Data::new(Mock { mode, db:Db(db.0.clone(), db.1.clone()), id:id.clone(),
                requests:Mutex::new(Vec::new()), receipt:Mutex::new(None) });
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let scope = MemuScope { base_url:format!("http://{}", listener.local_addr().unwrap()),
                user_id:"TestOwner".into(), soul_id:"TestSoul".into() };
            let factory = mock.clone();
            let server = HttpServer::new(move || App::new().app_data(factory.clone())
                .route("/integration/atomic/chat/status", web::get().to(observe))
                .route("/integration/atomic/chat/maintenance", web::post().to(handoff)))
                .workers(1).listen(listener).unwrap().run();
            let handle = server.handle();
            actix_web::rt::spawn(server);
            let bridge = ChatToolBridge::default();
            let work = lock_conversation(&db, &id, Some(&scope)).await.unwrap();
            let result = run(&state, &db, &scope, &bridge, &id, kind, &work).await;
            assert_eq!(result.is_ok(), matches!(mode, "complete" | "purge"));
            let before = read_chat(&db, &id).await.unwrap().conversation.atomic_chat_state.unwrap();
            if matches!(mode, "lost" | "unsubmitted" | "purge_lost") {
                assert_eq!(before.maintenance.as_ref().unwrap().status, Status::Pending);
                reconcile(&state, &db, &scope, &bridge, &id, &work).await.unwrap();
            } else if mode == "wrong_binding" {
                assert!(reconcile(&state, &db, &scope, &bridge, &id, &work).await.is_err());
            }
            let saved = read_chat(&db, &id).await.unwrap().conversation.atomic_chat_state.unwrap();
            let maintenance = saved.maintenance.as_ref().unwrap();
            assert_eq!(maintenance.attempt_id, before.maintenance.as_ref().unwrap().attempt_id);
            assert_eq!(saved.phase, if kind == Kind::Purge { Phase::Closed } else { Phase::Open });
            assert_eq!(maintenance.status, match mode {
                "refused" => Status::Failed, "wrong_binding" => Status::Pending, _ => Status::Complete });
            if maintenance.status == Status::Complete {
                assert_eq!(saved.native_context_tokens, Some(9000));
                assert_eq!(maintenance.awaiting_measurement, kind == Kind::Ordinary);
                assert_eq!(run(&state, &db, &scope, &bridge, &id, kind, &work).await.is_ok(), kind == Kind::Purge);
                let repeated = read_chat(&db, &id).await.unwrap().conversation.atomic_chat_state.unwrap();
                assert_eq!(repeated.maintenance.as_ref().unwrap().attempt_id, maintenance.attempt_id);
            }
            let requests = mock.requests.lock().unwrap();
            assert_eq!(requests.len(), if mode == "unsubmitted" { 2 } else { 1 });
            assert!(requests.iter().all(|request| request["attempt_id"] == maintenance.attempt_id));
            drop(requests);
            handle.stop(true).await;
        }
    }
}
