use crate::state::{AppState, MemuSessionConfig};
use actix_web::{http::StatusCode, HttpResponse};
use serde_json::{json, Value};
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
        return Err(HttpResponse::build(status).json(json!({"error": memu_error_value(&body)})));
    }
    response.json::<Value>().await.map_err(|e| {
        HttpResponse::BadGateway()
            .json(json!({"error": format!("{error_prefix} returned invalid JSON: {e}")}))
    })
}

pub fn memu_url(base_url: &str, segments: &[&str]) -> Result<reqwest::Url, HttpResponse> {
    let mut url = reqwest::Url::parse(base_url).map_err(|e| {
        HttpResponse::InternalServerError()
            .json(json!({"error": format!("invalid memU server URL: {e}")}))
    })?;
    let mut path = url.path_segments_mut().map_err(|_| {
        HttpResponse::InternalServerError()
            .json(json!({"error": "memU server URL cannot be a base"}))
    })?;
    path.pop_if_empty();
    path.extend(segments);
    drop(path);
    Ok(url)
}

pub fn memu_error_text(body: &str) -> String {
    match memu_error_value(body) {
        Value::String(message) => message,
        value => value.to_string(),
    }
}

fn memu_error_value(body: &str) -> Value {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Value::String(body.to_string());
    };
    value
        .get("error")
        .or_else(|| value.get("detail"))
        .cloned()
        .unwrap_or_else(|| Value::String(body.to_string()))
}

pub fn is_memu_id(id: &str) -> bool {
    id.starts_with("memory:") || id.starts_with("category:") || id.starts_with("entity:")
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
    let entity_ids = node
        .get("entity_ids")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!([]));
    let entity_names = node
        .get("entity_names")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!([]));
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
        "approved_at": node["approved_at"].as_str(),
        "description": node["description"].clone(),
        "approved_description": node["approved_description"].clone(),
        "approved_summary": node["approved_summary"].as_str(),
        "category_kind": node["category_kind"].clone(),
        "lore_subtype": node["lore_subtype"].clone(),
        "anchor_role": node["anchor_role"].clone(),
        "active": node["active"].clone(),
        "last_evidence_at": node["last_evidence_at"].clone(),
        "last_revised_at": node["last_revised_at"].clone(),
        "citations": node["citations"].clone(),
        "entity_ids": entity_ids,
        "entity_names": entity_names,
        "summaries_revision": node["summaries_revision"].clone(),
        "embedding_status": "complete",
        "tagging_status": "skipped",
        "embedding_error": null,
        "tagging_error": null,
        "kind": "captured",
        "tags": tags,
    })
}

pub fn readonly() -> HttpResponse {
    HttpResponse::Conflict().json(json!({"error": "local Atomic writes are disabled in memU mode"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memu_boundary_encodes_segments_and_normalizes_entity_arrays() {
        let Ok(url) = memu_url("http://localhost:8099/root/", &["entity:a/b?c"]) else {
            panic!("valid base URL rejected");
        };
        assert_eq!(url.path(), "/root/entity:a%2Fb%3Fc");

        let atom = atom_from_node(&json!({"id": "memory:m1", "entity_ids": null}));
        assert_eq!(atom["entity_ids"], json!([]));
        assert_eq!(atom["entity_names"], json!([]));
    }
}
