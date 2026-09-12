//! Bearer token authentication middleware — verifies tokens against the database

use crate::state::AppState;
use actix_web::Error;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::error::{ErrorConflict, ErrorUnauthorized};
use actix_web::http::Method;
use actix_web::web;
use futures::future::{LocalBoxFuture, Ready, ok};
use std::task::{Context, Poll};

/// Middleware that requires a valid Bearer token (looked up in the api_tokens table)
pub struct BearerAuth {
    pub state: web::Data<AppState>,
}

impl<S, B> Transform<S, ServiceRequest> for BearerAuth
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = BearerAuthMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(BearerAuthMiddleware {
            service,
            state: self.state.clone(),
        })
    }
}

pub struct BearerAuthMiddleware<S> {
    service: S,
    state: web::Data<AppState>,
}

fn integrated_api_allowed(method: &Method, path: &str) -> bool {
    let get = method == Method::GET;
    let post = method == Method::POST;

    if path == "/api/conversations" || path.starts_with("/api/conversations/") {
        return true;
    }
    if path.starts_with("/api/memu/") {
        return true;
    }
    if (path == "/api/search" || path == "/api/search/global") && post {
        return true;
    }
    if path == "/api/atoms" && (get || post) {
        return true;
    }
    if path.starts_with("/api/atoms/")
        && path != "/api/atoms/sources"
        && path != "/api/atoms/link-suggestions"
        && path != "/api/atoms/by-source-url"
        && !path.ends_with("/embedding-status")
        && get
    {
        return true;
    }
    if (path == "/api/tags" || path.starts_with("/api/tags/") && path.ends_with("/children")) && get
    {
        return true;
    }
    if (path == "/api/canvas/global" && get) || (path == "/api/canvas/rebuild" && post) {
        return true;
    }
    if path.starts_with("/api/graph/neighborhood/") && get {
        return true;
    }

    path == "/api/provider/verify"
        || path == "/api/utils/sqlite-vec"
        || path == "/api/settings"
        || path.starts_with("/api/settings/")
        || path.starts_with("/api/ollama/")
        || path == "/api/auth/tokens"
        || path.starts_with("/api/auth/tokens/")
        || (path == "/api/databases" || path.starts_with("/api/databases/"))
            && !path.contains("/exports/")
}

