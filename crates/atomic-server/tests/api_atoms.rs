//! Integration tests for the Atom and Tag REST API endpoints.
//!
//! Each test spins up a real actix-web test server backed by a temporary SQLite
//! database and exercises the endpoints with actual HTTP requests.

use actix_web::{App, HttpRequest, HttpResponse, HttpServer, test as actix_test, web};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shared test context — holds the temp dir (so the DB stays alive),
/// the app state, and the raw auth token.
struct TestCtx {
    _temp: tempfile::TempDir,
    state: web::Data<atomic_server::state::AppState>,
    token: String,
}

impl TestCtx {
    async fn new() -> Self {
        Self::new_with_memu(None).await
    }

    async fn new_with_memu(memu_base_url: Option<String>) -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (_info, raw_token) = manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test")
            .await
            .unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let state = web::Data::new(atomic_server::state::AppState {
            manager,
            event_tx,
            public_url: None,
            log_buffer: atomic_server::log_buffer::LogBuffer::new(16),
            memu_session: memu_base_url
                .map(|base_url| atomic_server::state::MemuSessionConfig { base_url }),
            export_jobs: atomic_server::export_jobs::ExportJobManager::for_tests(
                temp.path().join("exports"),
            ),
            setup_token: None,
            dangerously_skip_setup_token: false,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: atomic_server::state::SetupClaimLimiter::new(),
        });
        if state.memu_session.is_some() {
            state
                .bind_workspace_database(
                    "TestOwner",
                    "TestSoul",
                    &state.manager.active_id().unwrap(),
                )
                .unwrap();
        }
        TestCtx {
            _temp: temp,
            state,
            token: raw_token,
        }
    }

    fn auth_header(&self) -> (&str, String) {
        ("Authorization", format!("Bearer {}", self.token))
    }
}

async fn atomic_session_start_ok(body: web::Json<Value>) -> HttpResponse {
    assert_eq!(body["user_id"], "TestOwner");
    assert_eq!(body["soul_id"], "TestSoul");
    assert!(
        body["conversation_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("chat:atomic-"))
    );
    HttpResponse::Ok().json(json!({"snapshot_text": "hidden memU snapshot"}))
}

async fn atomic_session_start_fail() -> HttpResponse {
    HttpResponse::InternalServerError().json(json!({"error": "memu unavailable"}))
}

async fn atomic_chat_profile_ok() -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "backend": "api",
        "settings": {
            "provider": "openai_compat",
            "openai_compat_base_url": "http://127.0.0.1:9",
            "openai_compat_api_key": "test-key",
            "openai_compat_llm_model": "mock-llm",
            "openai_compat_context_length": "200000"
        }
    }))
}

async fn memu_owner() -> HttpResponse {
    HttpResponse::Ok().json(json!({"user_id": "TestOwner"}))
}

async fn memu_souls() -> HttpResponse {
    HttpResponse::Ok().json(json!({"souls": ["TestSoul"]}))
}

fn start_memu_stub(handler: fn() -> actix_web::Route) -> (String, actix_web::dev::ServerHandle) {
    let server = HttpServer::new(move || {
        App::new()
            .route("/owner", web::get().to(memu_owner))
            .route("/souls", web::get().to(memu_souls))
            .route("/integration/atomic/session_start", handler())
            .route(
                "/integration/atomic/chat_profile",
                web::get().to(atomic_chat_profile_ok),
            )
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    (format!("http://{}", addr), handle)
}

async fn fake_chat_completion(body: web::Json<Value>) -> HttpResponse {
    assert_eq!(body["model"], "mock-llm");
    HttpResponse::Ok()
        .content_type("text/event-stream")
        .body("data: {\"choices\":[{\"delta\":{\"content\":\"fake answer\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n")
}

async fn fake_chat_completion_with_memu_write_tools(body: web::Json<Value>) -> HttpResponse {
    let tool_names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"search_atoms"));
    assert!(tool_names.contains(&"get_atom"));
    assert!(!tool_names.contains(&"create_atom"));
    assert!(tool_names.contains(&"edit_atom"));
    HttpResponse::Ok()
        .content_type("text/event-stream")
        .body("data: {\"choices\":[{\"delta\":{\"content\":\"fake answer\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n")
}

