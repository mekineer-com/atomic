//! Atomic-owned Actix transport for MCP Streamable HTTP.
//!
//! This keeps Atomic on the official `rmcp` protocol/service layer while
//! owning the HTTP boundary where clients are strict about status codes and
//! content types.

use super::{AtomicMcpServer, DbSelection};
use crate::state::ServerEvent;
use actix_web::{
    error::InternalError,
    http::{
        header::{self, CACHE_CONTROL},
        StatusCode,
    },
    middleware,
    web::{self, Bytes, Data},
    HttpMessage, HttpRequest, HttpResponse, Result, Scope,
};
use atomic_core::executor::{WorkGuard, WorkTracker};
use atomic_core::manager::DatabaseManager;
use futures::{Stream, StreamExt};
use rmcp::{
    model::{ClientJsonRpcMessage, ClientRequest, GetExtensions},
    serve_server,
    service::{NotificationContext, RequestContext},
    transport::{
        common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        },
        streamable_http_server::session::{local::LocalSessionManager, SessionManager},
        TransportAdapterIdentity,
    },
    ErrorData, RoleServer, Service,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{broadcast, watch};

const HEADER_X_ACCEL_BUFFERING: &str = "X-Accel-Buffering";
const TEXT_MIME_TYPE: &str = "text/plain; charset=utf-8";

#[derive(Clone)]
pub struct AtomicMcpTransport {
    state: Data<TransportState>,
}

impl AtomicMcpTransport {
    pub fn new(
        manager: Arc<DatabaseManager>,
        event_tx: broadcast::Sender<ServerEvent>,
        sse_keep_alive: Duration,
    ) -> Self {
        Self {
            state: Data::new(TransportState {
                manager,
                event_tx,
                session_manager: Arc::new(LocalSessionManager::default()),
                sse_keep_alive,
                requests: WorkTracker::default(),
                sessions: WorkTracker::default(),
                idle_stop: watch::channel(false).0,
                protocol_stop: watch::channel(false).0,
            }),
        }
    }

    pub fn close_idle_streams(&self) {
        self.state.idle_stop.send_replace(true);
    }

    pub async fn shutdown(&self) {
        // HTTP bodies may have disconnected while their MCP calls still run.
        self.state.requests.wait_idle().await;
        self.state.protocol_stop.send_replace(true);
        self.state.sessions.wait_idle().await;
    }

    pub fn scope(
        self,
    ) -> Scope<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        web::scope("")
            .app_data(self.state.clone())
            .wrap(middleware::NormalizePath::trim())
            .route("", web::get().to(handle_get))
            .route("", web::post().to(handle_post))
            .route("", web::delete().to(handle_delete))
    }
}

struct TransportState {
    manager: Arc<DatabaseManager>,
    event_tx: broadcast::Sender<ServerEvent>,
    session_manager: Arc<LocalSessionManager>,
    sse_keep_alive: Duration,
    requests: WorkTracker,
    sessions: WorkTracker,
    idle_stop: watch::Sender<bool>,
    protocol_stop: watch::Sender<bool>,
}

impl TransportState {
    fn server(&self) -> DrainingMcpServer {
        DrainingMcpServer(AtomicMcpServer::new(
            Arc::clone(&self.manager),
            self.event_tx.clone(),
        ))
    }
}

// Keep admission accounting alive across rmcp's detached request dispatch,
// even if a tool consumes/drops its RequestContext before finishing work.
struct DrainingMcpServer(AtomicMcpServer);

