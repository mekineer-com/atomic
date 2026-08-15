use crate::routes::memu_proxy::{client, memu_json, scope_query, session};
use crate::state::AppState;
use actix_web::{web, HttpResponse};

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