fn start_fake_model() -> (String, actix_web::dev::ServerHandle) {
    let server = HttpServer::new(move || {
        App::new()
            .route("/chat/completions", web::post().to(fake_chat_completion))
            .route("/v1/chat/completions", web::post().to(fake_chat_completion))
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    (format!("http://{}", addr), handle)
}

fn start_fake_model_with_memu_write_tools() -> (String, actix_web::dev::ServerHandle) {
    let server = HttpServer::new(move || {
        App::new()
            .route(
                "/chat/completions",
                web::post().to(fake_chat_completion_with_memu_write_tools),
            )
            .route(
                "/v1/chat/completions",
                web::post().to(fake_chat_completion_with_memu_write_tools),
            )
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    (format!("http://{}", addr), handle)
}

struct ToolModelState {
    request_count: AtomicUsize,
    tool_call_on: usize,
    tool_calls: Value,
    pause_on: Option<usize>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    requests: Mutex<Vec<Value>>,
    answer: Mutex<String>,
}

async fn fake_tool_completion(
    state: web::Data<ToolModelState>,
    body: web::Json<Value>,
) -> HttpResponse {
    state.requests.lock().unwrap().push(body.into_inner());
    let request_number = state.request_count.fetch_add(1, Ordering::SeqCst);
    if state.pause_on == Some(request_number) {
        state.started.notify_one();
        state.release.notified().await;
    }
    let delta = if request_number == state.tool_call_on {
        json!({"tool_calls": state.tool_calls})
    } else {
        json!({"content": state.answer.lock().unwrap().clone()})
    };
    HttpResponse::Ok()
        .content_type("text/event-stream")
        .body(format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": delta, "finish_reason": null}]})
        ))
}

fn start_tool_model(
    tool_call_on: usize,
    tool_calls: Value,
    pause_on: Option<usize>,
) -> (
    String,
    actix_web::dev::ServerHandle,
    web::Data<ToolModelState>,
) {
    let state = web::Data::new(ToolModelState {
        request_count: AtomicUsize::new(0),
        tool_call_on,
        tool_calls,
        pause_on,
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        requests: Mutex::new(Vec::new()),
        answer: Mutex::new("fake answer".into()),
    });
    let server = HttpServer::new({
        let state = state.clone();
        move || {
            App::new()
                .app_data(state.clone())
                .route("/chat/completions", web::post().to(fake_tool_completion))
                .route("/v1/chat/completions", web::post().to(fake_tool_completion))
        }
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    (format!("http://{}", addr), handle, state)
}

struct MemuCreateState {
    creations: Mutex<Vec<Value>>,
}

struct MemuEndState {
    hold: AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    published: Mutex<Vec<Value>>,
    fail_next: AtomicBool,
    capacity: AtomicUsize,
    cli: AtomicBool,
}

async fn memu_session_end(state: web::Data<MemuEndState>, body: web::Json<Value>) -> HttpResponse {
    state.started.notify_one();
    if state.hold.load(Ordering::SeqCst) {
        state.release.notified().await;
    }
    if state.fail_next.swap(false, Ordering::SeqCst) {
        return HttpResponse::InternalServerError().json(json!({"error":"Fictional publication failure"}));
    }
    state.published.lock().unwrap().push(body.into_inner());
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

async fn memu_memory_create(
    state: web::Data<MemuCreateState>,
    body: web::Json<Value>,
) -> HttpResponse {
    let body = body.into_inner();
    state.creations.lock().unwrap().push(body.clone());
    HttpResponse::Created().json(json!({
        "id": "memory:human-created",
        "label": "Human memory",
        "summary": body["text"],
        "memory_type": "knowledge",
        "created_at": "2026-07-04T00:00:00Z",
        "updated_at": "2026-07-04T00:00:00Z",
        "category_ids": [],
        "category_names": []
    }))
}

fn start_memu_create_stub() -> (
    String,
    actix_web::dev::ServerHandle,
    web::Data<MemuCreateState>,
) {
    let state = web::Data::new(MemuCreateState {
        creations: Mutex::new(Vec::new()),
    });
    let server = HttpServer::new({
        let state = state.clone();
        move || {
            App::new()
                .app_data(state.clone())
                .route("/owner", web::get().to(memu_owner))
                .route("/souls", web::get().to(memu_souls))
                .route(
                    "/integration/atomic/memories",
                    web::post().to(memu_memory_create),
                )
        }
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    (format!("http://{}", addr), handle, state)
}

fn start_memu_stub_with_model(
    model_url: String,
) -> (
    String,
    actix_web::dev::ServerHandle,
    web::Data<MemuEndState>,
) {
    let end_state = web::Data::new(MemuEndState {
        hold: AtomicBool::new(false),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        published: Mutex::new(Vec::new()),
        fail_next: AtomicBool::new(false),
        capacity: AtomicUsize::new(200_000),
        cli: AtomicBool::new(false),
    });
    let captured_end = end_state.clone();
    let server = HttpServer::new(move || {
        let model_url = model_url.clone();
        let profile_state = captured_end.clone();
        App::new()
            .app_data(captured_end.clone())
            .route("/owner", web::get().to(memu_owner))
            .route("/souls", web::get().to(memu_souls))
            .route(
                "/integration/atomic/session_start",
                web::post().to(atomic_session_start_ok),
            )
            .route(
                "/integration/atomic/session_end",
                web::post().to(memu_session_end),
            )
            .route(
                "/integration/atomic/chat_profile",
                web::get().to(move |request: HttpRequest| {
                    let model_url = model_url.clone();
                    let capacity = profile_state.capacity.load(Ordering::SeqCst);
                    let backend = if profile_state.cli.load(Ordering::SeqCst) { "claude_code" } else { "api" };
                    async move {
                        assert_eq!(request.query_string(), "user_id=TestOwner&soul_id=TestSoul");
                        if backend == "claude_code" {
                            return HttpResponse::Ok().json(json!({
                                "backend":"claude_code", "settings":{},
                                "cli":{"model":"fictional-cli", "context_window_tokens":capacity,
                                    "timeout_seconds":90, "effort":"medium", "fingerprint":"fictional-profile",
                                    "memu_response_sentences":3, "memu_log_prompts":false}
                            }));
                        }
                        HttpResponse::Ok().json(json!({
                            "backend": backend,
                            "settings": {
                                "provider": "openai_compat",
                                "openai_compat_base_url": model_url,
                                "openai_compat_api_key": "test-key",
                                "openai_compat_llm_model": "mock-llm",
                                "openai_compat_context_length": capacity.to_string()
                            }
                        }))
                    }
                }),
            )
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    (format!("http://{}", addr), handle, end_state)
}

async fn memu_atoms(req: HttpRequest) -> HttpResponse {
    let qs = req.query_string();
    assert!(qs.contains("user_id="));
    assert!(qs.contains("soul_id="));
    HttpResponse::Ok().json(json!({
        "atoms": [{
            "id": "memory:m1",
            "title": "Memory one",
            "snippet": "Memory summary",
            "source_url": null,
            "source": "episodic",
            "published_at": null,
            "created_at": "2026-07-04T00:00:00Z",
            "updated_at": "2026-07-04T00:00:00Z",
            "embedding_status": "complete",
            "tagging_status": "skipped",
            "embedding_error": null,
            "tagging_error": null,
            "tags": []
        }],
        "total_count": 1,
        "limit": 50,
        "offset": 0
    }))
}

async fn memu_tags(req: HttpRequest) -> HttpResponse {
    assert!(req.query_string().contains("user_id="));
    HttpResponse::Ok().json(json!([{
        "id": "category:c1",
        "name": "Core",
        "parent_id": null,
        "created_at": "2026-07-04T00:00:00Z",
        "is_autotag_target": false,
        "autotag_description": "",
        "atom_count": 1,
        "children_total": 0,
        "children": []
    }]))
}

async fn memu_memory(path: web::Path<String>) -> HttpResponse {
    assert_eq!(path.as_str(), "memory:m1");
    HttpResponse::Ok().json(json!({
        "id": "memory:m1",
        "kind": "memory",
        "label": "Memory one",
        "summary": "Memory summary",
        "memory_type": "episodic",
        "created_at": "2026-07-04T00:00:00Z",
        "updated_at": "2026-07-04T00:00:00Z",
        "category_ids": ["c1"],
        "category_names": ["Core"]
    }))
}

async fn memu_update_memory(path: web::Path<String>, body: web::Json<Value>) -> HttpResponse {
    assert_eq!(path.as_str(), "m1");
    assert_eq!(body["summary"], "Updated memory");
    assert_eq!(body["displayed_summary"], "Memory summary");
    HttpResponse::Ok().json(json!({
        "id": "memory:m1",
        "kind": "memory",
        "label": "Memory one",
        "summary": "Updated memory",
        "memory_type": "episodic",
        "created_at": "2026-07-04T00:00:00Z",
        "updated_at": "2026-07-05T00:00:00Z",
        "category_ids": ["c1"],
        "category_names": ["Core"]
    }))
}

async fn memu_update_category(path: web::Path<String>, body: web::Json<Value>) -> HttpResponse {
    assert_eq!(path.as_str(), "c1");
    assert_eq!(body["kind"], "topic");
    assert_eq!(body["displayed_title"], "Core");
    assert_eq!(body["displayed_description"], "A category brief");
    HttpResponse::Ok().json(json!({
        "id": "category:c1",
        "kind": "category",
        "category_kind": "topic",
        "label": "Core",
        "summary": "Category summary",
        "created_at": "2026-07-04T00:00:00Z",
        "updated_at": "2026-07-05T00:00:00Z",
        "category_ids": [],
        "category_names": []
    }))
}

async fn memu_approve_empty(body: web::Json<Value>) -> HttpResponse {
    assert_eq!(body["displayed_summary"], "Memory summary");
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

async fn memu_delete_memory_conflict(path: web::Path<String>, req: HttpRequest) -> HttpResponse {
    assert_eq!(path.as_str(), "m1");
    assert!(req.query_string().contains("displayed_summary=Memory+summary"));
    HttpResponse::Conflict().json(json!({
        "detail": {
            "message": "Memory is cited in dossier prose",
            "dossiers": [{"id": "category:c1", "name": "Core", "ref": "M1"}]
        }
    }))
}

async fn memu_search(req: HttpRequest) -> HttpResponse {
    let query = req.query_string();
    if query.contains("q=memory") {
        assert!(query.contains("mode=hybrid"));
    }
    if query.contains("q=Core") {
        assert!(query.contains("mode=keyword"));
    }
    HttpResponse::Ok().json(json!({
        "nodes": [{
            "id": "memory:m1",
            "kind": "memory",
            "label": "Memory one",
            "summary": "Memory summary",
            "memory_type": "episodic",
            "score": 0.9,
            "created_at": "2026-07-04T00:00:00Z",
            "updated_at": "2026-07-04T00:00:00Z",
            "category_ids": ["c1"],
            "category_names": ["Core"]
        }],
        "limit": 5,
        "count": 1
    }))
}

async fn memu_canvas_source(req: HttpRequest) -> HttpResponse {
    assert!(req.query_string().contains("user_id="));
    HttpResponse::Ok().json(json!({
        "atoms": [{
            "id": "memory:m1",
            "title": "Memory one",
            "embedding_f32_le_b64": "AACAPwAAAAA=",
            "primary_tag": "Core",
            "tag_count": 1,
            "tag_ids": ["category:c1"],
            "source_url": null
        }, {
            "id": "memory:m2",
            "title": "Memory two",
            "embedding_f32_le_b64": "ZmZmP83MzD0=",
            "primary_tag": "Core",
            "tag_count": 1,
            "tag_ids": ["category:c1"],
            "source_url": null
        }, {
            "id": "memory:m3",
            "title": "Memory three",
            "embedding_f32_le_b64": "zcxMPs3MTD8=",
            "primary_tag": "Core",
            "tag_count": 1,
            "tag_ids": ["category:c1"],
            "source_url": null
        }],
        "edges": [{
            "source": "memory:m1",
            "target": "memory:m2",
            "weight": 0.9,
            "kind": "similarity",
            "predicate": "similarity"
        }],
        "timing_ms": {"total": 1.25}
    }))
}

async fn memu_bad_canvas_source() -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "atoms": [{
            "id": "memory:bad",
            "title": "Bad memory",
            "embedding_f32_le_b64": "not base64"
        }],
        "edges": []
    }))
}

async fn memu_neighborhood(path: web::Path<String>, req: HttpRequest) -> HttpResponse {
    assert_eq!(path.as_str(), "memory:m1");
    assert!(req.query_string().contains("user_id="));
    HttpResponse::Ok().json(json!({
        "center_atom_id": "memory:m1",
        "nodes": [{
            "id": "memory:m1",
            "kind": "memory",
            "label": "Memory one",
            "summary": "Memory summary",
            "memory_type": "episodic",
            "created_at": "2026-07-04T00:00:00Z",
            "updated_at": "2026-07-04T00:00:00Z",
            "category_ids": ["c1"],
            "category_names": ["Core"],
            "depth": 0
        }],
        "edges": []
    }))
}

fn start_memu_memory_stub() -> (String, actix_web::dev::ServerHandle) {
    let server = HttpServer::new(move || {
        App::new()
            .route("/owner", web::get().to(memu_owner))
            .route("/souls", web::get().to(memu_souls))
            .route("/integration/atomic/atoms", web::get().to(memu_atoms))
            .route("/integration/atomic/tags", web::get().to(memu_tags))
            .route("/integration/atomic/search", web::get().to(memu_search))
            .route(
                "/integration/atomic/canvas-source",
                web::get().to(memu_canvas_source),
            )
            .route(
                "/integration/atomic/canvas-source",
                web::post().to(memu_bad_canvas_source),
            )
            .route(
                "/integration/atomic/neighborhood/{id}",
                web::get().to(memu_neighborhood),
            )
            .route("/memory/{id}", web::get().to(memu_memory))
            .route("/memory/{id}", web::patch().to(memu_update_memory))
            .route("/category/{id}", web::patch().to(memu_update_category))
            .route(
                "/memory/{id}",
                web::delete().to(memu_delete_memory_conflict),
            )
            .route("/memory/{id}/approve", web::post().to(memu_approve_empty))
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    (format!("http://{}", addr), handle)
}

/// Build an actix App that mirrors the real server's /api scope (auth + routes).
fn test_app(
    ctx: &TestCtx,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(ctx.state.clone())
        .route(
            "/api/exports/{id}/download",
            web::get().to(atomic_server::routes::exports::download_export),
        )
        .service(
            web::scope("/api")
                .wrap(atomic_server::auth::BearerAuth {
                    state: ctx.state.clone(),
                })
                .configure(atomic_server::routes::configure_routes),
        )
}

async fn local_atom_count(ctx: &TestCtx) -> i32 {
    ctx.state
        .manager
        .active_core()
        .await
        .unwrap()
        .list_atoms(
            &atomic_core::ListAtomsParams {
                tag_id: None,
                limit: 50,
                offset: 0,
                cursor: None,
                cursor_id: None,
                source_filter: atomic_core::SourceFilter::All,
                source_value: None,
                sort_by: atomic_core::SortField::Updated,
                sort_order: atomic_core::SortOrder::Desc,
            },
            &atomic_core::KindFilter::All,
        )
        .await
        .unwrap()
        .total_count
}

// ---------------------------------------------------------------------------
// Atom CRUD tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_integrated_human_create_uses_exact_memu_scope() {
    let (memu_url, memu_handle, memu_state) = start_memu_create_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"content": "  A human-authored memory.  "}))
        .to_request();
    let response = actix_test::call_service(&app, req).await;
    assert_eq!(response.status(), 201);
    let atom: Value = actix_test::read_body_json(response).await;
    assert_eq!(atom["id"], "memory:human-created");
    assert_eq!(atom["content"], "  A human-authored memory.  ");
    assert_eq!(
        memu_state.creations.lock().unwrap().as_slice(),
        &[json!({
            "text": "  A human-authored memory.  ",
            "user_id": "TestOwner",
            "soul_id": "TestSoul",
        })]
    );
    assert_eq!(local_atom_count(&ctx).await, 0);

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({"content": "  "}))
        .to_request();
    assert_eq!(actix_test::call_service(&app, req).await.status(), 400);
    assert_eq!(memu_state.creations.lock().unwrap().len(), 1);

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_standalone_conversation_does_not_require_memu_config() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
}

#[actix_web::test]
async fn test_create_conversation_stores_hidden_memu_snapshot() {
    let (memu_url, memu_handle) = start_memu_stub(|| web::post().to(atomic_session_start_ok));
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let created: Value = actix_test::read_body_json(resp).await;
    let conversation_id = created["id"].as_str().unwrap();
    assert_eq!(created["message_count"], 0);

    let core = ctx.state.manager.active_core().await.unwrap();
    let raw = core
        .get_conversation(conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raw.messages.len(), 1);
    assert_eq!(raw.messages[0].message.role, "system");
    assert_eq!(raw.messages[0].message.content, "hidden memU snapshot");

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/conversations/{conversation_id}"))
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let visible: Value = actix_test::read_body_json(resp).await;
    assert_eq!(visible["messages"].as_array().unwrap().len(), 0);

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_create_conversation_cleans_up_after_memu_failure() {
    let (memu_url, memu_handle) = start_memu_stub(|| web::post().to(atomic_session_start_fail));
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 502);

    let req = actix_test::TestRequest::get()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let conversations: Value = actix_test::read_body_json(resp).await;
    assert_eq!(conversations.as_array().unwrap().len(), 0);

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_memu_session_satisfies_provider_verify() {
    let (memu_url, memu_handle) = start_memu_stub(|| web::post().to(atomic_session_start_ok));
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/provider/verify")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp: Value = actix_test::call_and_read_body_json(&app, req).await;

    assert_eq!(resp["configured"], true);
    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_standalone_provider_verify_still_reads_database_settings() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let request = actix_test::TestRequest::get()
        .uri("/api/provider/verify")
        .insert_header(ctx.auth_header())
        .to_request();
    let response = actix_test::call_service(&app, request).await;
    assert_eq!(response.status(), 200);
    let body: Value = actix_test::read_body_json(response).await;
    assert_eq!(body["configured"], false);
}

#[actix_web::test]
async fn test_memu_read_routes_proxy() {
    let (memu_url, memu_handle) = start_memu_memory_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let atoms: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(atoms["atoms"][0]["id"], "memory:m1");

    let req = actix_test::TestRequest::get()
        .uri("/api/tags?min_count=0")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let tags: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(tags[0]["id"], "category:c1");

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms/memory:m1")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let atom: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(atom["content"], "Memory summary");
    assert_eq!(atom["tags"][0]["id"], "category:c1");

    let req = actix_test::TestRequest::post()
        .uri("/api/search")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"query": "memory", "mode": "hybrid"}))
        .to_request();
    let search: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(search[0]["id"], "memory:m1");

    let req = actix_test::TestRequest::post()
        .uri("/api/search/global")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"query": "Core", "section_limit": 5}))
        .to_request();
    let search: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(search["atoms"][0]["id"], "memory:m1");
    assert_eq!(search["tags"][0]["id"], "category:c1");

    let req = actix_test::TestRequest::get()
        .uri("/api/canvas/global")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let response = actix_test::call_service(&app, req).await;
    let server_timing = response
        .headers()
        .get("server-timing")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(server_timing.contains("memu_total;dur=1.25"));
    assert!(server_timing.contains("atomic_fetch_json;dur="));
    assert!(server_timing.contains("atomic_project_build;dur="));
    let canvas: Value = actix_test::read_body_json(response).await;
    assert_eq!(canvas["atoms"][0]["atom_id"], "memory:m1");
    assert_eq!(canvas["atoms"][0]["tag_ids"][0], "category:c1");
    assert_eq!(canvas["edges"][0]["source"], "memory:m1");
    assert_eq!(canvas["edges"][0]["target"], "memory:m2");
    let expected_positions = atomic_core::projection::compute_2d_projection(&[
        ("memory:m1".to_string(), vec![1.0, 0.0]),
        ("memory:m2".to_string(), vec![0.9, 0.1]),
        ("memory:m3".to_string(), vec![0.2, 0.8]),
    ]);
    for (id, expected_x, expected_y) in expected_positions {
        let atom = canvas["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|atom| atom["atom_id"] == id)
            .unwrap();
        assert_eq!(atom["x"].as_f64().unwrap(), expected_x);
        assert_eq!(atom["y"].as_f64().unwrap(), expected_y);
    }

    let req = actix_test::TestRequest::post()
        .uri("/api/canvas/rebuild")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"atom_ids": ["memory:bad"]}))
        .to_request();
    let response = actix_test::call_service(&app, req).await;
    assert_eq!(response.status(), 502);
    let error: Value = actix_test::read_body_json(response).await;
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("memU canvas source shape failed")
    );

    let req = actix_test::TestRequest::get()
        .uri("/api/graph/neighborhood/memory:m1")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let neighborhood: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(neighborhood["center_atom_id"], "memory:m1");
    assert_eq!(neighborhood["atoms"][0]["depth"], 0);

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_openalma_rejects_another_workspace_database() {
    let (memu_url, memu_handle) = start_memu_memory_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let foreign = ctx
        .state
        .manager
        .create_database("Foreign Soul")
        .await
        .unwrap();
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms?db={}", foreign.id))
        .insert_header(ctx.auth_header())
        .insert_header(("X-Atomic-Source", "workspace"))
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let response = actix_test::call_service(&app, req).await;

    assert_eq!(response.status(), 400);
    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_cited_memory_delete_preserves_conflict_response() {
    let (memu_url, memu_handle) = start_memu_memory_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::delete()
        .uri("/api/memu/reviews/memory/m1?displayed_summary=Memory%20summary")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .to_request();
    let response = actix_test::call_service(&app, req).await;
    assert_eq!(response.status(), 409);
    let body: Value = actix_test::read_body_json(response).await;
    assert_eq!(body["error"]["dossiers"][0]["ref"], "M1");

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_memu_review_save_broadcasts_atom_updated() {
    let (memu_url, memu_handle) = start_memu_memory_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mut events = ctx.state.event_tx.subscribe();

    let req = actix_test::TestRequest::patch()
        .uri("/api/memu/reviews/memory/m1")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"summary": "Updated memory", "displayed_summary": "Memory summary"}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    match tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        atomic_server::state::ServerEvent::AtomUpdated { atom, .. } => {
            assert_eq!(atom.atom.id, "memory:m1");
            assert_eq!(atom.atom.content, "Updated memory");
        }
        other => panic!("unexpected event: {other:?}"),
    }

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_memu_category_kind_only_update_is_forwarded() {
    let (memu_url, memu_handle) = start_memu_memory_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::patch()
        .uri("/api/memu/reviews/category/c1")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({
            "kind": "topic",
            "displayed_summary": "Category summary",
            "displayed_title": "Core",
            "displayed_description": "A category brief",
            "summaries_revision": 4
        }))
        .to_request();
    let response = actix_test::call_service(&app, req).await;
    assert_eq!(response.status(), 200);
    let body: Value = actix_test::read_body_json(response).await;
    assert_eq!(body["category_kind"], "topic");

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_empty_memu_review_response_does_not_broadcast_atom_updated() {
    let (memu_url, memu_handle) = start_memu_memory_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mut events = ctx.state.event_tx.subscribe();

    let req = actix_test::TestRequest::post()
        .uri("/api/memu/reviews/memory/m1/approve")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"displayed_summary": "Memory summary"}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    assert!(events.try_recv().is_err());

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_category_approval_requires_summary_snapshot() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/memu/reviews/category/c1/approve")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({}))
        .to_request();
    let response = actix_test::call_service(&app, req).await;

    assert_eq!(response.status(), 400);
}