impl Service<RoleServer> for DrainingMcpServer {
    async fn handle_request(
        &self,
        request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ServerResult, ErrorData> {
        let _work = context.extensions.get::<Arc<WorkGuard>>().cloned();
        self.0.handle_request(request, context).await
    }

    async fn handle_notification(
        &self,
        notification: rmcp::model::ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.0.handle_notification(notification, context).await
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        Service::get_info(&self.0)
    }
}

async fn stopped(mut stop: watch::Receiver<bool>) {
    stop.wait_for(|value| *value)
        .await
        .expect("MCP stop closed");
}

fn text_error(status: StatusCode, message: &'static str) -> HttpResponse {
    HttpResponse::build(status)
        .content_type(TEXT_MIME_TYPE)
        .body(message)
}

fn accepts(req: &HttpRequest, media_type: &str) -> bool {
    req.headers()
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|part| {
                let item = part
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                item == "*/*" || item == media_type || item == media_type.replace("text/", "*/")
            })
        })
}

fn accepts_all(req: &HttpRequest, media_types: &[&str]) -> bool {
    media_types
        .iter()
        .all(|media_type| accepts(req, media_type))
}

fn has_json_content_type(req: &HttpRequest) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case(JSON_MIME_TYPE)
        })
}

fn request_session_id(req: &HttpRequest) -> Option<Arc<str>> {
    req.headers()
        .get(HEADER_SESSION_ID)
        .and_then(|v| v.to_str().ok())
        .map(|s| Arc::<str>::from(s.to_owned()))
}

fn db_selection(req: &HttpRequest) -> DbSelection {
    let db_id = req.query_string().split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        if parts.next()? == "db" {
            parts.next().map(String::from)
        } else {
            None
        }
    });
    DbSelection(db_id)
}

fn attach_request_extensions(req: &HttpRequest, message: &mut ClientJsonRpcMessage) {
    if let ClientJsonRpcMessage::Request(request_msg) = message {
        request_msg
            .request
            .extensions_mut()
            .insert(db_selection(req));
    }
}

fn wrap_with_sse_keepalive<S>(
    stream: S,
    keep_alive: Duration,
) -> impl Stream<Item = Result<Bytes, actix_web::Error>>
where
    S: Stream<Item = Result<Bytes, actix_web::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut stream = Box::pin(stream);
        let mut keep_alive_timer = tokio::time::interval(keep_alive);
        keep_alive_timer.tick().await;

        loop {
            tokio::select! {
                result = stream.next() => {
                    match result {
                        Some(msg) => yield msg,
                        None => break,
                    }
                }
                _ = keep_alive_timer.tick() => {
                    yield Ok(Bytes::from(":ping\n\n"));
                }
            }
        }
    }
}

async fn handle_get(req: HttpRequest, state: Data<TransportState>) -> Result<HttpResponse> {
    if !accepts(&req, EVENT_STREAM_MIME_TYPE) {
        return Ok(text_error(
            StatusCode::NOT_ACCEPTABLE,
            "Not Acceptable: Client must accept text/event-stream",
        ));
    }

    let Some(session_id) = request_session_id(&req) else {
        return Ok(text_error(
            StatusCode::BAD_REQUEST,
            "Bad Request: Session ID is required",
        ));
    };

    let has_session = state
        .session_manager
        .has_session(&session_id)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    if !has_session {
        return Ok(text_error(
            StatusCode::NOT_FOUND,
            "Not Found: Session not found",
        ));
    }

    let last_event_id = req
        .headers()
        .get(HEADER_LAST_EVENT_ID)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    // rmcp's validated event IDs are <index> for the idle common channel,
    // or <index>/<request-id> for a finite request response stream.
    let resumed_request = last_event_id.as_deref().is_some_and(|id| id.contains('/'));

    let sse_stream: std::pin::Pin<Box<dyn Stream<Item = _> + Send>> =
        if let Some(last_event_id) = last_event_id {
            Box::pin(
                state
                    .session_manager
                    .resume(&session_id, last_event_id)
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?,
            )
        } else {
            Box::pin(
                state
                    .session_manager
                    .create_standalone_stream(&session_id)
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?,
            )
        };

    let formatted_stream = sse_stream.map(|msg| {
        let mut output = String::new();
        if let Some(id) = msg.event_id {
            output.push_str(&format!("id: {id}\n"));
        }
        match msg.message {
            Some(message) => {
                let data = serde_json::to_string(message.as_ref()).unwrap_or_else(|_| "{}".into());
                output.push_str(&format!("data: {data}\n\n"));
            }
            None => output.push_str("data:\n\n"),
        }
        Ok::<_, actix_web::Error>(Bytes::from(output))
    });

    let idle_stop = state.idle_stop.subscribe();
    Ok(HttpResponse::Ok()
        .content_type(EVENT_STREAM_MIME_TYPE)
        .append_header((CACHE_CONTROL, "no-cache"))
        .append_header((HEADER_X_ACCEL_BUFFERING, "no"))
        .streaming(wrap_with_sse_keepalive(
            formatted_stream.take_until(async move {
                if resumed_request {
                    std::future::pending::<()>().await;
                } else {
                    stopped(idle_stop).await;
                }
            }),
            state.sse_keep_alive,
        )))
}

