//! Semantic graph routes

use crate::db_extractor::Db;
use crate::error::ok_or_error;
use crate::routes::memu_proxy;
use crate::state::AppState;
use actix_web::{web, HttpResponse};
use serde::Deserialize;
use utoipa::IntoParams;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EdgesQuery {
    /// Minimum similarity score (default: 0.5)
    pub min_similarity: Option<f32>,
}

#[utoipa::path(get, path = "/api/graph/edges", params(EdgesQuery), responses((status = 200, description = "Semantic edges", body = Vec<atomic_core::SemanticEdge>)), tag = "graph")]
pub async fn get_semantic_edges(db: Db, query: web::Query<EdgesQuery>) -> HttpResponse {
    let min_similarity = query.min_similarity.unwrap_or(0.5);
    ok_or_error(db.0.get_semantic_edges(min_similarity).await)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct NeighborhoodQuery {
    /// Graph traversal depth (default: 1)
    pub depth: Option<i32>,
    /// Minimum similarity score (default: 0.5)
    pub min_similarity: Option<f32>,
}

#[utoipa::path(get, path = "/api/graph/neighborhood/{atom_id}", params(("atom_id" = String, Path, description = "Center atom ID"), NeighborhoodQuery), responses((status = 200, description = "Neighborhood graph", body = atomic_core::NeighborhoodGraph)), tag = "graph")]
pub async fn get_atom_neighborhood(
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<String>,
    query: web::Query<NeighborhoodQuery>,
) -> HttpResponse {
    let atom_id = path.into_inner();
    let depth = query.depth.unwrap_or(1);
    let min_similarity = query.min_similarity.unwrap_or(0.5);
    if let Some(config) = state.memu_session.clone() {
        if !memu_proxy::is_memu_id(&atom_id) {
            return HttpResponse::NotFound().json(serde_json::json!({"error": "memU atom not found"}));
        }
        let client = match memu_proxy::client() {
            Ok(client) => client,
            Err(response) => return response,
        };
        let mut params = vec![
            ("depth", depth.to_string()),
            ("min_similarity", min_similarity.to_string()),
        ];
        params.extend(
            memu_proxy::scope_query(&config)
                .into_iter()
                .map(|(key, value)| (key, value.to_string())),
        );
        let body = match memu_proxy::memu_json(
            client
                .get(format!(
                    "{}/integration/atomic/neighborhood/{}",
                    config.base_url, atom_id
                ))
                .query(&params),
            "memU neighborhood",
        )
        .await
        {
            Ok(body) => body,
            Err(response) => return response,
        };
        let atoms: Vec<serde_json::Value> = body["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|node| {
                let mut atom = memu_proxy::atom_from_node(node);
                if let Some(map) = atom.as_object_mut() {
                    map.insert(
                        "depth".to_string(),
                        serde_json::json!(node["depth"].as_i64().unwrap_or(1)),
                    );
                }
                atom
            })
            .collect();
        return HttpResponse::Ok().json(serde_json::json!({
            "center_atom_id": body["center_atom_id"],
            "atoms": atoms,
            "edges": body["edges"].as_array().cloned().unwrap_or_default(),
        }));
    }
    ok_or_error(
        db.0.get_atom_neighborhood(&atom_id, depth, min_similarity)
            .await,
    )
}

/// Queue a full rebuild of the semantic-edge graph.
///
/// Returns the number of atoms **queued** for edge recomputation, not the
/// number of edges written. The actual edge computation runs asynchronously
/// on the background pipeline; this endpoint returns as soon as the work
/// is spawned. Clients that need completion should subscribe to pipeline
/// events over WebSocket.
#[utoipa::path(post, path = "/api/graph/rebuild-edges", responses((status = 200, description = "Edge rebuild queued; returns the number of atoms queued for recomputation")), tag = "graph")]
pub async fn rebuild_semantic_edges(db: Db) -> HttpResponse {
    ok_or_error(db.0.rebuild_semantic_edges().await)
}
