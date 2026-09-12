//! Database resolution extractor for actix-web.
//!
//! `Db` is a `FromRequest` extractor that resolves the correct `AtomicCore`
//! from the request (via `X-Atomic-Database` header, `?db=` param, or active db).

use crate::state::AppState;
use actix_web::{web, FromRequest, HttpRequest};
use atomic_core::AtomicCore;

/// Extractor that resolves the correct AtomicCore for the current request.
pub struct Db(pub AtomicCore, pub String);

impl FromRequest for Db {
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut actix_web::dev::Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let state = req.app_data::<web::Data<AppState>>().ok_or_else(|| {
                actix_web::error::ErrorInternalServerError("AppState not configured")
            })?;
            state
                .resolve_core_with_id(&req)
                .await
                .map(|(core, id)| Db(core, id))
                .map_err(|e| {
                    actix_web::error::ErrorBadRequest(format!("Database not found: {}", e))
                })
        })
    }
}