#[actix_web::test]
async fn test_paused_chat_preserves_structured_error_without_saving_message() {
    let server = HttpServer::new(|| {
        App::new()
            .route("/owner", web::get().to(memu_owner))
            .route("/souls", web::get().to(memu_souls))
            .route("/integration/atomic/session_start", web::post().to(atomic_session_start_ok))
            .route("/integration/atomic/chat_profile", web::get().to(|request: HttpRequest| async move {
                assert_eq!(request.query_string(), "user_id=TestOwner&soul_id=TestSoul");
                HttpResponse::Conflict().json(json!({"detail": {
                    "code": "soul_paused", "message": "TestSoul is paused. Retry in OpenAlma launcher.", "reason": "Failed"
                }}))
            }))
    }).bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}", server.addrs()[0]);
    let server = server.run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let ctx = TestCtx::new_with_memu(Some(url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let core = ctx.state.manager.active_core().await.unwrap();
    let conversation = core.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
    let id = &conversation.conversation.id;
    core.save_message(id, "system", "Fictional existing Soul Presence").await.unwrap();
    let request = actix_test::TestRequest::post()
        .uri(&format!("/api/conversations/{id}/messages"))
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"content": "Draft"}))
        .to_request();
    let response = actix_test::call_service(&app, request).await;
    assert_eq!(response.status(), 409);
    let error: Value = actix_test::read_body_json(response).await;
    assert_eq!(error["error"]["code"], "soul_paused");
    let core = ctx.state.manager.active_core().await.unwrap();
    let saved = core.get_conversation(id).await.unwrap().unwrap();
    assert!(
        saved
            .messages
            .iter()
            .all(|row| row.message.role == "system")
    );
    handle.stop(true).await;
}

