use crate::routes::memu_proxy::{client, memu_json, scope_query, session};
use crate::routes::memu_reviews::updated_atom_response;
use crate::state::AppState;
use actix_web::{HttpResponse, web};
use serde_json::{Value, json};

pub async fn list(state: web::Data<AppState>) -> HttpResponse {
    proxy(state, None).await
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
    let mut url = format!("{}/integration/atomic/entities", config.base_url);
    if let Some(entity_id) = entity_id {
        url.push('/');
        url.push_str(&entity_id);
    }
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
    match memu_json(
        client
            .patch(format!(
                "{}/integration/atomic/entities/{}",
                config.base_url,
                path.into_inner()
            ))
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
    let url = format!(
        "{}/integration/atomic/memories/{memory_id}/entities/{entity_id}",
        config.base_url
    );
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
    let url = if update {
        format!(
            "{}/souls/{}/relationships/entity:{}",
            config.base_url, config.soul_id, entity_id
        )
    } else {
        format!("{}/souls/{}/relationships", config.base_url, config.soul_id)
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

pub async fn deactivate_relationship(
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
    match memu_json(
        client
            .delete(format!(
                "{}/souls/{}/relationships/entity:{}",
                config.base_url,
                config.soul_id,
                path.into_inner()
            ))
            .query(&[("user_id", config.user_id.as_str())]),
        "memU relationship deactivate",
    )
    .await
    {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}
