//! Integration tests for the Atom and Tag REST API endpoints.
//!
//! Each test spins up a real actix-web test server backed by a temporary SQLite
//! database and exercises the endpoints with actual HTTP requests.

use actix_web::{App, HttpRequest, HttpResponse, HttpServer, test as actix_test, web};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
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
            memu_session: memu_base_url.map(|base_url| atomic_server::state::MemuSessionConfig {
                base_url,
                user_id: "Marcos".to_string(),
                soul_id: "Siri".to_string(),
            }),
            export_jobs: atomic_server::export_jobs::ExportJobManager::for_tests(
                temp.path().join("exports"),
            ),
            setup_token: None,
            dangerously_skip_setup_token: false,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: atomic_server::state::SetupClaimLimiter::new(),
        });
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
    assert_eq!(body["user_id"], "Marcos");
    assert_eq!(body["soul_id"], "Siri");
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
        "settings": {
            "provider": "openai_compat",
            "openai_compat_base_url": "http://127.0.0.1:9",
            "openai_compat_api_key": "test-key",
            "openai_compat_llm_model": "mock-llm"
        }
    }))
}

fn start_memu_stub(handler: fn() -> actix_web::Route) -> (String, actix_web::dev::ServerHandle) {
    let server = HttpServer::new(move || {
        App::new()
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

fn start_memu_stub_with_model(model_url: String) -> (String, actix_web::dev::ServerHandle) {
    let server = HttpServer::new(move || {
        let model_url = model_url.clone();
        App::new()
            .route(
                "/integration/atomic/session_start",
                web::post().to(atomic_session_start_ok),
            )
            .route(
                "/integration/atomic/chat_profile",
                web::get().to(move || {
                    let model_url = model_url.clone();
                    async move {
                        HttpResponse::Ok().json(json!({
                            "settings": {
                                "provider": "openai_compat",
                                "openai_compat_base_url": model_url,
                                "openai_compat_api_key": "test-key",
                                "openai_compat_llm_model": "mock-llm"
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
    (format!("http://{}", addr), handle)
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

async fn memu_approve_empty() -> HttpResponse {
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

async fn memu_search(req: HttpRequest) -> HttpResponse {
    let query = req.query_string();
    if query.contains("q=memory") {
        assert!(query.contains("mode=hybrid"));
    }
    if query.contains("q=global") {
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
            "embedding": [1.0, 0.0],
            "primary_tag": "Core",
            "tag_count": 1,
            "tag_ids": ["category:c1"],
            "source_url": null
        }, {
            "id": "memory:m2",
            "title": "Memory two",
            "embedding": [0.9, 0.1],
            "primary_tag": "Core",
            "tag_count": 1,
            "tag_ids": ["category:c1"],
            "source_url": null
        }]
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
            .route("/integration/atomic/atoms", web::get().to(memu_atoms))
            .route("/integration/atomic/tags", web::get().to(memu_tags))
            .route("/integration/atomic/search", web::get().to(memu_search))
            .route(
                "/integration/atomic/canvas-source",
                web::get().to(memu_canvas_source),
            )
            .route(
                "/integration/atomic/neighborhood/{id}",
                web::get().to(memu_neighborhood),
            )
            .route("/memory/{id}", web::get().to(memu_memory))
            .route("/memory/{id}", web::patch().to(memu_update_memory))
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

// ---------------------------------------------------------------------------
// Atom CRUD tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_create_conversation_requires_memu_config() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 500);
}

#[actix_web::test]
async fn test_create_conversation_stores_hidden_memu_snapshot() {
    let (memu_url, memu_handle) = start_memu_stub(|| web::post().to(atomic_session_start_ok));
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
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
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 502);

    let req = actix_test::TestRequest::get()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
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
async fn test_memu_read_routes_proxy_and_keep_writes_read_only() {
    let (memu_url, memu_handle) = start_memu_memory_stub();
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let atoms: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(atoms["atoms"][0]["id"], "memory:m1");

    let req = actix_test::TestRequest::get()
        .uri("/api/tags?min_count=0")
        .insert_header(ctx.auth_header())
        .to_request();
    let tags: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(tags[0]["id"], "category:c1");

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms/memory:m1")
        .insert_header(ctx.auth_header())
        .to_request();
    let atom: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(atom["content"], "Memory summary");
    assert_eq!(atom["tags"][0]["id"], "category:c1");

    let req = actix_test::TestRequest::post()
        .uri("/api/search")
        .insert_header(ctx.auth_header())
        .set_json(json!({"query": "memory", "mode": "hybrid"}))
        .to_request();
    let search: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(search[0]["id"], "memory:m1");

    let req = actix_test::TestRequest::post()
        .uri("/api/search/global")
        .insert_header(ctx.auth_header())
        .set_json(json!({"query": "global", "section_limit": 5}))
        .to_request();
    let search: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(search["atoms"][0]["id"], "memory:m1");

    let req = actix_test::TestRequest::get()
        .uri("/api/canvas/global")
        .insert_header(ctx.auth_header())
        .to_request();
    let canvas: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(canvas["atoms"][0]["atom_id"], "memory:m1");
    assert_eq!(canvas["atoms"][0]["tag_ids"][0], "category:c1");
    assert_eq!(canvas["edges"][0]["source"], "memory:m1");
    assert_eq!(canvas["edges"][0]["target"], "memory:m2");

    let req = actix_test::TestRequest::get()
        .uri("/api/graph/neighborhood/memory:m1")
        .insert_header(ctx.auth_header())
        .to_request();
    let neighborhood: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(neighborhood["center_atom_id"], "memory:m1");
    assert_eq!(neighborhood["atoms"][0]["depth"], 0);

    let req = actix_test::TestRequest::put()
        .uri("/api/atoms/memory:m1/content")
        .insert_header(ctx.auth_header())
        .set_json(json!({"content": "changed"}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 409);

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({"content": "hidden local atom"}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 409);

    let req = actix_test::TestRequest::put()
        .uri("/api/canvas/positions")
        .insert_header(ctx.auth_header())
        .set_json(json!([{"atom_id": "memory:m1", "x": 1.0, "y": 2.0}]))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let req = actix_test::TestRequest::get()
        .uri("/api/canvas/positions")
        .insert_header(ctx.auth_header())
        .to_request();
    let positions: Value = actix_test::call_and_read_body_json(&app, req).await;
    assert_eq!(positions.as_array().unwrap().len(), 0);

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
        .set_json(json!({"summary": "Updated memory"}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    match tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        atomic_server::state::ServerEvent::AtomUpdated { atom } => {
            assert_eq!(atom.atom.id, "memory:m1");
            assert_eq!(atom.atom.content, "Updated memory");
        }
        other => panic!("unexpected event: {other:?}"),
    }

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
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    assert!(events.try_recv().is_err());

    memu_handle.stop(true).await;
}

#[actix_web::test]
async fn test_send_message_uses_memu_chat_profile() {
    let (model_url, model_handle) = start_fake_model();
    let (memu_url, memu_handle) = start_memu_stub_with_model(model_url);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let created: Value = actix_test::read_body_json(resp).await;
    let conversation_id = created["id"].as_str().unwrap();

    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/conversations/{conversation_id}/messages"))
        .insert_header(ctx.auth_header())
        .set_json(json!({"content": "hello"}))
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

    memu_handle.stop(true).await;
    model_handle.stop(true).await;
}

#[actix_web::test]
async fn test_memu_backed_chat_offers_edit_but_not_create() {
    let (model_url, model_handle) = start_fake_model_with_memu_write_tools();
    let (memu_url, memu_handle) = start_memu_stub_with_model(model_url);
    let ctx = TestCtx::new_with_memu(Some(memu_url)).await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/conversations")
        .insert_header(ctx.auth_header())
        .set_json(json!({"tag_ids": [], "title": null}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let created: Value = actix_test::read_body_json(resp).await;
    let conversation_id = created["id"].as_str().unwrap();

    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/conversations/{conversation_id}/messages"))
        .insert_header(ctx.auth_header())
        .set_json(json!({"content": "hello"}))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

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
}