#[actix_web::test]
async fn test_send_message_uses_memu_chat_profile() {
    let (model_url, model_handle, model) = start_tool_model(usize::MAX, json!([]), None);
    let (memu_url, memu_handle, end) = start_memu_stub_with_model(model_url);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let created: Value = actix_test::read_body_json(resp).await;
    let conversation_id = created["id"].as_str().unwrap();

    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/conversations/{conversation_id}/messages"))
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"content": "This Atomic session is ending. Write a recap of your activity is wording I want to discuss."}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["content"], "fake answer");

    let core = ctx.state.manager.active_core().await.unwrap();
    let raw = core
        .get_conversation(conversation_id)
        .await
        .unwrap()
        .unwrap();
    let roles: Vec<&str> = raw
        .messages
        .iter()
        .map(|m| m.message.role.as_str())
        .collect();
    assert_eq!(roles, vec!["system", "user", "assistant"]);
    let saved_state = raw.conversation.atomic_chat_state.as_ref().unwrap();
    assert_eq!(saved_state.phase, atomic_core::AtomicChatPhase::Open);
    assert!(saved_state.active.is_none());
    assert_eq!(saved_state.context_window_tokens, Some(200_000));
    let user_id = raw.messages[1].message.id.clone();

    let send_again = || actix_test::TestRequest::post()
        .uri(&format!("/api/conversations/{conversation_id}/messages"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"content":"Preserved refused draft"})).to_request();
    for (capacity, cli) in [(20_100, false), (0, false), (200_000, true)] {
        end.capacity.store(capacity, Ordering::SeqCst);
        end.cli.store(cli, Ordering::SeqCst);
        assert_eq!(actix_test::call_service(&app, send_again()).await.status(), 409);
        let retained = core.get_conversation(conversation_id).await.unwrap().unwrap();
        assert_eq!(retained.messages.len(), 3);
        assert_eq!(retained.conversation.atomic_chat_state.unwrap().phase, atomic_core::AtomicChatPhase::Open);
    }
    end.capacity.store(200_000, Ordering::SeqCst);
    end.cli.store(false, Ordering::SeqCst);
    let retry = actix_test::TestRequest::post().uri(&format!("/api/conversations/{conversation_id}/retry"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"user_message_id":user_id})).to_request();
    assert_eq!(actix_test::call_service(&app, retry).await.status(), 200);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 1, "completed Retry must not call the model");
    let get = actix_test::TestRequest::get().uri(&format!("/api/conversations/{conversation_id}"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .to_request();
    let response = actix_test::call_service(&app, get).await;
    let view: Value = actix_test::read_body_json(response).await;
    assert_eq!(view["context_meter"]["limit_tokens"], 180_000);
    assert!(view["context_meter"]["estimated_tokens"].as_u64().unwrap() > 0);

    let end_request = || actix_test::TestRequest::post().uri("/api/memu/session/end")
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"conversation_id":conversation_id})).to_request();
    *model.answer.lock().unwrap() = " \n".into();
    assert_eq!(actix_test::call_service(&app, end_request()).await.status(), 500);
    let blank = core.get_conversation(conversation_id).await.unwrap().unwrap();
    assert_eq!(blank.messages.len(), 4, "blank recap must not be published as a saved assistant reply");
    assert!(matches!(blank.conversation.atomic_chat_state.unwrap().recap.unwrap().outcome,
        atomic_core::AtomicChatRecapOutcome::Failed { .. }));
    *model.answer.lock().unwrap() = "fake answer".into();
    end.fail_next.store(true, Ordering::SeqCst);
    assert_eq!(actix_test::call_service(&app, end_request()).await.status(), 500);
    let closing = core.get_conversation(conversation_id).await.unwrap().unwrap();
    assert_eq!(closing.conversation.atomic_chat_state.unwrap().phase, atomic_core::AtomicChatPhase::Closing);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 3);
    assert_eq!(actix_test::call_service(&app, end_request()).await.status(), 200);
    assert_eq!(actix_test::call_service(&app, end_request()).await.status(), 200);
    let closed = core.get_conversation(conversation_id).await.unwrap().unwrap();
    assert_eq!(closed.messages.len(), 5);
    assert_eq!(closed.conversation.atomic_chat_state.unwrap().phase, atomic_core::AtomicChatPhase::Closed);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 3, "publication Retry must reuse the saved recap");
    assert!(model.requests.lock().unwrap().last().unwrap().get("tools").is_none());
    assert_eq!(end.published.lock().unwrap().len(), 1);
    assert_eq!(end.published.lock().unwrap()[0]["transcript"].as_array().unwrap().len(), 2);
    assert!(end.published.lock().unwrap()[0]["transcript"][0]["content"].as_str().unwrap().contains("wording I want to discuss"));
    end.cli.store(true, Ordering::SeqCst);
    let get = actix_test::TestRequest::get().uri(&format!("/api/conversations/{conversation_id}"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .to_request();
    let view: Value = actix_test::read_body_json(actix_test::call_service(&app, get).await).await;
    assert!(view.get("context_meter").is_none());
    assert!(view.get("context_error").is_none(), "Closed must not consult the current model profile");
    assert_eq!(actix_test::call_service(&app, end_request()).await.status(), 200);

    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_finished_legacy_end_republishes_without_consulting_a_model_profile() {
    let (model_url, model_handle, model) = start_tool_model(usize::MAX, json!([]), None);
    let (memu_url, memu_handle, end) = start_memu_stub_with_model(model_url);
    end.cli.store(true, Ordering::SeqCst);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let core = ctx.state.manager.active_core().await.unwrap();
    let conv = core.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
    let id = &conv.conversation.id;
    for (role, content) in [("system", "Fictional original Presence"), ("user", "Older question"),
        ("assistant", "Older answer"), ("user", "This Atomic session is ending. Write a recap of your activity\nFull Atomic transcript:\nOlder dialogue"),
        ("assistant", "Older recap text")] {
        core.save_message(id, role, content).await.unwrap();
    }
    let request = actix_test::TestRequest::post().uri("/api/memu/session/end")
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"conversation_id":id})).to_request();
    assert_eq!(actix_test::call_service(&app, request).await.status(), 200);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 0);
    assert_eq!(end.published.lock().unwrap()[0]["activity_recap"], "Older recap text");
    assert!(core.get_conversation(id).await.unwrap().unwrap().conversation.atomic_chat_state.is_none());
    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_capacity_refusal_distinguishes_oversized_draft_from_full_sitting() {
    let (model_url, model_handle, model) = start_tool_model(usize::MAX, json!([]), None);
    let (memu_url, memu_handle, end) = start_memu_stub_with_model(model_url.clone());
    end.capacity.store(20_100, Ordering::SeqCst);
    let ctx = TestCtx::new_with_memu(Some(memu_url.clone())).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let core = ctx.state.manager.active_core().await.unwrap();
    let conversation = core.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
    let id = conversation.conversation.id;
    core.save_message(&id, "system", "Fictional original Presence").await.unwrap();
    let request = actix_test::TestRequest::post().uri(&format!("/api/conversations/{id}/messages"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"content":"oversized fictional draft ".repeat(1000)})).to_request();
    assert_eq!(actix_test::call_service(&app, request).await.status(), 409);
    tokio::time::timeout(Duration::from_secs(10), atomic_core::executor::wait_idle()).await.unwrap();
    let saved = core.get_conversation(&id).await.unwrap().unwrap();
    assert_eq!(saved.messages.len(), 1);
    assert!(saved.conversation.atomic_chat_state.is_none(), "an oversized draft must not close an otherwise open sitting");
    assert_eq!(model.request_count.load(Ordering::SeqCst), 0);
    assert_eq!(end.published.lock().unwrap().len(), 0);
    core.set_chat_phase(&id, "TestOwner", "TestSoul", &atomic_core::AtomicChatBackend::Api,
        &atomic_core::AtomicChatPhase::Open).await.unwrap();
    core.save_message(&id, "user", &"old evidence ".repeat(2000)).await.unwrap();
    core.save_message(&id, "assistant", "Older answer").await.unwrap();
    let settings = [("provider", "openai_compat"), ("openai_compat_base_url", model_url.as_str()),
        ("openai_compat_api_key", "test-key"), ("openai_compat_llm_model", "mock-llm"),
        ("openai_compat_context_length", "200000")].into_iter().map(|(k,v)| (k.to_string(),v.to_string())).collect();
    let small = atomic_core::AtomicChatRequest::New { content:"Short retained draft".into(), page_context:None, canvas_context:None };
    let fresh = core.estimate_fresh_chat_request_tokens(&id, &small, settings,
        atomic_core::MemuToolConfig { base_url:memu_url, user_id:"TestOwner".into(), soul_id:"TestSoul".into() }).await.unwrap();
    end.capacity.store(fresh + 20_000 + 50, Ordering::SeqCst);
    let request = actix_test::TestRequest::post().uri(&format!("/api/conversations/{id}/messages"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"content":"Short retained draft"})).to_request();
    assert_eq!(actix_test::call_service(&app, request).await.status(), 409);
    tokio::time::timeout(Duration::from_secs(10), atomic_core::executor::wait_idle()).await.unwrap();
    let closed = core.get_conversation(&id).await.unwrap().unwrap();
    assert_eq!(closed.conversation.atomic_chat_state.unwrap().phase, atomic_core::AtomicChatPhase::Closed);
    assert!(!closed.messages.iter().any(|row| row.message.content == "Short retained draft"));
    assert_eq!(model.request_count.load(Ordering::SeqCst), 1, "only the old sitting's recap should run");
    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_oversized_legacy_api_chat_refuses_send_and_end_without_closeout() {
    let (model_url, model_handle, model) = start_tool_model(usize::MAX, json!([]), None);
    let (memu_url, memu_handle, end) = start_memu_stub_with_model(model_url);
    end.capacity.store(25_000, Ordering::SeqCst);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let core = ctx.state.manager.active_core().await.unwrap();
    let conv = core.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
    let id = &conv.conversation.id;
    core.save_message(id, "system", "Fictional original Presence").await.unwrap();
    core.save_message(id, "user", &"old evidence ".repeat(30_000)).await.unwrap();
    core.save_message(id, "assistant", "Older answer").await.unwrap();
    for (uri, body) in [
        (format!("/api/conversations/{id}/messages"), json!({"content":"New unsaved draft"})),
        ("/api/memu/session/end".into(), json!({"conversation_id":id})),
    ] {
        let request = actix_test::TestRequest::post().uri(&uri)
            .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
            .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(body).to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), 409);
        let error: Value = actix_test::read_body_json(response).await;
        assert!(error["error"].as_str().unwrap().contains("help@openalma.org"));
    }
    tokio::time::timeout(Duration::from_secs(10), atomic_core::executor::wait_idle()).await.unwrap();
    let saved = core.get_conversation(id).await.unwrap().unwrap();
    assert!(saved.conversation.atomic_chat_state.is_none());
    assert_eq!(saved.messages.len(), 3);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 0);
    assert!(end.published.lock().unwrap().is_empty());
    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_retry_reconciles_stranded_and_legacy_turns_without_replaying_unknown_work() {
    use atomic_core::{AtomicChatBackend as Backend, AtomicChatRequest as Request};
    let (model_url, model_handle, model) = start_tool_model(usize::MAX, json!([]), None);
    let (memu_url, memu_handle, _) = start_memu_stub_with_model(model_url);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let core = ctx.state.manager.active_core().await.unwrap();
    for (bound, interrupted_tool) in [(true, false), (false, false), (true, true), (false, true)] {
        let conv = core.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
        let id = &conv.conversation.id;
        core.save_message(id, "system", "Fictional original Presence").await.unwrap();
        let user = if bound {
            core.admit_chat_turn(id, "TestOwner", "TestSoul", &Backend::Api,
                &Request::New { content: "Fictional stranded turn".into(), page_context: None, canvas_context: None }, 200_000).await.unwrap()
        } else { core.save_message(id, "user", "Fictional unanswered legacy turn").await.unwrap() };
        if interrupted_tool {
            let saved = core.get_conversation(id).await.unwrap().unwrap();
            let mut records = atomic_core::agent::ChatToolRecords::from_saved_message(saved.messages.last().unwrap());
            records.tool_calls.push(atomic_core::ChatToolCall { id: format!("{id}-tool"), message_id: user.id.clone(),
                tool_name: "edit_atom".into(), tool_input: json!({"atom_id":"memory:fictional"}),
                tool_output: None, status: "running".into(), created_at: String::new(), completed_at: None });
            core.save_chat_tool_records(id, &user.id, &records).await.unwrap();
        }
        let before = model.request_count.load(Ordering::SeqCst);
        let request = actix_test::TestRequest::post().uri(&format!("/api/conversations/{id}/retry"))
            .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
            .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"user_message_id":user.id})).to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), if interrupted_tool { 409 } else { 200 });
        let saved = core.get_conversation(id).await.unwrap().unwrap();
        assert_eq!(saved.messages.iter().filter(|row| row.message.role == "user").count(), 1);
        if interrupted_tool {
            assert_eq!(model.request_count.load(Ordering::SeqCst), before);
            let tool = &saved.messages[1].tool_calls[0];
            assert_eq!(tool.status, "failed");
            assert_eq!(tool.tool_output.as_ref().unwrap()["outcome_unknown"], true);
            let end = actix_test::TestRequest::post().uri("/api/memu/session/end")
                .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
                .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"conversation_id":id})).to_request();
            assert_eq!(actix_test::call_service(&app, end).await.status(), 200);
            assert_eq!(model.request_count.load(Ordering::SeqCst), before, "no interchange means no invented recap call");
        } else {
            assert_eq!(model.request_count.load(Ordering::SeqCst), before + 1);
            assert!(saved.conversation.atomic_chat_state.unwrap().active.is_none());
        }
    }
    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_retry_does_not_return_a_recap_for_a_failed_ordinary_turn() {
    use atomic_core::{AtomicChatBackend as Backend, AtomicChatRequest as Request};
    let (model_url, model_handle, model) = start_tool_model(usize::MAX, json!([]), None);
    let (memu_url, memu_handle, _) = start_memu_stub_with_model(model_url);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let core = ctx.state.manager.active_core().await.unwrap();
    let conv = core.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
    let id = &conv.conversation.id;
    core.save_message(id, "system", "Fictional original Presence").await.unwrap();
    core.save_message(id, "user", "Older question").await.unwrap();
    core.save_message(id, "assistant", "Older answer").await.unwrap();
    let user = core.admit_chat_turn(id, "TestOwner", "TestSoul", &Backend::Api,
        &Request::New { content:"Fictional failed ordinary request".into(), page_context:None, canvas_context:None }, 200_000).await.unwrap();
    core.fail_chat_turn(id, &user.id, "Fictional ordinary failure", false).await.unwrap();
    let end = actix_test::TestRequest::post().uri("/api/memu/session/end")
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"conversation_id":id})).to_request();
    assert_eq!(actix_test::call_service(&app, end).await.status(), 200);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 1);
    let retry = actix_test::TestRequest::post().uri(&format!("/api/conversations/{id}/retry"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"user_message_id":user.id})).to_request();
    assert_eq!(actix_test::call_service(&app, retry).await.status(), 409);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 1);
    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_automatic_close_out_revalidates_profile_after_the_admitted_reply_finishes() {
    use atomic_core::AtomicChatRequest as Request;
    let (model_url, model_handle, model) = start_tool_model(usize::MAX, json!([]), Some(0));
    *model.answer.lock().unwrap() = "Fictional reply content ".repeat(150);
    let (memu_url, memu_handle, end) = start_memu_stub_with_model(model_url.clone());
    let ctx = TestCtx::new_with_memu(Some(memu_url.clone())).await;
    let app = actix_test::init_service(test_app(&ctx)).await;
    let core = ctx.state.manager.active_core().await.unwrap();
    let conv = core.create_conversation(&[], None, Some(("TestOwner", "TestSoul"))).await.unwrap();
    let id = &conv.conversation.id;
    core.save_message(id, "system", "Fictional original Presence").await.unwrap();
    let settings = [("provider", "openai_compat"), ("openai_compat_base_url", model_url.as_str()),
        ("openai_compat_api_key", "test-key"), ("openai_compat_llm_model", "mock-llm"),
        ("openai_compat_context_length", "200000")].into_iter().map(|(k,v)| (k.to_string(),v.to_string())).collect();
    let input = Request::New { content:"Fictional near-limit input".into(), page_context:None, canvas_context:None };
    let projected = core.estimate_chat_request_tokens(id, &input, settings,
        atomic_core::MemuToolConfig { base_url:memu_url, user_id:"TestOwner".into(), soul_id:"TestSoul".into() }).await.unwrap();
    end.capacity.store(projected + 20_000 + 50, Ordering::SeqCst);
    let send = actix_test::TestRequest::post().uri(&format!("/api/conversations/{id}/messages"))
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"content":"Fictional near-limit input"})).to_request();
    let pending = actix_test::call_service(&app, send);
    tokio::pin!(pending);
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! { _ = model.started.notified() => {}, _ = &mut pending => panic!("reply did not reach the gated model") }
    }).await.unwrap();
    end.cli.store(true, Ordering::SeqCst);
    model.release.notify_one();
    assert_eq!(pending.await.status(), 200);
    tokio::time::timeout(Duration::from_secs(10), atomic_core::executor::wait_idle()).await.unwrap();
    let saved = core.get_conversation(id).await.unwrap().unwrap();
    let state = saved.conversation.atomic_chat_state.unwrap();
    assert_eq!(state.phase, atomic_core::AtomicChatPhase::Closing);
    assert!(state.recap.is_none());
    assert_eq!(model.request_count.load(Ordering::SeqCst), 1, "backend flip must not start a stale-config API recap");
    end.cli.store(false, Ordering::SeqCst);
    let end_request = actix_test::TestRequest::post().uri("/api/memu/session/end")
        .insert_header(ctx.auth_header()).insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul")) .set_json(json!({"conversation_id":id})).to_request();
    assert_eq!(actix_test::call_service(&app, end_request).await.status(), 200);
    assert_eq!(model.request_count.load(Ordering::SeqCst), 2);
    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_memu_backed_chat_offers_edit_but_not_create() {
    let (model_url, model_handle) = start_fake_model_with_memu_write_tools();
    let (memu_url, memu_handle, _) = start_memu_stub_with_model(model_url);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let created: Value = actix_test::read_body_json(resp).await;
    let conversation_id = created["id"].as_str().unwrap();

    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/conversations/{conversation_id}/messages"))
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"content": "hello"}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let req = actix_test::TestRequest::post()
        .uri("/api/memu/session/end")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"conversation_id": conversation_id}))
        .to_request();
    assert_eq!(actix_test::call_service(&app, req).await.status(), 200);

    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_integrated_chat_and_end_finish_after_waiting_handlers_are_dropped() {
    for drop_end_while_waiting in [false, true] {
        let (model_url, model_handle, model) = start_tool_model(
            0,
            json!([{
                "index": 0, "id": "page-call", "type": "function",
                "function": {"name": "get_current_page_context", "arguments": "{}"}
            }]),
            Some(1),
        );
        let (memu_url, memu_handle, end) = start_memu_stub_with_model(model_url);
        end.hold.store(true, Ordering::SeqCst);
        let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
        let app = actix_test::init_service(test_app(&ctx)).await;
        let core = ctx.state.manager.active_core().await.unwrap();
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let id = &conversation.conversation.id;
        core.save_message(id, "system", "Fictional Soul Presence")
            .await
            .unwrap();
        let req = actix_test::TestRequest::post()
            .uri(&format!("/api/conversations/{id}/messages"))
            .insert_header(ctx.auth_header())
            .insert_header(("X-OpenAlma-User", "TestOwner"))
            .insert_header(("X-OpenAlma-Soul", "TestSoul"))
            .set_json(json!({"content": "Inspect this page", "page_context": {"view": "memories"}}))
            .to_request();
        {
            let pending = actix_test::call_service(&app, req);
            tokio::pin!(pending);
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::select! {
                    _ = model.started.notified() => {},
                    _ = &mut pending => panic!("reply returned before the gated model result"),
                }
            })
            .await
            .unwrap();
        } // Simulate cancellation of the HTTP handler after the tool has completed.
        let req = actix_test::TestRequest::post()
            .uri("/api/memu/session/end")
            .insert_header(ctx.auth_header())
            .insert_header(("X-OpenAlma-User", "TestOwner"))
            .insert_header(("X-OpenAlma-Soul", "TestSoul"))
            .set_json(json!({"conversation_id": id}))
            .to_request();
        {
            let pending = actix_test::call_service(&app, req);
            tokio::pin!(pending);
            assert!(
                // Allow the two scope-validation HTTP reads to finish before cancellation.
                tokio::time::timeout(Duration::from_millis(250), &mut pending)
                    .await
                    .is_err()
            );
            assert_eq!(
                model.request_count.load(Ordering::SeqCst),
                2,
                "End must wait for the detached reply's lock"
            );
            if !drop_end_while_waiting {
                model.release.notify_one();
                tokio::time::timeout(Duration::from_secs(10), async {
                    tokio::select! {
                        _ = end.started.notified() => {},
                        _ = &mut pending => panic!("End returned before publication finished"),
                    }
                })
                .await
                .unwrap();
            }
        } // Drop End either while queued or during publication.
        if drop_end_while_waiting {
            model.release.notify_one();
            tokio::time::timeout(Duration::from_secs(10), end.started.notified())
                .await
                .unwrap();
        }
        end.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), atomic_core::executor::wait_idle())
            .await
            .unwrap();
        let saved = core.get_conversation(id).await.unwrap().unwrap();
        assert_eq!(
            saved
                .messages
                .iter()
                .map(|m| m.message.role.as_str())
                .collect::<Vec<_>>(),
            ["system", "user", "assistant", "user", "assistant"]
        );
        assert_eq!(saved.messages[2].tool_calls.len(), 1);
        assert_eq!(
            saved.messages[2].tool_calls[0].tool_name,
            "get_current_page_context"
        );
        assert_eq!(saved.messages[2].tool_calls[0].status, "complete");
        assert_eq!(saved.conversation.atomic_chat_state.as_ref().unwrap().phase, atomic_core::AtomicChatPhase::Closed);
        assert!(model.requests.lock().unwrap().last().unwrap().get("tools").is_none(), "recap must not expose tools");
        let publications = end.published.lock().unwrap().clone();
        assert_eq!(publications.len(), 1);
        assert_eq!(publications[0]["activity_recap"], "fake answer");
        assert_eq!(publications[0]["transcript"].as_array().unwrap().len(), 2);
        assert_eq!(model.request_count.load(Ordering::SeqCst), 3);
        memu_handle.stop(true).await;
        model_handle.stop(true).await;
    }
}