async fn handle_post(
    req: HttpRequest,
    body: Bytes,
    state: Data<TransportState>,
) -> Result<HttpResponse> {
    if !accepts_all(&req, &[JSON_MIME_TYPE, EVENT_STREAM_MIME_TYPE]) {
        return Ok(text_error(
            StatusCode::NOT_ACCEPTABLE,
            "Not Acceptable: Client must accept both application/json and text/event-stream",
        ));
    }

    if !has_json_content_type(&req) {
        return Ok(text_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported Media Type: Content-Type must be application/json",
        ));
    }

    let mut message: ClientJsonRpcMessage = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(_) => {
            return Ok(text_error(
                StatusCode::BAD_REQUEST,
                "Bad Request: Body must be a valid MCP JSON-RPC message",
            ));
        }
    };

    attach_request_extensions(&req, &mut message);
    if *state.protocol_stop.borrow() {
        return Ok(HttpResponse::ServiceUnavailable().finish());
    }
    if let ClientJsonRpcMessage::Request(request) = &mut message {
        if (req.app_data::<Data<crate::shutdown::Shutdown>>().is_some()
            || *state.idle_stop.borrow())
            && req
                .extensions()
                .get::<crate::shutdown::AdmittedRequest>()
                .is_none()
        {
            return Ok(HttpResponse::ServiceUnavailable().finish());
        }
        // rmcp retains initialize's request until the client finishes its
        // handshake. It is read-only; that idle lifetime is not admitted work.
        if !matches!(request.request, ClientRequest::InitializeRequest(_)) {
            request
                .request
                .extensions_mut()
                .insert(Arc::new(state.requests.start()));
        }
    }

    if let Some(session_id) = request_session_id(&req) {
        let has_session = state
            .session_manager
            .has_session(&session_id)
            .await
            .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

        if !has_session {
            return Ok(text_error(
                StatusCode::NOT_FOUND,
                "Not Found: Session not found",
            ));
        }

        return match message {
            ClientJsonRpcMessage::Request(request_msg) => {
                let stream = state
                    .session_manager
                    .create_stream(&session_id, ClientJsonRpcMessage::Request(request_msg))
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

                let formatted_stream = stream.map(|msg| {
                    let mut output = String::new();
                    if let Some(id) = msg.event_id {
                        output.push_str(&format!("id: {id}\n"));
                    }
                    match msg.message {
                        Some(message) => {
                            let data = serde_json::to_string(message.as_ref())
                                .unwrap_or_else(|_| "{}".into());
                            output.push_str(&format!("data: {data}\n\n"));
                        }
                        None => output.push_str("data:\n\n"),
                    }
                    Ok::<_, actix_web::Error>(Bytes::from(output))
                });

                Ok(HttpResponse::Ok()
                    .content_type(EVENT_STREAM_MIME_TYPE)
                    .append_header((CACHE_CONTROL, "no-cache"))
                    .append_header((HEADER_X_ACCEL_BUFFERING, "no"))
                    .streaming(wrap_with_sse_keepalive(
                        formatted_stream,
                        state.sse_keep_alive,
                    )))
            }
            ClientJsonRpcMessage::Notification(_)
            | ClientJsonRpcMessage::Response(_)
            | ClientJsonRpcMessage::Error(_) => {
                state
                    .session_manager
                    .accept_message(&session_id, message)
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

                Ok(HttpResponse::Accepted().finish())
            }
        };
    }

    let is_initialize = matches!(
        &message,
        ClientJsonRpcMessage::Request(request_msg)
            if matches!(request_msg.request, ClientRequest::InitializeRequest(_))
    );

    if !is_initialize {
        return Ok(text_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Unprocessable Entity: Expected initialize request",
        ));
    }

    let (session_id, transport) = state
        .session_manager
        .create_session()
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    let session_work = state.sessions.start();
    let protocol_stop = state.protocol_stop.subscribe();
    tokio::spawn({
        let session_manager = Arc::clone(&state.session_manager);
        let session_id = Arc::clone(&session_id);
        let service_instance = state.server();
        async move {
            let _session_work = session_work;
            let service = tokio::select! {
                result = serve_server::<DrainingMcpServer, _, _, TransportAdapterIdentity>(
                    service_instance, transport,
                ) => Some(result),
                _ = stopped(protocol_stop.clone()) => None,
            };

            match service {
                Some(Ok(service)) => {
                    let cancel = service.cancellation_token();
                    let waiting = service.waiting();
                    tokio::pin!(waiting);
                    tokio::select! {
                        _ = &mut waiting => {},
                        _ = stopped(protocol_stop) => {
                            cancel.cancel();
                            let _ = waiting.await;
                        }
                    }
                }
                Some(Err(e)) => tracing::error!("Failed to create MCP service: {e}"),
                None => {}
            }

            let _ = session_manager
                .close_session(&session_id)
                .await
                .inspect_err(|e| {
                    tracing::error!("Failed to close MCP session {session_id}: {e}");
                });
        }
    });

    let response = state
        .session_manager
        .initialize_session(&session_id, message)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    let sse_stream = async_stream::stream! {
        let data = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
        yield Ok::<_, actix_web::Error>(Bytes::from(format!("data: {data}\n\n")));
    };

    Ok(HttpResponse::Ok()
        .content_type(EVENT_STREAM_MIME_TYPE)
        .append_header((CACHE_CONTROL, "no-cache"))
        .append_header((HEADER_X_ACCEL_BUFFERING, "no"))
        .append_header((HEADER_SESSION_ID, session_id.as_ref()))
        .streaming(sse_stream))
}

