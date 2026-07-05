//! Search routes

use crate::db_extractor::Db;
use crate::error::{ApiErrorResponse, ok_or_error};
use crate::routes::memu_proxy;
use crate::state::AppState;
use actix_web::{HttpResponse, web};
use atomic_core::{SearchMode, SearchOptions, SemanticSearchResult, SimilarAtomResult};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SearchRequest {
    /// Search query text
    pub query: String,
    /// Search mode: "keyword", "semantic", or "hybrid"
    pub mode: String,
    /// Max results (default: 20)
    pub limit: Option<i32>,
    /// Minimum similarity threshold
    pub threshold: Option<f32>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct GlobalSearchRequest {
    /// Search query text
    pub query: String,
    /// Per-section result cap (default: 5)
    pub section_limit: Option<i32>,
}

#[utoipa::path(
    post,
    path = "/api/search",
    request_body = SearchRequest,
    responses(
        (status = 200, description = "Search results", body = Vec<SemanticSearchResult>),
        (status = 400, description = "Invalid search mode", body = ApiErrorResponse),
    ),
    tag = "search",
)]
pub async fn search(
    state: web::Data<AppState>,
    db: Db,
    body: web::Json<SearchRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    if let Some(config) = state.memu_session.clone() {
        return memu_search(config, &req.query, req.limit.unwrap_or(20)).await;
    }
    let mode = match req.mode.as_str() {
        "keyword" => SearchMode::Keyword,
        "semantic" => SearchMode::Semantic,
        "hybrid" => SearchMode::Hybrid,
        _ => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Invalid search mode. Use 'keyword', 'semantic', or 'hybrid'."
            }));
        }
    };

    let mut options = SearchOptions::new(req.query, mode, req.limit.unwrap_or(20));
    if let Some(threshold) = req.threshold {
        options = options.with_threshold(threshold);
    }

    let result = db.0.search(options).await;
    ok_or_error(result)
}

#[utoipa::path(
    post,
    path = "/api/search/global",
    request_body = GlobalSearchRequest,
    responses(
        (status = 200, description = "Grouped global search results", body = atomic_core::GlobalSearchResponse),
    ),
    tag = "search",
)]
pub async fn global_search(
    state: web::Data<AppState>,
    db: Db,
    body: web::Json<GlobalSearchRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    if let Some(config) = state.memu_session.clone() {
        let atoms =
            match memu_search_value(config, &req.query, req.section_limit.unwrap_or(5)).await {
                Ok(atoms) => atoms,
                Err(response) => return response,
            };
        return HttpResponse::Ok().json(serde_json::json!({
            "atoms": atoms,
            "wiki": [],
            "chats": [],
            "tags": [],
        }));
    }
    ok_or_error(
        db.0.search_global_keyword(&req.query, req.section_limit.unwrap_or(5))
            .await,
    )
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FindSimilarQuery {
    /// Max results (default: 10)
    pub limit: Option<i32>,
    /// Minimum similarity threshold (default: 0.7)
    pub threshold: Option<f32>,
}

#[utoipa::path(
    get,
    path = "/api/atoms/{id}/similar",
    params(
        ("id" = String, Path, description = "Atom ID"),
        FindSimilarQuery,
    ),
    responses(
        (status = 200, description = "Similar atoms", body = Vec<SimilarAtomResult>),
    ),
    tag = "search",
)]
pub async fn find_similar(
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    query: web::Query<FindSimilarQuery>,
) -> HttpResponse {
    let atom_id = path.into_inner();
    if state.memu_session.is_some() && memu_proxy::is_memu_id(&atom_id) {
        return HttpResponse::Ok().json(Vec::<serde_json::Value>::new());
    }
    let limit = query.limit.unwrap_or(10);
    let threshold = query.threshold.unwrap_or(0.7);
    ok_or_error(db.0.find_similar(&atom_id, limit, threshold).await)
}

async fn memu_search(
    config: crate::state::MemuSessionConfig,
    query: &str,
    limit: i32,
) -> HttpResponse {
    match memu_search_value(config, query, limit).await {
        Ok(body) => HttpResponse::Ok().json(body),
        Err(response) => response,
    }
}

async fn memu_search_value(
    config: crate::state::MemuSessionConfig,
    query: &str,
    limit: i32,
) -> Result<Vec<serde_json::Value>, HttpResponse> {
    let client = memu_proxy::client()?;
    let params = vec![
        ("q", query.to_string()),
        ("user_id", config.user_id),
        ("soul_id", config.soul_id),
        ("limit", limit.max(1).to_string()),
    ];
    let body = memu_proxy::memu_json(
        client
            .get(format!("{}/integration/atomic/search", config.base_url))
            .query(&params),
        "memU search",
    )
    .await?;
    Ok(body["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|node| {
            let mut atom = memu_proxy::atom_from_node(node);
            if let Some(map) = atom.as_object_mut() {
                map.insert(
                    "similarity_score".to_string(),
                    serde_json::json!(node["score"].as_f64().unwrap_or(1.0) as f32),
                );
                map.insert(
                    "matching_chunk_content".to_string(),
                    serde_json::json!(node["summary"].as_str().unwrap_or_default()),
                );
                map.insert("matching_chunk_index".to_string(), serde_json::json!(0));
            }
            atom
        })
        .collect())
}
