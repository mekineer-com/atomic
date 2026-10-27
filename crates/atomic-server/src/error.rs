//! API error handling — maps AtomicCoreError to HTTP responses

use actix_web::HttpResponse;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiErrorResponse {
    pub error: String,
}

/// Transport-independent status and JSON body for background work.
pub type JsonError = (actix_web::http::StatusCode, serde_json::Value);

pub fn core_json_error(error: atomic_core::AtomicCoreError) -> JsonError {
    (
        status_code_for(&error),
        serde_json::json!({"error": error.to_string()}),
    )
}

pub fn json_error_response((status, body): JsonError) -> HttpResponse {
    HttpResponse::build(status).json(body)
}

pub fn error_response(error: atomic_core::AtomicCoreError) -> HttpResponse {
    json_error_response(core_json_error(error))
}

/// Shorthand to convert Result<T, AtomicCoreError> to HttpResponse
pub fn ok_or_error<T: serde::Serialize>(
    result: Result<T, atomic_core::AtomicCoreError>,
) -> HttpResponse {
    match result {
        Ok(data) => HttpResponse::Ok().json(data),
        Err(e) => error_response(e),
    }
}

/// Get the HTTP status code for an AtomicCoreError
pub fn status_code_for(e: &atomic_core::AtomicCoreError) -> actix_web::http::StatusCode {
    use actix_web::http::StatusCode;
    match e {
        atomic_core::AtomicCoreError::NotFound(_) => StatusCode::NOT_FOUND,
        atomic_core::AtomicCoreError::Validation(_) => StatusCode::BAD_REQUEST,
        atomic_core::AtomicCoreError::Configuration(_) => StatusCode::BAD_REQUEST,
        atomic_core::AtomicCoreError::Conflict(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::StatusCode;
    use atomic_core::AtomicCoreError;

    #[actix_web::test]
    async fn structured_json_error_survives_http_boundary() {
        let body = serde_json::json!({"error": {"code": "soul_paused", "message": "Retry later"}});
        let response = json_error_response((StatusCode::CONFLICT, body.clone()));
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            body
        );
    }

    #[test]
    fn test_not_found_maps_to_404() {
        assert_eq!(
            status_code_for(&AtomicCoreError::NotFound("atom xyz".into())),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn test_validation_maps_to_400() {
        assert_eq!(
            status_code_for(&AtomicCoreError::Validation("bad input".into())),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_configuration_maps_to_400() {
        assert_eq!(
            status_code_for(&AtomicCoreError::Configuration("missing key".into())),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_database_error_maps_to_500() {
        assert_eq!(
            status_code_for(&AtomicCoreError::DatabaseOperation("db broke".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn test_lock_error_maps_to_500() {
        assert_eq!(
            status_code_for(&AtomicCoreError::Lock("poisoned".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn test_ok_or_error_success() {
        let resp = ok_or_error(Ok::<_, AtomicCoreError>(serde_json::json!({"key": "val"})));
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_ok_or_error_failure() {
        let resp = ok_or_error(Err::<serde_json::Value, _>(AtomicCoreError::NotFound(
            "nope".into(),
        )));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