async fn handle_delete(req: HttpRequest, state: Data<TransportState>) -> Result<HttpResponse> {
    let Some(session_id) = request_session_id(&req) else {
        return Ok(text_error(
            StatusCode::BAD_REQUEST,
            "Bad Request: Session ID is required",
        ));
    };

    let has_session = state
        .session_manager
        .has_session(&session_id)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    if !has_session {
        return Ok(text_error(
            StatusCode::NOT_FOUND,
            "Not Found: Session not found",
        ));
    }

    state
        .session_manager
        .close_session(&session_id)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    Ok(HttpResponse::NoContent().finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ServerEvent;
    use actix_web::{body::to_bytes, http::header, test as actix_test, App};
    use serde_json::json;

    fn test_transport() -> (AtomicMcpTransport, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(DatabaseManager::new(temp.path()).unwrap());
        let (event_tx, _) = broadcast::channel::<ServerEvent>(16);
        let transport = AtomicMcpTransport::new(manager, event_tx, Duration::from_secs(30));

        (transport, temp)
    }

    fn initialize_request() -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "atomic-test",
                    "version": "0.0.0"
                }
            }
        })
    }

    fn tools_list_request(id: i32) -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": {}
        })
    }

    macro_rules! initialize_session {
        ($app:expr) => {{
            let req = actix_test::TestRequest::post()
                .uri("/mcp")
                .insert_header((
                    header::ACCEPT,
                    format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
                ))
                .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
                .set_payload(initialize_request().to_string())
                .to_request();
            let resp = actix_test::call_service(&$app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_content_type(&resp, EVENT_STREAM_MIME_TYPE);
            let session_id = resp
                .headers()
                .get(HEADER_SESSION_ID)
                .and_then(|value| value.to_str().ok())
                .expect("initialize response should include session id")
                .to_string();
            let body = to_bytes(resp.into_body()).await.unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(
                body.contains("\"result\""),
                "initialize should return a JSON-RPC result: {body}"
            );
            session_id
        }};
    }

    macro_rules! send_initialized {
        ($app:expr, $session_id:expr) => {{
            let req = actix_test::TestRequest::post()
                .uri("/mcp")
                .insert_header((
                    header::ACCEPT,
                    format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
                ))
                .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
                .insert_header((HEADER_SESSION_ID, $session_id.clone()))
                .set_payload(
                    json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    })
                    .to_string(),
                )
                .to_request();
            let resp = actix_test::call_service(&$app, req).await;
            assert_eq!(resp.status(), StatusCode::ACCEPTED);
        }};
    }

    fn assert_content_type<B>(resp: &actix_web::dev::ServiceResponse<B>, expected: &str) {
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .expect("response should include content-type");
        assert!(
            content_type.starts_with(expected),
            "expected content-type {expected}, got {content_type}"
        );
    }

    #[actix_web::test]
    async fn get_missing_session_returns_400_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/mcp")
            .insert_header((header::ACCEPT, EVENT_STREAM_MIME_TYPE))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn get_unknown_session_returns_404_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/mcp")
            .insert_header((header::ACCEPT, EVENT_STREAM_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, "stale-session"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn post_unknown_session_returns_404_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, "stale-session"))
            .set_payload(tools_list_request(2).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn post_without_session_requires_initialize_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .set_payload(tools_list_request(2).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn shutdown_closes_idle_handshake_without_waiting_for_client() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.clone().scope())),
        )
        .await;
        let _session_id = initialize_session!(app);
        transport.close_idle_streams();
        tokio::time::timeout(Duration::from_secs(1), transport.shutdown())
            .await
            .unwrap();
        assert!(transport
            .state
            .session_manager
            .sessions
            .read()
            .await
            .is_empty());
    }

    #[actix_web::test]
    async fn shutdown_waits_for_queued_request_after_http_disconnect() {
        let fetched = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let fetched_handler = fetched.clone();
        let release_handler = release.clone();
        let source = actix_web::HttpServer::new(move || {
            let fetched = fetched_handler.clone();
            let release = release_handler.clone();
            App::new().route(
                "/",
                web::get().to(move || {
                    let fetched = fetched.clone();
                    let release = release.clone();
                    async move {
                        fetched.notify_one();
                        release.notified().await;
                        let paragraphs = "<p>This fictional field report records observations from a synthetic greenhouse. The sample plants received equal amounts of water, light, and nutrients throughout the trial. These invented observations exist only to exercise article ingestion and durable storage during shutdown.</p>".repeat(8);
                        HttpResponse::Ok().content_type("text/html").body(format!(
                            "<html><head><title>Fictional field report</title></head><body><article><h1>Fictional field report</h1>{paragraphs}</article></body></html>"
                        ))
                    }
                }),
            )
        })
        .workers(1)
        .disable_signals()
        .bind(("127.0.0.1", 0))
        .unwrap();
        let source_url = format!("http://{}/", source.addrs()[0]);
        let source = source.run();
        let source_handle = source.handle();
        actix_web::rt::spawn(source);

        let (transport, _temp) = test_transport();
        let shutdown = Data::new(crate::shutdown::Shutdown::default());
        let app = actix_test::init_service(
            App::new()
                .app_data(shutdown.clone())
                .wrap(middleware::from_fn(crate::shutdown::admission))
                .service(web::scope("/mcp").service(transport.clone().scope())),
        )
        .await;
        let session_id = initialize_session!(app);
        send_initialized!(app, session_id);
        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id.clone()))
            .set_payload(
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "ingest_url", "arguments": {"url": source_url.clone()}}
                })
                .to_string(),
            )
            .to_request();
        drop(actix_test::call_service(&app, req).await);
        tokio::time::timeout(Duration::from_secs(2), fetched.notified())
            .await
            .unwrap();
        // The first request-wise channel is 0. Prime its resumable cursor
        // through rmcp's own disconnect API while the mutation is held.
        transport
            .state
            .session_manager
            .sessions
            .read()
            .await
            .get(session_id.as_str())
            .unwrap()
            .close_sse_stream(0, Some(Duration::from_millis(1)))
            .await
            .unwrap();
        let resumed = actix_test::TestRequest::get()
            .uri("/mcp")
            .insert_header((header::ACCEPT, EVENT_STREAM_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id.clone()))
            .insert_header((HEADER_LAST_EVENT_ID, "0/0"))
            .to_request();
        let resumed = actix_test::call_service(&app, resumed).await;
        assert_eq!(resumed.status(), StatusCode::OK);
        let resumed_body = to_bytes(resumed.into_body());
        tokio::pin!(resumed_body);
        shutdown.begin();
        transport.close_idle_streams();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut resumed_body)
                .await
                .is_err()
        );
        let refused = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id.clone()))
            .set_payload(tools_list_request(3).to_string())
            .to_request();
        assert_eq!(
            actix_test::call_service(&app, refused).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), transport.shutdown())
                .await
                .is_err()
        );
        release.notify_one();
        let terminal = tokio::time::timeout(Duration::from_secs(2), &mut resumed_body)
            .await
            .unwrap()
            .unwrap();
        let terminal = String::from_utf8(terminal.to_vec()).unwrap();
        assert!(
            terminal.contains("\"result\""),
            "missing terminal result: {terminal}"
        );
        assert!(terminal.contains("\"id\":2"), "wrong request: {terminal}");
        shutdown.wait_idle().await;
        tokio::time::timeout(Duration::from_secs(1), transport.shutdown())
            .await
            .unwrap();
        let core = transport.state.manager.active_core().await.unwrap();
        assert!(core
            .get_atom_by_source_url(&source_url)
            .await
            .unwrap()
            .is_some());
        atomic_core::executor::wait_idle().await;
        source_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn shutdown_closes_standalone_sse_without_waiting_for_keepalive() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.clone().scope())),
        )
        .await;
        let session_id = initialize_session!(app);
        send_initialized!(app, session_id);
        let req = actix_test::TestRequest::get()
            .uri("/mcp")
            .insert_header((header::ACCEPT, EVENT_STREAM_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .to_request();
        let response = actix_test::call_service(&app, req).await;
        transport.close_idle_streams();
        tokio::time::timeout(Duration::from_secs(1), to_bytes(response.into_body()))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), transport.shutdown())
            .await
            .unwrap();
    }

    #[actix_web::test]
    async fn initialize_returns_sse_and_session_header() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;
        let session_id = initialize_session!(app);

        assert!(!session_id.is_empty());
    }

    #[actix_web::test]
    async fn notification_with_session_returns_accepted() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;
        let session_id = initialize_session!(app);
        send_initialized!(app, session_id);

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .set_payload(
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            )
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[actix_web::test]
    async fn request_with_session_returns_sse_stream() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;
        let session_id = initialize_session!(app);

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .set_payload(tools_list_request(2).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_content_type(&resp, EVENT_STREAM_MIME_TYPE);

        let _ = resp.into_body();
    }

    #[actix_web::test]
    async fn delete_unknown_session_returns_404_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::delete()
            .uri("/mcp")
            .insert_header((HEADER_SESSION_ID, "stale-session"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_content_type(&resp, "text/plain");
    }
}
