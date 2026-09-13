//! Database management routes

use crate::error::ApiErrorResponse;
use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[utoipa::path(get, path = "/api/databases", responses((status = 200, description = "List of databases with active ID")), tag = "databases")]
pub async fn list_databases(request: HttpRequest, state: web::Data<AppState>) -> HttpResponse {
    if state.memu_session.is_some() {
        let (_, database_id) = match state.resolve_core_with_id(&request).await {
            Ok(value) => value,
            Err(error) => return crate::error::error_response(error),
        };
        return match state.manager.list_databases().await {
            Ok((databases, _)) => HttpResponse::Ok().json(serde_json::json!({
                "databases": databases.into_iter().filter(|db| db.id == database_id).collect::<Vec<_>>(),
                "active_id": database_id,
            })),
            Err(error) => crate::error::error_response(error),
        };
    }
    match state.manager.list_databases().await {
        Ok((databases, active_id)) => HttpResponse::Ok().json(serde_json::json!({
            "databases": databases,
            "active_id": active_id,
        })),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateDatabaseBody {
    /// Name for the new database
    pub name: String,
}

#[utoipa::path(post, path = "/api/databases", request_body = CreateDatabaseBody, responses((status = 201, description = "Database created", body = atomic_core::DatabaseInfo)), tag = "databases")]
pub async fn create_database(
    state: web::Data<AppState>,
    body: web::Json<CreateDatabaseBody>,
) -> HttpResponse {
    let name = body.into_inner().name;
    match state.manager.create_database(&name).await {
        Ok(info) => HttpResponse::Created().json(info),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct RenameDatabaseBody {
    /// New name for the database
    pub name: String,
}

#[utoipa::path(put, path = "/api/databases/{id}", params(("id" = String, Path, description = "Database ID")), request_body = RenameDatabaseBody, responses((status = 200, description = "Database renamed"), (status = 404, description = "Database not found", body = ApiErrorResponse)), tag = "databases")]
pub async fn rename_database(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<RenameDatabaseBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let name = body.into_inner().name;
    match state.manager.rename_database(&id, &name).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"renamed": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(delete, path = "/api/databases/{id}", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Database deleted"), (status = 400, description = "Cannot delete default database", body = ApiErrorResponse)), tag = "databases")]
pub async fn delete_database(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    match state.manager.delete_database(&id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"deleted": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(put, path = "/api/databases/{id}/activate", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Database activated")), tag = "databases")]
pub async fn activate_database(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    match state.manager.set_active(&id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"activated": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(put, path = "/api/databases/{id}/default", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Default database changed")), tag = "databases")]
pub async fn set_default_database(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    match state.manager.set_default_database(&id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"set_default": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(get, path = "/api/databases/{id}/stats", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Database statistics")), tag = "databases")]
pub async fn database_stats(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    match state.manager.get_core(&id).await {
        Ok(core) => {
            let atom_count = core.count_atoms().await.unwrap_or(0);
            HttpResponse::Ok().json(serde_json::json!({
                "atom_count": atom_count,
            }))
        }
        Err(e) => crate::error::error_response(e),
    }
}
