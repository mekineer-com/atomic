use crate::state::{AppState, MemuSessionConfig};
use actix_web::{HttpResponse, http::StatusCode};
use serde_json::{Value, json};
use std::time::Duration;

pub fn session(state: &AppState) -> Result<MemuSessionConfig, HttpResponse> {
    state.memu_session.clone().ok_or_else(|| {
        HttpResponse::InternalServerError().json(json!({
            "error": "MEMU_SERVER_URL, MEMU_USER_ID, and MEMU_SOUL_ID are required"
        }))
    })
}

pub fn client() -> Result<reqwest::Client, HttpResponse> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| {
            HttpResponse::InternalServerError()
                .json(json!({"error": format!("memU client failed: {e}")}))
        })
}

pub async fn memu_json(
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

pub fn is_memu_id(id: &str) -> bool {
    id.starts_with("memory:") || id.starts_with("category:")
}

pub fn scope_query(config: &MemuSessionConfig) -> [(&str, &str); 2] {
    [
        ("user_id", config.user_id.as_str()),
        ("soul_id", config.soul_id.as_str()),
    ]
}

pub fn atom_from_node(node: &Value) -> Value {
    let id = node["id"].as_str().unwrap_or_default();
    let summary = node["summary"].as_str().unwrap_or_default();
    let timestamp = node["updated_at"]
        .as_str()
        .or_else(|| node["happened_at"].as_str())
        .or_else(|| node["created_at"].as_str())
        .unwrap_or("1970-01-01T00:00:00Z");
    let category_ids = node["category_ids"].as_array().into_iter().flatten();
    let category_names = node["category_names"].as_array().into_iter().flatten();
    let tags: Vec<Value> = category_ids
        .zip(category_names)
        .filter_map(|(id, name)| Some((id.as_str()?, name.as_str()?)))
        .map(|(id, name)| {
            json!({
                "id": format!("category:{id}"),
                "name": name,
                "parent_id": null,
                "created_at": "1970-01-01T00:00:00Z",
                "is_autotag_target": false,
                "autotag_description": "",
            })
        })
        .collect();
    json!({
        "id": id,
        "title": node["label"].as_str().unwrap_or(id),
        "content": summary,
        "snippet": summary,
        "source_url": null,
        "source": node["memory_type"].as_str().or_else(|| node["kind"].as_str()),
        "published_at": node["happened_at"].as_str(),
        "created_at": node["created_at"].as_str().unwrap_or(timestamp),
        "updated_at": timestamp,
        "embedding_status": "complete",
        "tagging_status": "skipped",
        "embedding_error": null,
        "tagging_error": null,
        "kind": "captured",
        "tags": tags,
    })
}

pub fn readonly() -> HttpResponse {
    HttpResponse::Conflict().json(json!({"error": "memU atoms are read-only in this slice"}))
}
