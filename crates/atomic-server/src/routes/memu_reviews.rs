use crate::state::{AppState, MemuSessionConfig};
use actix_web::{http::StatusCode, web, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Deserialize)]
pub struct SummaryUpdate {
    pub summary: String,
}

pub async fn status(state: web::Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(json!({"enabled": state.memu_session.is_some()}))
}

fn session(state: &AppState) -> Result<MemuSessionConfig, HttpResponse> {
    state.memu_session.clone().ok_or_else(|| {
        HttpResponse::InternalServerError().json(json!({
            "error": "MEMU_SERVER_URL, MEMU_USER_ID, and MEMU_SOUL_ID are required"
        }))
    })
}

async fn memu_json(
    request: reqwest::RequestBuilder,
    error_prefix: &str,
) -> Result<Value, HttpResponse> {
    let response = request.send().await.map_err(|e| {
        HttpResponse::BadGateway()
            .json(json!({"error": format!("{error_prefix} request failed: {e}")}))
    })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return Err(HttpResponse::build(status).json(json!({"error": body})));
    }
    response.json::<Value>().await.map_err(|e| {
        HttpResponse::BadGateway()
            .json(json!({"error": format!("{error_prefix} returned invalid JSON: {e}")}))
    })
}

fn client() -> Result<reqwest::Client, HttpResponse> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| {
            HttpResponse::InternalServerError()
                .json(json!({"error": format!("memU client failed: {e}")}))
        })
}

pub async fn list_pending(state: web::Data<AppState>) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    match memu_json(
        client.get(format!("{}/pending", config.base_url)).query(&[
            ("user_id", config.user_id.as_str()),
            ("soul_id", config.soul_id.as_str()),
        ]),
        "memU pending",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn approve_memory(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    approve(state, "memory", &path.into_inner()).await
}

pub async fn approve_category(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    approve(state, "category", &path.into_inner()).await
}

async fn approve(state: web::Data<AppState>, kind: &str, id: &str) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    match memu_json(
        client
            .post(format!("{}/{}/{}/approve", config.base_url, kind, id))
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ]),
        "memU approve",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn update_memory(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<SummaryUpdate>,
) -> HttpResponse {
    update(state, "memory", &path.into_inner(), body.into_inner()).await
}

pub async fn update_category(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<SummaryUpdate>,
) -> HttpResponse {
    update(state, "category", &path.into_inner(), body.into_inner()).await
}

async fn update(
    state: web::Data<AppState>,
    kind: &str,
    id: &str,
    body: SummaryUpdate,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    match memu_json(
        client
            .patch(format!("{}/{}/{}", config.base_url, kind, id))
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ])
            .json(&json!({
                "summary": body.summary,
                "approved": true,
                "edited_by": "atomic:user",
            })),
        "memU update",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn delete_memory(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    match memu_json(
        client
            .delete(format!("{}/memory/{}", config.base_url, path.into_inner()))
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ]),
        "memU delete",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}