#[actix_web::test]
async fn test_memu_chat_returns_tool_result_without_executing_unavailable_create() {
    let tool_calls = json!([{
        "index": 0,
        "id": "call-create",
        "type": "function",
        "function": {
            "name": "create_atom",
            "arguments": "{\"content\":\"# Must not exist\"}"
        }
    }]);
    let (model_url, model_handle, _) = start_tool_model(0, tool_calls, None);
    let (memu_url, memu_handle, _) = start_memu_stub_with_model(model_url);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let created: Value = actix_test::call_and_read_body_json(&app, req).await;
    let conversation_id = created["id"].as_str().unwrap();

    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/conversations/{conversation_id}/messages"))
        .insert_header(ctx.auth_header())
        .insert_header(("X-OpenAlma-User", "TestOwner"))
        .insert_header(("X-OpenAlma-Soul", "TestSoul"))
        .set_json(json!({"content": "create a note"}))
        .to_request();
    let response: Value = actix_test::call_and_read_body_json(&app, req).await;

    assert_eq!(local_atom_count(&ctx).await, 0);
    assert_eq!(response["tool_calls"][0]["tool_name"], "create_atom");
    assert!(
        response["tool_calls"][0]["tool_output"]
            .as_str()
            .unwrap()
            .contains("Tool unavailable in this session")
    );
    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_create_and_get_atom() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "# Hello\n\nThis is a test atom.",
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201, "create should return 201");

    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().expect("response should have id");
    assert_eq!(body["content"], "# Hello\n\nThis is a test atom.");
    assert!(body["tags"].as_array().unwrap().is_empty());

    // Get
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["id"], atom_id);
    assert_eq!(body["content"], "# Hello\n\nThis is a test atom.");
}

