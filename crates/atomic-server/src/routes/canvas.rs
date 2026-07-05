//! Canvas position routes

use crate::db_extractor::Db;
use crate::error::ok_or_error;
use crate::routes::memu_proxy;
use crate::state::AppState;
use actix_web::{HttpResponse, web};
use atomic_core::{
    AtomPosition, CanvasAtomPosition, CanvasClusterLabel, CanvasEdgeData, GlobalCanvasData,
    projection,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};

#[utoipa::path(get, path = "/api/canvas/positions", responses((status = 200, description = "All atom positions", body = Vec<AtomPosition>)), tag = "canvas")]
pub async fn get_positions(db: Db) -> HttpResponse {
    ok_or_error(db.0.get_atom_positions().await)
}

#[utoipa::path(put, path = "/api/canvas/positions", request_body = Vec<AtomPosition>, responses((status = 200, description = "Positions saved")), tag = "canvas")]
pub async fn save_positions(
    state: web::Data<AppState>,
    db: Db,
    body: web::Json<Vec<AtomPosition>>,
) -> HttpResponse {
    let positions = body.into_inner();
    if state.memu_session.is_some() {
        return HttpResponse::Ok().json(serde_json::json!({"status": "ok"}));
    }
    match db.0.save_atom_positions(&positions).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"status": "ok"})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(get, path = "/api/canvas/atoms-with-embeddings", responses((status = 200, description = "Atoms with embedding vectors", body = Vec<atomic_core::AtomWithEmbedding>)), tag = "canvas")]
