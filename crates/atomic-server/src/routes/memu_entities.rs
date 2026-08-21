use crate::routes::memu_proxy::{client, memu_json, memu_url, scope_query, session};
use crate::routes::memu_reviews::updated_atom_response;
use crate::state::AppState;
use actix_web::{web, HttpResponse};
use serde_json::{json, Value};

pub async fn list(state: web::Data<AppState>) -> HttpResponse {
    proxy(state, None).await
}

pub async fn create(state: web::Data<AppState>, body: web::Json<Value>) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let url = match memu_url(&config.base_url, &["integration", "atomic", "entities"]) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client
            .post(url)
            .query(&scope_query(&config))
            .json(&body.into_inner()),
        "memU entity create",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn detail(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    proxy(state, Some(path.into_inner())).await
}

async fn proxy(state: web::Data<AppState>, entity_id: Option<String>) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let mut segments = vec!["integration", "atomic", "entities"];
    if let Some(ref entity_id) = entity_id {
        segments.push(entity_id);
    }
    let url = match memu_url(&config.base_url, &segments) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client.get(url).query(&scope_query(&config)),
        "memU entities",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn update(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<Value>,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let entity_id = path.into_inner();
    let url = match memu_url(
        &config.base_url,
        &["integration", "atomic", "entities", &entity_id],
    ) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client
            .patch(url)
            .query(&scope_query(&config))
            .json(&body.into_inner()),
        "memU entity update",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

async fn entity_action(
    state: web::Data<AppState>,
    entity_id: String,
    action: Option<&str>,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let mut segments = vec!["integration", "atomic", "entities", &entity_id];
    if let Some(action) = action {
        segments.push(action);
    }
    let url = match memu_url(&config.base_url, &segments) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let request = if action.is_some() {
        client.post(url)
    } else {
        client.delete(url)
    };
    match memu_json(
        request.query(&scope_query(&config)),
        "memU entity action",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn ignore(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    entity_action(state, path.into_inner(), Some("ignore")).await
}

pub async fn restore(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    entity_action(state, path.into_inner(), Some("restore")).await
}

pub async fn delete(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    entity_action(state, path.into_inner(), None).await
}

pub async fn merge_preview(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> HttpResponse {
    let Some(duplicate_id) = query.get("duplicate_entity_id") else {
        return HttpResponse::BadRequest()
            .json(json!({"error": "duplicate_entity_id is required"}));
    };
    merge_proxy(state, path.into_inner(), duplicate_id.clone(), false).await
}

pub async fn merge(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<Value>,
) -> HttpResponse {
    let duplicate_id = body
        .get("duplicate_entity_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if duplicate_id.is_empty() {
        return HttpResponse::BadRequest()
            .json(json!({"error": "duplicate_entity_id is required"}));
    }
    merge_proxy(state, path.into_inner(), duplicate_id.to_owned(), true).await
}

async fn merge_proxy(
    state: web::Data<AppState>,
    entity_id: String,
    duplicate_id: String,
    commit: bool,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let action = if commit { "merge" } else { "merge-preview" };
    let url = match memu_url(
        &config.base_url,
        &["integration", "atomic", "entities", &entity_id, action],
    ) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let request = if commit {
        client
            .post(url)
            .query(&scope_query(&config))
            .json(&json!({"duplicate_entity_id": duplicate_id}))
    } else {
        client.get(url).query(&[
            ("user_id", config.user_id.as_str()),
            ("soul_id", config.soul_id.as_str()),
            ("duplicate_entity_id", duplicate_id.as_str()),
        ])
    };
    match memu_json(request, "memU entity merge").await {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

async fn set_memory_entity(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    attached: bool,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let (memory_id, entity_id) = path.into_inner();
    let url = match memu_url(
        &config.base_url,
        &[
            "integration",
            "atomic",
            "memories",
            &memory_id,
            "entities",
            &entity_id,
        ],
    ) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let request = if attached {
        client.put(url)
    } else {
        client.delete(url)
    }
    .query(&scope_query(&config));
    match memu_json(request, "memU entity assignment").await {
        Ok(body) => updated_atom_response(&state, body),
        Err(response) => response,
    }
}

pub async fn attach(state: web::Data<AppState>, path: web::Path<(String, String)>) -> HttpResponse {
    set_memory_entity(state, path, true).await
}

pub async fn detach(state: web::Data<AppState>, path: web::Path<(String, String)>) -> HttpResponse {
    set_memory_entity(state, path, false).await
}

pub async fn promote_relationship(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<Value>,
) -> HttpResponse {
    relationship_write(state, path.into_inner(), body.into_inner(), false).await
}

pub async fn update_relationship(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<Value>,
) -> HttpResponse {
    relationship_write(state, path.into_inner(), body.into_inner(), true).await
}

async fn relationship_write(
    state: web::Data<AppState>,
    entity_id: String,
    mut body: Value,
    update: bool,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let Some(payload) = body.as_object_mut() else {
        return HttpResponse::BadRequest().json(json!({"error": "object body required"}));
    };
    payload.insert("user_id".into(), config.user_id.clone().into());
    if !update {
        payload.insert("entity_id".into(), entity_id.clone().into());
    }
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let speaker_id = format!("entity:{entity_id}");
    let url = if update {
        memu_url(
            &config.base_url,
            &["souls", &config.soul_id, "relationships", &speaker_id],
        )
    } else {
        memu_url(
            &config.base_url,
            &["souls", &config.soul_id, "relationships"],
        )
    };
    let url = match url {
        Ok(url) => url,
        Err(response) => return response,
    };
    let request = if update {
        client.patch(url).json(&body)
    } else {
        client.post(url).json(&body)
    };
    match memu_json(request, "memU relationship update").await {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

pub async fn remove_relationship(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let config = match session(&state) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client = match client() {
        Ok(client) => client,
        Err(response) => return response,
    };
    let speaker_id = format!("entity:{}", path.into_inner());
    let url = match memu_url(
        &config.base_url,
        &["souls", &config.soul_id, "relationships", &speaker_id],
    ) {
        Ok(url) => url,
        Err(response) => return response,
    };
    match memu_json(
        client
            .delete(url)
            .query(&[("user_id", config.user_id.as_str())]),
        "memU relationship remove",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}