#[actix_web::test]
async fn test_get_atom_not_found() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms/nonexistent-id")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn test_list_atoms_empty() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["total_count"], 0);
    assert!(body["atoms"].as_array().unwrap().is_empty());
}

#[actix_web::test]
async fn test_list_atoms_with_pagination() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create 3 atoms
    for i in 0..3 {
        let req = actix_test::TestRequest::post()
            .uri("/api/atoms")
            .insert_header(ctx.auth_header())
            .set_json(json!({ "content": format!("Atom {}", i) }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);
    }

    // List with limit=2
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms?limit=2")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["total_count"], 3);
    assert_eq!(body["atoms"].as_array().unwrap().len(), 2);
    assert_eq!(body["limit"], 2);
}

#[actix_web::test]
async fn test_update_atom() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "original" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().unwrap().to_string();

    // Update
    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "updated" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["content"], "updated");
}

#[actix_web::test]
async fn test_delete_atom() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "to delete" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().unwrap().to_string();

    // Delete
    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Verify gone
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn test_bulk_create_atoms() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms/bulk")
        .insert_header(ctx.auth_header())
        .set_json(json!([
            { "content": "Bulk atom 1" },
            { "content": "Bulk atom 2" },
            { "content": "Bulk atom 3" },
        ]))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["count"], 3);
    assert_eq!(body["atoms"].as_array().unwrap().len(), 3);
}

