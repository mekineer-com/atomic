use crate::routes::memu_proxy::{self, client, memu_json, session};
use crate::state::{AppState, ServerEvent};
use actix_web::{web, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
pub struct SummaryUpdate {
    pub summary: String,
}

pub async fn status(state: web::Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(json!({"enabled": state.memu_session.is_some()}))
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
        Ok(body) => updated_atom_response(&state, body),
        Err(response) => response,
    }
}

fn updated_atom_response(state: &AppState, body: Value) -> HttpResponse {
    if let Ok(atom) =
        serde_json::from_value::<atomic_core::AtomWithTags>(memu_proxy::atom_from_node(&body))
    {
        let _ = state.event_tx.send(ServerEvent::AtomUpdated { atom });
    }
    HttpResponse::Ok().json(body)
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
        Ok(body) => updated_atom_response(&state, body),
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
