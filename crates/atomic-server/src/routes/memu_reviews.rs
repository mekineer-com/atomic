use crate::routes::memu_proxy::{self, client, memu_json, memu_url, session};
use crate::state::{AppState, ServerEvent};
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Deserialize, Serialize)]
pub struct SummaryUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summaries_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displayed_summary: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct SummaryGuard {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summaries_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displayed_summary: Option<String>,
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

pub async fn approve_category(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<SummaryGuard>,
) -> HttpResponse {
    approve_summary(
        state,
        "category",
        &path.into_inner(),
        body.into_inner(),
        true,
    )
    .await
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
    let url = match memu_url(&config.base_url, &[kind, id, "approve"]) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client.post(url).query(&[
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

pub(super) fn updated_atom_response(state: &AppState, body: Value) -> HttpResponse {
    let revision = body.get("summaries_revision").cloned();
    let mut atom_value = memu_proxy::atom_from_node(&body);
    if memu_proxy::is_memu_id(body["id"].as_str().unwrap_or_default()) {
        if let Ok(atom) = serde_json::from_value::<atomic_core::AtomWithTags>(atom_value.clone()) {
            let _ = state.event_tx.send(ServerEvent::AtomUpdated { atom });
        }
    }
    if let (Some(revision), Some(response)) = (revision, atom_value.as_object_mut()) {
        response.insert("summaries_revision".into(), revision);
    }
    HttpResponse::Ok().json(atom_value)
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
    if (kind == "memory" && body.summary.is_none())
        || (kind == "category"
            && body.summary.is_none()
            && body.title.is_none()
            && body.description.is_none())
    {
        return HttpResponse::BadRequest().json(json!({"error": "no changes supplied"}));
    }
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let url = match memu_url(&config.base_url, &[kind, id]) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let mut payload = json!({
        "approved": true,
        "edited_by": "atomic:user",
    });
    if let Some(summary) = body.summary {
        payload["summary"] = summary.into();
    }
    if let Some(title) = body.title {
        payload["title"] = title.into();
    }
    if let Some(description) = body.description {
        payload["description"] = description.into();
    }
    if let Some(revision) = body.summaries_revision {
        payload["summaries_revision"] = revision.into();
    }
    if let Some(displayed) = body.displayed_summary {
        payload["displayed_summary"] = displayed.into();
    }
    match memu_json(
        client
            .patch(url)
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ])
            .json(&payload),
        "memU update",
    )
    .await
    {
        Ok(body) => updated_atom_response(&state, body),
        Err(response) => response,
    }
}

pub async fn update_soul_summary(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<SummaryUpdate>,
) -> HttpResponse {
    let body = body.into_inner();
    if body.summary.is_none() {
        return HttpResponse::BadRequest().json(json!({"error": "summary is required"}));
    }
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let summary_id = path.into_inner();
    let url = match memu_url(&config.base_url, &["soul-summary", &summary_id]) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client
            .patch(url)
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ])
            .json(&body),
        "memU soul summary update",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn approve_soul_summary(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<SummaryGuard>,
) -> HttpResponse {
    approve_summary(
        state,
        "soul-summary",
        &path.into_inner(),
        body.into_inner(),
        false,
    )
    .await
}

async fn approve_summary(
    state: web::Data<AppState>,
    kind: &str,
    id: &str,
    body: SummaryGuard,
    atom_response: bool,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let url = match memu_url(&config.base_url, &[kind, id, "approve"]) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client
            .post(url)
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ])
            .json(&body),
        "memU summary approve",
    )
    .await
    {
        Ok(body) if atom_response => updated_atom_response(&state, body),
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
    let memory_id = path.into_inner();
    let url = match memu_url(&config.base_url, &["memory", &memory_id]) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client.delete(url).query(&[
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