#[actix_web::test]
async fn test_create_atom_with_source_url() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "Article content",
            "source_url": "https://example.com/article",
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["source_url"], "https://example.com/article");
}

// ---------------------------------------------------------------------------
// Tag CRUD tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_create_and_list_tags() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create a tag
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "rust" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["name"], "rust");
    assert!(body["id"].as_str().is_some());

    // List tags (min_count=0 so we see tags with no atoms)
    let req = actix_test::TestRequest::get()
        .uri("/api/tags?min_count=0")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    let tags = body.as_array().unwrap();
    assert!(tags.iter().any(|t| t["name"] == "rust"));
}

#[actix_web::test]
async fn test_create_atom_with_tags() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create a tag first
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "testing" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tag: Value = actix_test::read_body_json(resp).await;
    let tag_id = tag["id"].as_str().unwrap().to_string();

    // Create atom with that tag
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "Tagged content",
            "tag_ids": [tag_id],
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    let tags = body["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0]["name"], "testing");
}

#[actix_web::test]
async fn test_update_tag() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "old-name" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tag: Value = actix_test::read_body_json(resp).await;
    let tag_id = tag["id"].as_str().unwrap().to_string();

    // Update
    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/tags/{}", tag_id))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "new-name" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["name"], "new-name");
}

#[actix_web::test]
async fn test_delete_tag() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "ephemeral" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tag: Value = actix_test::read_body_json(resp).await;
    let tag_id = tag["id"].as_str().unwrap().to_string();

    // Delete
    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/tags/{}", tag_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// Auth tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_unauthenticated_request_rejected() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .to_request();
    // The middleware may return an actix error (Err) or a proper 401 response (Ok).
    // Handle both: what matters is that the status is 401, not 200.
    match actix_test::try_call_service(&app, req).await {
        Ok(resp) => assert_eq!(resp.status(), 401, "should return 401 Unauthorized"),
        Err(err) => {
            let resp = err.error_response();
            assert_eq!(resp.status(), 401, "should return 401 Unauthorized");
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAPI spec test
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_openapi_spec_is_valid() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(App::new().app_data(ctx.state.clone()).route(
        "/api/docs/openapi.json",
        web::get().to(atomic_server::openapi_spec),
    ))
    .await;

    let req = actix_test::TestRequest::get()
        .uri("/api/docs/openapi.json")
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["openapi"], "3.1.0");
    assert!(body["paths"]["/api/atoms"].is_object());
    assert!(body["paths"]["/api/atoms/{id}"].is_object());
    assert!(body["paths"]["/api/tags"].is_object());
    assert!(body["components"]["schemas"]["Atom"].is_object());
    assert!(body["components"]["schemas"]["AtomWithTags"].is_object());
}