pub async fn get_atoms_with_embeddings(db: Db) -> HttpResponse {
    // Canvas displays every atom in the graph, including future report findings.
    ok_or_error(
        db.0.get_atoms_with_embeddings(&atomic_core::models::KindFilter::All)
            .await,
    )
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CanvasLevelQuery {
    /// Parent node ID (null for root level)
    pub parent_id: Option<String>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CanvasLevelBody {
    /// Hint for which children to include
    pub children_hint: Option<Vec<String>>,
}

#[utoipa::path(post, path = "/api/canvas/level", params(CanvasLevelQuery), request_body(content = Option<CanvasLevelBody>), responses((status = 200, description = "Canvas level data", body = atomic_core::CanvasLevel)), tag = "canvas")]
pub async fn get_canvas_level(
    db: Db,
    query: web::Query<CanvasLevelQuery>,
    body: Option<web::Json<CanvasLevelBody>>,
) -> HttpResponse {
    let parent_id = query.parent_id.clone();
    let children_hint = body.and_then(|b| b.into_inner().children_hint);
    ok_or_error(
        db.0.get_canvas_level(parent_id.as_deref(), children_hint)
            .await,
    )
}

/// Compute PCA 2D projection and return all atoms with positions, edges, and cluster labels
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GlobalCanvasQuery {
    /// If set, only atoms whose `source_url` starts with this prefix are returned.
    /// Use `obsidian://VaultName/` to filter to a specific vault.
    pub source_prefix: Option<String>,
}

#[derive(Deserialize)]
struct MemuCanvasSource {
    atoms: Vec<MemuCanvasAtom>,
    #[serde(default)]
    edges: Vec<CanvasEdgeData>,
}

#[derive(Deserialize)]
struct MemuCanvasAtom {
    id: String,
    title: String,
    embedding: Vec<f32>,
    primary_tag: Option<String>,
    tag_count: i32,
    tag_ids: Vec<String>,
    #[serde(default)]
    entity_ids: Vec<String>,
    #[serde(default)]
    entity_names: Vec<String>,
    source_url: Option<String>,
}

#[utoipa::path(get, path = "/api/canvas/global", params(GlobalCanvasQuery), responses((status = 200, description = "Global canvas data", body = atomic_core::GlobalCanvasData)), tag = "canvas")]
pub async fn get_global_canvas(
    state: web::Data<AppState>,
    db: Db,
    query: web::Query<GlobalCanvasQuery>,
) -> HttpResponse {
    if let Some(config) = state.memu_session.clone() {
        let client = match memu_proxy::client() {
            Ok(client) => client,
            Err(response) => return response,
        };
        let mut params = vec![("limit", "500".to_string())];
        params.extend(
            memu_proxy::scope_query(&config)
                .into_iter()
                .map(|(key, value)| (key, value.to_string())),
        );
        let body = match memu_proxy::memu_json(
            client
                .get(format!(
                    "{}/integration/atomic/canvas-source",
                    config.base_url
                ))
                .query(&params),
            "memU canvas source",
        )
        .await
        {
            Ok(body) => body,
            Err(response) => return response,
        };
        let source = match serde_json::from_value::<MemuCanvasSource>(body) {
            Ok(source) => source,
            Err(e) => {
                return HttpResponse::BadGateway().json(
                    serde_json::json!({"error": format!("memU canvas source shape failed: {e}")}),
                );
            }
        };
        return HttpResponse::Ok().json(memu_canvas_data(source));
    }

    let source_prefix = query.into_inner().source_prefix;

    match db.0.compute_and_get_canvas_data().await {
        Ok(data) => {
            if let Some(ref prefix) = source_prefix {
                HttpResponse::Ok().json(filter_canvas_by_source_prefix(&data, prefix))
            } else {
                HttpResponse::Ok().json(&*data)
            }
        }
        Err(e) => crate::error::error_response(e),
    }
}

fn memu_canvas_data(source: MemuCanvasSource) -> GlobalCanvasData {
    let embeddings: Vec<(String, Vec<f32>)> = source
        .atoms
        .iter()
        .map(|atom| (atom.id.clone(), atom.embedding.clone()))
        .collect();
    let positions: std::collections::HashMap<String, (f64, f64)> =
        projection::compute_2d_projection(&embeddings)
            .into_iter()
            .map(|(id, x, y)| (id, (x, y)))
            .collect();
    let similarity_edges = memu_similarity_edges(&source.atoms);
    let atoms = source
        .atoms
        .into_iter()
        .filter_map(|atom| {
            let (x, y) = positions.get(&atom.id)?;
            Some(CanvasAtomPosition {
                atom_id: atom.id,
                x: *x,
                y: *y,
                title: atom.title,
                primary_tag: atom.primary_tag,
                tag_count: atom.tag_count,
                tag_ids: atom.tag_ids,
                entity_ids: atom.entity_ids,
                entity_names: atom.entity_names,
                source_url: atom.source_url,
            })
        })
        .collect();
    let mut edges = source.edges;
    merge_edges(&mut edges, similarity_edges);
    GlobalCanvasData {
        atoms,
        edges,
        clusters: vec![],
    }
}

fn memu_similarity_edges(atoms: &[MemuCanvasAtom]) -> Vec<CanvasEdgeData> {
    let mut scored = Vec::new();
    for (idx, left) in atoms.iter().enumerate() {
        for right in atoms.iter().skip(idx + 1) {
            let Some(score) = cosine(&left.embedding, &right.embedding) else {
                continue;
            };
            if score >= 0.5 {
                scored.push((left.id.clone(), right.id.clone(), score));
            }
        }
    }
    scored.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut per_atom: HashMap<String, usize> = HashMap::new();
    let mut edges = Vec::new();
    for (source, target, weight) in scored {
        if per_atom.get(&source).copied().unwrap_or(0) >= 3
            || per_atom.get(&target).copied().unwrap_or(0) >= 3
        {
            continue;
        }
        *per_atom.entry(source.clone()).or_default() += 1;
        *per_atom.entry(target.clone()).or_default() += 1;
        edges.push(CanvasEdgeData {
            source,
            target,
            weight,
            kind: Some("similarity".to_string()),
            predicate: Some("similarity".to_string()),
        });
    }
    edges
}

fn cosine(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    let denom = left_norm.sqrt() * right_norm.sqrt();
    (denom > 0.0).then_some(dot / denom)
}

fn merge_edges(edges: &mut Vec<CanvasEdgeData>, extra: Vec<CanvasEdgeData>) {
    let mut seen: HashSet<(String, String, String)> = edges
        .iter()
        .map(edge_key)
        .collect();
    for edge in extra {
        if seen.insert(edge_key(&edge)) {
            edges.push(edge);
        }
    }
}

fn edge_key(edge: &CanvasEdgeData) -> (String, String, String) {
    let (left, right) = sorted_pair(&edge.source, &edge.target);
    let layer = edge
        .predicate
        .as_deref()
        .or(edge.kind.as_deref())
        .unwrap_or("similarity");
    (left, right, layer.to_string())
}

fn sorted_pair(left: &str, right: &str) -> (String, String) {
    if left <= right {
        (left.to_string(), right.to_string())
    } else {
        (right.to_string(), left.to_string())
    }
}

fn filter_canvas_by_source_prefix(data: &GlobalCanvasData, prefix: &str) -> GlobalCanvasData {
    let atoms: Vec<CanvasAtomPosition> = data
        .atoms
        .iter()
        .filter(|a| {
            a.source_url
                .as_deref()
                .map(|url| url.starts_with(prefix))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    let kept: HashSet<&str> = atoms.iter().map(|a| a.atom_id.as_str()).collect();

    let edges: Vec<CanvasEdgeData> = data
        .edges
        .iter()
        .filter(|e| kept.contains(e.source.as_str()) && kept.contains(e.target.as_str()))
        .cloned()
        .collect();

    let clusters: Vec<CanvasClusterLabel> = data
        .clusters
        .iter()
        .filter_map(|c| {
            let atom_ids: Vec<String> = c
                .atom_ids
                .iter()
                .filter(|id| kept.contains(id.as_str()))
                .cloned()
                .collect();
            if atom_ids.is_empty() {
                None
            } else {
                Some(CanvasClusterLabel {
                    atom_count: atom_ids.len() as i32,
                    atom_ids,
                    ..c.clone()
                })
            }
        })
        .collect();

    GlobalCanvasData {
        atoms,
        edges,
        clusters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(id: &str) -> MemuCanvasAtom {
        MemuCanvasAtom {
            id: id.to_string(),
            title: id.to_string(),
            embedding: vec![1.0, 0.0],
            primary_tag: None,
            tag_count: 0,
            tag_ids: vec![],
            entity_ids: vec![],
            entity_names: vec![],
            source_url: None,
        }
    }

    fn edge(source: &str, target: &str, predicate: &str) -> CanvasEdgeData {
        CanvasEdgeData {
            source: source.to_string(),
            target: target.to_string(),
            weight: 0.7,
            kind: Some(predicate.to_string()),
            predicate: Some(predicate.to_string()),
        }
    }

    #[test]
    fn memu_canvas_keeps_similarity_and_predicate_layers() {
        let data = memu_canvas_data(MemuCanvasSource {
            atoms: vec![atom("memory:a"), atom("memory:b")],
            edges: vec![edge("memory:a", "memory:b", "caused_by")],
        });

        let layers: HashSet<_> = data
            .edges
            .iter()
            .map(|edge| edge.predicate.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(layers, HashSet::from(["caused_by", "similarity"]));
    }

    #[test]
    fn memu_canvas_dedupes_same_pair_same_layer_symmetrically() {
        let data = memu_canvas_data(MemuCanvasSource {
            atoms: vec![atom("memory:a"), atom("memory:b")],
            edges: vec![edge("memory:b", "memory:a", "similarity")],
        });

        let similarity_count = data
            .edges
            .iter()
            .filter(|edge| edge.predicate.as_deref() == Some("similarity"))
            .count();
        assert_eq!(similarity_count, 1);
    }
}
