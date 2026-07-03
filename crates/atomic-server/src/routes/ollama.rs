//! Ollama and provider routes

use crate::{db_extractor::Db, state::AppState};
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, Serialize, ToSchema)]
pub struct TestOllamaBody {
    /// Ollama server host URL
    pub host: String,
}

#[utoipa::path(post, path = "/api/ollama/test", request_body = TestOllamaBody, responses((status = 200, description = "Connection test result")), tag = "providers")]
pub async fn test_ollama(body: web::Json<TestOllamaBody>) -> HttpResponse {
    match atomic_core::providers::models::test_ollama_connection(&body.host).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({"success": true})),
        Ok(false) => HttpResponse::Ok().json(serde_json::json!({"success": false})),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OllamaHostQuery {
    /// Ollama host URL (default: http://127.0.0.1:11434)
    pub host: Option<String>,
}

#[utoipa::path(get, path = "/api/ollama/models", params(OllamaHostQuery), responses((status = 200, description = "All Ollama models")), tag = "providers")]
pub async fn get_ollama_models(query: web::Query<OllamaHostQuery>) -> HttpResponse {
    let host = query.host.as_deref().unwrap_or("http://127.0.0.1:11434");
    match atomic_core::providers::models::fetch_ollama_models(host).await {
        Ok(models) => HttpResponse::Ok().json(models),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[utoipa::path(get, path = "/api/ollama/embedding-models", params(OllamaHostQuery), responses((status = 200, description = "Ollama embedding models")), tag = "providers")]
pub async fn get_ollama_embedding_models(query: web::Query<OllamaHostQuery>) -> HttpResponse {
    let host = query.host.as_deref().unwrap_or("http://127.0.0.1:11434");
    match atomic_core::providers::models::get_ollama_embedding_models(host).await {
        Ok(models) => HttpResponse::Ok().json(models),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[utoipa::path(get, path = "/api/ollama/llm-models", params(OllamaHostQuery), responses((status = 200, description = "Ollama LLM models")), tag = "providers")]
pub async fn get_ollama_llm_models(query: web::Query<OllamaHostQuery>) -> HttpResponse {
    let host = query.host.as_deref().unwrap_or("http://127.0.0.1:11434");
    match atomic_core::providers::models::get_ollama_llm_models(host).await {
        Ok(models) => HttpResponse::Ok().json(models),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[utoipa::path(get, path = "/api/provider/verify", responses((status = 200, description = "Whether an AI provider is configured")), tag = "providers")]
pub async fn verify_provider_configured(state: web::Data<AppState>, db: Db) -> HttpResponse {
    if state.memu_session.is_some() {
        return HttpResponse::Ok().json(serde_json::json!({"configured": true}));
    }

    let settings = match db.0.get_settings().await {
        Ok(s) => s,
        Err(e) => return crate::error::error_response(e),
    };

    let config = atomic_core::ProviderConfig::from_settings(&settings);

    let configured = match config.provider_type {
        atomic_core::ProviderType::OpenRouter => config
            .openrouter_api_key
            .as_ref()
            .map(|k| !k.is_empty())
            .unwrap_or(false),
        atomic_core::ProviderType::Ollama => !config.ollama_host.is_empty(),
        atomic_core::ProviderType::OpenAICompat => !config.openai_compat_base_url.is_empty(),
    };

    HttpResponse::Ok().json(serde_json::json!({"configured": configured}))
}