#[actix_web::test]
async fn test_shutdown_websocket_keeps_final_events_without_blocking_http_drain() {
    let ctx = TestCtx::new().await;
    let shutdown = web::Data::new(atomic_server::shutdown::Shutdown::default());
    let app = actix_test::init_service(
        App::new()
            .app_data(ctx.state.clone())
            .app_data(shutdown.clone())
            .wrap(actix_web::middleware::from_fn(atomic_server::shutdown::admission))
            .route("/ws", web::get().to(
                |req: HttpRequest,
                 state: web::Data<atomic_server::state::AppState>,
                 query: web::Query<atomic_server::ws::WsQuery>| async move {
                    // An idle client whose input remains open until shutdown.
                    let input: std::pin::Pin<Box<dyn futures::Stream<
                        Item = Result<web::Bytes, actix_web::error::PayloadError>,
                    >>> = Box::pin(futures::stream::pending());
                    let mut input = actix_web::dev::Payload::from(input);
                    let payload = <web::Payload as actix_web::FromRequest>::from_request(
                        &req, &mut input,
                    ).await?;
                    atomic_server::ws::ws_handler(req, payload, state, query).await
                },
            )),
    ).await;
    let req = actix_test::TestRequest::get()
        .uri(&format!("/ws?token={}", ctx.token))
        .insert_header(("connection", "upgrade"))
        .insert_header(("upgrade", "websocket"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_request();
    let response = actix_test::call_service(&app, req).await;
    assert_eq!(response.status(), actix_web::http::StatusCode::SWITCHING_PROTOCOLS);
    shutdown.begin();
    tokio::time::timeout(Duration::from_millis(100), shutdown.wait_idle())
        .await.unwrap();
    ctx.state.event_tx.send(atomic_server::state::ServerEvent::EmbeddingComplete {
        atom_id: "fictional-final-atom".into(),
        database_id: None,
    }).unwrap();
    let close_streams = shutdown.close_streams();
    tokio::pin!(close_streams);
    // Let the actor enqueue completion and Close while the body is unpolled.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut close_streams)
            .await.is_err(),
        "WS drain ended before response body EOF"
    );
    let ((), body) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(&mut close_streams, actix_web::body::to_bytes(response.into_body()))
    }).await.unwrap();
    let body = body.unwrap();
    let frames = String::from_utf8_lossy(&body);
    assert!(frames.contains("EmbeddingComplete"), "missing completion: {frames}");
    assert!(frames.contains("fictional-final-atom"));
}

#[actix_web::test]
async fn test_database_markdown_export_job_downloads_zip() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "# Exported Note\n\nThis should land in a ZIP.",
            "tag_ids": [],
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let db_id = ctx.state.manager.active_id().unwrap();
    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/databases/{}/exports/markdown", db_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 202);
    let mut job: Value = actix_test::read_body_json(resp).await;
    let job_id = job["id"].as_str().unwrap().to_string();

    for _ in 0..50 {
        if job["status"] == "complete" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let req = actix_test::TestRequest::get()
            .uri(&format!("/api/exports/{}", job_id))
            .insert_header(ctx.auth_header())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        job = actix_test::read_body_json(resp).await;
    }

    assert_eq!(job["status"], "complete");
    assert_eq!(job["total_atoms"], 1);
    let download_path = job["download_path"].as_str().unwrap();

    let req = actix_test::TestRequest::get()
        .uri(download_path)
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body = actix_test::read_body(resp).await;
    assert!(body.starts_with(b"PK"));

    // Shutdown waits for the writer, not its 24-hour artifact retention timer.
    atomic_core::executor::wait_idle().await;
    let job_dir = ctx._temp.path().join("exports").join(&job_id);
    assert!(job_dir.join("markdown.zip").exists());
    assert!(!job_dir.join("markdown.zip.part").exists());
    assert!(!job_dir.join("snapshot.db").exists());
    ctx.state.export_jobs.cleanup_finished();
    assert!(!job_dir.exists());
}