impl<S, B> Service<ServiceRequest> for BearerAuthMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let state = self.state.clone();
        let blocked =
            state.memu_session.is_some() && !integrated_api_allowed(req.method(), req.path());

        // Extract the Authorization header
        let raw_token = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(String::from);

        let raw_token = match raw_token {
            Some(t) => t,
            None => {
                return Box::pin(async {
                    Err(ErrorUnauthorized("Invalid or missing Bearer token"))
                });
            }
        };

        let fut = self.service.call(req);
        Box::pin(async move {
            // Verify token — uses registry if available, falls through to storage backend otherwise
            let core = match state.manager.active_core().await {
                Ok(c) => c,
                Err(_) => {
                    return Err(ErrorUnauthorized("Invalid or missing Bearer token"));
                }
            };

            let token_info = match core.verify_api_token(&raw_token).await {
                Ok(Some(info)) => info,
                Ok(None) | Err(_) => {
                    return Err(ErrorUnauthorized("Invalid or missing Bearer token"));
                }
            };

            // Fire-and-forget last_used_at update
            let token_id = token_info.id.clone();
            let core_clone = core.clone();
            tokio::spawn(async move {
                let _ = core_clone.update_token_last_used(&token_id).await;
            });

            if blocked {
                return Err(ErrorConflict(
                    "Local Atomic knowledge is unavailable in OpenAlma mode",
                ));
            }

            fut.await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test as actix_test;
    use actix_web::{App, HttpResponse, web};
    use tokio::sync::broadcast;

    async fn protected_endpoint() -> HttpResponse {
        HttpResponse::Ok().json(serde_json::json!({"ok": true}))
    }

    #[test]
    fn integrated_mode_allows_only_scoped_data_and_instance_controls() {
        assert!(integrated_api_allowed(
            &Method::GET,
            "/api/conversations/example"
        ));
        assert!(integrated_api_allowed(&Method::POST, "/api/search"));
        assert!(integrated_api_allowed(
            &Method::GET,
            "/api/atoms/memory:example"
        ));
        assert!(integrated_api_allowed(&Method::POST, "/api/atoms"));
        assert!(integrated_api_allowed(&Method::GET, "/api/settings/models"));
        assert!(integrated_api_allowed(
            &Method::PUT,
            "/api/databases/example/activate"
        ));

        assert!(!integrated_api_allowed(&Method::GET, "/api/wiki"));
        assert!(!integrated_api_allowed(&Method::POST, "/api/reports"));
        assert!(!integrated_api_allowed(
            &Method::POST,
            "/api/databases/example/exports/markdown"
        ));
        assert!(!integrated_api_allowed(
            &Method::GET,
            "/api/canvas/positions"
        ));
        assert!(!integrated_api_allowed(&Method::GET, "/api/graph/edges"));
        assert!(!integrated_api_allowed(
            &Method::GET,
            "/api/atoms/by-source-url"
        ));
    }

    async fn test_app_state(integrated: bool) -> (web::Data<AppState>, String) {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = std::sync::Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (info, raw_token) = manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test-token")
            .await
            .unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: None,
            log_buffer: crate::log_buffer::LogBuffer::new(16),
            memu_session: integrated.then(|| crate::state::MemuSessionConfig {
                base_url: "http://127.0.0.1:8099".to_string(),
            }),
            export_jobs: crate::export_jobs::ExportJobManager::for_tests(
                temp.path().join("exports"),
            ),
            setup_token: None,
            dangerously_skip_setup_token: false,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: crate::state::SetupClaimLimiter::new(),
        });
        // Leak the tempdir so the DB stays alive during the test
        std::mem::forget(temp);
        let _ = info;
        (state, raw_token)
    }

    #[actix_web::test]
    async fn test_valid_bearer_token() {
        let (state, raw_token) = test_app_state(false).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/ping")
            .insert_header(("Authorization", format!("Bearer {}", raw_token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn test_missing_auth_header() {
        let (state, _) = test_app_state(false).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get().uri("/api/ping").to_request();
        let resp = actix_test::try_call_service(&app, req).await;
        assert!(resp.is_err());
    }

    #[actix_web::test]
    async fn test_wrong_bearer_token() {
        let (state, _) = test_app_state(false).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/ping")
            .insert_header(("Authorization", "Bearer wrong-token"))
            .to_request();
        let resp = actix_test::try_call_service(&app, req).await;
        assert!(resp.is_err());
    }

    #[actix_web::test]
    async fn test_revoked_token_rejected() {
        let (state, raw_token) = test_app_state(false).await;

        // Get the token ID and revoke it
        let core = state.manager.active_core().await.unwrap();
        core.create_api_token("replacement").await.unwrap();
        let tokens = core.list_api_tokens().await.unwrap();
        let token_id = &tokens.iter().find(|t| t.name == "test-token").unwrap().id;
        core.revoke_api_token(token_id).await.unwrap();

        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/ping")
            .insert_header(("Authorization", format!("Bearer {}", raw_token)))
            .to_request();
        let resp = actix_test::try_call_service(&app, req).await;
        assert!(resp.is_err());
    }

    #[actix_web::test]
    async fn integrated_mode_blocks_local_route_after_authentication() {
        let (state, raw_token) = test_app_state(true).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/wiki", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let request = actix_test::TestRequest::get()
            .uri("/api/wiki")
            .insert_header(("Authorization", format!("Bearer {raw_token}")))
            .to_request();
        let error = actix_test::try_call_service(&app, request)
            .await
            .expect_err("local route must be blocked");
        assert_eq!(
            error.as_response_error().status_code(),
            actix_web::http::StatusCode::CONFLICT
        );
    }
}
