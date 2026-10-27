//! Chat agent loop with tool calling and streaming
//!
//! Provides the agentic chat loop that searches the knowledge base,
//! retrieves atoms, and generates responses with citations.
//! Uses a callback-based event system (same pattern as EmbeddingEvent).

use crate::atom_edit::{AtomEditOperation, apply_atom_edits};
use crate::chunking::count_tokens;
use crate::embedding::EmbeddingEvent;
use crate::models::{
    Atom, AtomKind, AtomWithTags, ChatCitation, ChatMessage, ChatMessageWithContext, ChatToolCall,
    SemanticSearchResult, Tag,
};
use crate::providers::traits::LlmConfig;
use crate::providers::types::{
    GenerationParams, Message, MessageRole, StreamDelta, ToolDefinition,
};
use crate::providers::{ProviderConfig, ProviderType, create_streaming_llm_provider};
use crate::search::{SearchMode, SearchOptions};
use crate::storage::StorageBackend;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use uuid::Uuid;

type ChatEventCallback = Arc<dyn Fn(ChatEvent) + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct MemuToolConfig {
    pub base_url: String,
    pub user_id: String,
    pub soul_id: String,
}

#[derive(Serialize)]
struct AtomicPromptLogRequest<'a> {
    conversation_id: &'a str,
    model: &'a str,
    messages: &'a [Message],
}

// ==================== Chat Events ====================

/// Events emitted during the chat agent loop.
/// Consumers (Tauri, HTTP server) bridge these to their own event systems.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    /// Streaming content delta (accumulated)
    StreamDelta {
        conversation_id: String,
        content: String,
    },
    /// Tool execution started
    ToolStart {
        conversation_id: String,
        tool_call_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    /// Tool execution completed
    ToolComplete {
        conversation_id: String,
        tool_call_id: String,
        results_count: i32,
        is_error: bool,
    },
    /// Full message completed
    Complete {
        conversation_id: String,
        message: ChatMessageWithContext,
    },
    /// Canvas action requested by the agent (executed on frontend)
    CanvasAction {
        conversation_id: String,
        action: String,
        params: serde_json::Value,
    },
    /// Atom created by a chat tool
    AtomCreated {
        conversation_id: String,
        atom: AtomWithTags,
    },
    /// Atom updated by a chat tool
    AtomUpdated {
        conversation_id: String,
        atom: AtomWithTags,
    },
    /// Embedding/tagging pipeline event for an atom mutated by a chat tool
    AtomPipelineEvent {
        conversation_id: String,
        event: EmbeddingEvent,
    },
    /// Error during chat
    Error {
        conversation_id: String,
        error: String,
    },
}

// ==================== Tool Definitions ====================

fn get_tools(memu_backed: bool) -> Vec<ToolDefinition> {
    let mut tools = vec![
        ToolDefinition::new(
            "search_atoms",
            "Search for relevant atoms using hybrid keyword and semantic search. Use this to find information related to a specific topic or question. Set since_days when the user is asking about recent notes (e.g., 7 for last week, 30 for last month).",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query to find relevant atoms"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5)",
                        "default": 5
                    },
                    "since_days": {
                        "type": "integer",
                        "description": "Optional recency filter: only return atoms created within the last N days. Use when the user's question is time-sensitive (e.g., 7 for last week, 30 for last month).",
                        "minimum": 1
                    }
                },
                "required": ["query"]
            }),
        ),
        ToolDefinition::new(
            "get_atom",
            "Get the content of a specific atom by its ID. Returns up to `limit` lines starting at `offset` (defaults: offset=0, limit=500). If the atom has more content, the response includes a line-count header indicating how to continue reading via a follow-up call with a higher offset.",
            json!({
                "type": "object",
                "properties": {
                    "atom_id": {
                        "type": "string",
                        "description": "The ID of the atom to retrieve"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (0-indexed). Use the value from a prior response's header to continue reading a truncated atom.",
                        "minimum": 0,
                        "default": 0
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return. Defaults to 500; prefer the default unless you know you need more.",
                        "minimum": 1,
                        "maximum": 2000,
                        "default": 500
                    }
                },
                "required": ["atom_id"]
            }),
        ),
        ToolDefinition::new(
            "create_atom",
            "Create a new atom with markdown content. Only use this when the user explicitly asks you to create, save, draft, or add a new atom/note. Do not call this for ordinary answers. After creating an atom, mention it in your final response using [[atom_id]] so the UI can link to it.",
            json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Full markdown content for the new atom"
                    },
                    "source_url": {
                        "type": "string",
                        "description": "Optional source URL for the atom"
                    },
                    "published_at": {
                        "type": "string",
                        "description": "Optional ISO 8601 publication date"
                    },
                    "tag_ids": {
                        "type": "array",
                        "description": "Optional existing tag IDs to assign",
                        "items": { "type": "string" },
                        "default": []
                    }
                },
                "required": ["content"]
            }),
        ),
        ToolDefinition::new(
            "edit_atom",
            "Apply edits to an existing atom. Only use this when the user explicitly asks you to modify an atom. Supports replace, insert_after, append, and replace_all operations. Prefer targeted edits. Use replace_all only when the user explicitly asks for a full rewrite or provides complete replacement content. replace and insert_after require exact text that appears exactly once in the current atom; call get_atom first if you need context.",
            json!({
                "type": "object",
                "properties": {
                    "atom_id": {
                        "type": "string",
                        "description": "The ID of the atom to edit"
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more edits applied in order to the atom content. The whole operation fails without saving if any edit is invalid.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "operation": {
                                    "type": "string",
                                    "enum": ["replace", "insert_after", "append", "replace_all"],
                                    "description": "replace swaps exact old_text for new_text; insert_after inserts text after exact anchor_text; append adds text to the end of the atom; replace_all replaces the full atom content."
                                },
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to replace. Required for replace and must occur exactly once."
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "Replacement text for replace."
                                },
                                "anchor_text": {
                                    "type": "string",
                                    "description": "Exact text to insert after. Required for insert_after and must occur exactly once."
                                },
                                "text": {
                                    "type": "string",
                                    "description": "Text to insert for insert_after or append. Include leading newlines/spaces exactly as desired."
                                },
                                "content": {
                                    "type": "string",
                                    "description": "Full replacement markdown content. Required for replace_all."
                                }
                            },
                            "required": ["operation"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["atom_id", "edits"]
            }),
        ),
    ];
    if memu_backed {
        tools.retain(|tool| tool.name != "create_atom");
    }
    tools
}

// ==================== UI Context ====================

/// Context about the user's current app view, passed from the frontend with a
/// chat turn. This stays compact so the agent can explicitly retrieve full
/// content through tools instead of receiving hidden prompt content.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PageContext {
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub atom_id: Option<String>,
    #[serde(default)]
    pub atom_title: Option<String>,
    #[serde(default)]
    pub atom_snippet: Option<String>,
    #[serde(default)]
    pub wiki_tag_id: Option<String>,
    #[serde(default)]
    pub wiki_tag_name: Option<String>,
    #[serde(default)]
    pub selected_tag_id: Option<String>,
}

fn get_page_context_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition::new(
        "get_current_page_context",
        "Get compact context about what the user is currently viewing in Atomic, such as the visible atom, wiki page, selected tag, and a short atom snippet. Use this first when the user says things like \"this atom\", \"the note I'm reading\", \"this page\", \"what I'm looking at\", or otherwise refers to visible UI context.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    )]
}

fn get_page_context_system_prompt() -> &'static str {
    r#"

You can inspect the user's current Atomic UI context with get_current_page_context.
Use it before answering when the user refers to "this", "current", "open", "visible", or the note/page they are reading. If it returns an atom_id and you need more than the snippet, call get_atom with that atom_id before answering."#
}

async fn execute_get_current_page_context(
    storage: &StorageBackend,
    page_context: Option<&PageContext>,
    scope_tag_ids: &[String],
    memu_tool_config: Option<&MemuToolConfig>,
) -> Result<Option<serde_json::Value>, String> {
    let Some(ctx) = page_context else {
        return Ok(None);
    };

    let mut visible_atom = serde_json::Value::Null;
    if let Some(atom_id) = ctx.atom_id.as_deref().filter(|id| !id.is_empty()) {
        if let Some(config) = memu_tool_config.filter(|_| is_memu_id(atom_id)) {
            visible_atom = match fetch_memu_node(config, atom_id).await? {
                Some(node) => {
                    let title = if node.label.is_empty() {
                        node.id.clone()
                    } else {
                        node.label.clone()
                    };
                    json!({
                        "id": node.id,
                        "title": title,
                        "snippet": node.summary,
                        "kind": node.kind,
                    })
                }
                None => json!({
                    "id": atom_id,
                    "title": ctx.atom_title.as_deref(),
                    "snippet": ctx.atom_snippet.as_deref(),
                    "not_found": true,
                }),
            };
            return Ok(Some(json!({
                "view": ctx.view.as_deref(),
                "visible_atom": visible_atom,
                "wiki": {
                    "tag_id": ctx.wiki_tag_id.as_deref(),
                    "tag_name": ctx.wiki_tag_name.as_deref(),
                },
                "selected_tag_id": ctx.selected_tag_id.as_deref(),
            })));
        }
        if !atom_matches_scope(storage, atom_id, scope_tag_ids).await? {
            return Err(format!(
                "Atom {atom_id} is outside this conversation's scope"
            ));
        }
        let stored_atom = storage
            .get_atom_impl(atom_id)
            .await
            .map_err(|e| e.to_string())?;

        visible_atom = match stored_atom {
            Some(atom_with_tags) => json!({
                "id": atom_with_tags.atom.id,
                "title": atom_with_tags.atom.title,
                "snippet": atom_with_tags.atom.snippet,
                "source_url": atom_with_tags.atom.source_url,
                "tags": atom_with_tags
                    .tags
                    .into_iter()
                    .map(|tag| json!({ "id": tag.id, "name": tag.name }))
                    .collect::<Vec<_>>(),
            }),
            None => json!({
                "id": atom_id,
                "title": ctx.atom_title.as_deref(),
                "snippet": ctx.atom_snippet.as_deref(),
                "not_found": true,
            }),
        };
    }

    Ok(Some(json!({
        "view": ctx.view.as_deref(),
        "visible_atom": visible_atom,
        "wiki": {
            "tag_id": ctx.wiki_tag_id.as_deref(),
            "tag_name": ctx.wiki_tag_name.as_deref(),
        },
        "selected_tag_id": ctx.selected_tag_id.as_deref(),
    })))
}

// ==================== Canvas Context ====================

/// Context about the canvas state, passed from the frontend when chatting from the canvas.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CanvasContext {
    pub clusters: Vec<CanvasClusterSummary>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CanvasClusterSummary {
    pub label: String,
    pub atom_count: i32,
}

fn get_canvas_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "zoom_to_cluster",
            "Zoom the canvas camera to center on a single cluster of related atoms. The canvas can only show one view at a time — each call replaces the previous view. Use this when the user asks about a topic area visible on their knowledge graph.",
            json!({
                "type": "object",
                "properties": {
                    "cluster_label": {
                        "type": "string",
                        "description": "The label of the cluster to zoom to"
                    }
                },
                "required": ["cluster_label"]
            }),
        ),
        ToolDefinition::new(
            "focus_atom",
            "Zoom to a specific atom on the canvas and show its preview. The canvas can only focus on one atom at a time. Use this after searching to highlight a specific result on the graph.",
            json!({
                "type": "object",
                "properties": {
                    "atom_id": {
                        "type": "string",
                        "description": "The ID of the atom to focus on"
                    }
                },
                "required": ["atom_id"]
            }),
        ),
    ]
}

const MAX_CANVAS_CLUSTERS: usize = 30;

fn get_canvas_system_prompt(ctx: &CanvasContext) -> String {
    // Take the largest clusters by atom count
    let mut sorted: Vec<&CanvasClusterSummary> = ctx.clusters.iter().collect();
    sorted.sort_by(|a, b| b.atom_count.cmp(&a.atom_count));
    let cluster_list: Vec<String> = sorted
        .iter()
        .take(MAX_CANVAS_CLUSTERS)
        .map(|c| format!("- \"{}\" ({} atoms)", c.label, c.atom_count))
        .collect();
    format!(
        r#"

You are also viewing the user's knowledge graph canvas. The following topic clusters are visible:
{}

You have canvas interaction tools available:
- zoom_to_cluster: Animate the camera to center on a cluster. Use when discussing a topic area.
- focus_atom: Zoom to a specific atom and show its preview. Use after searching to highlight results on the canvas.

The canvas shows a single view at a time — each navigation call replaces the previous one. If the user asks about multiple clusters or atoms, pick the most relevant one or navigate sequentially, not simultaneously.

Use these tools proactively when they would help the user navigate their knowledge visually."#,
        cluster_list.join("\n")
    )
}

// ==================== Tool Execution ====================

#[derive(Debug, Deserialize)]
struct MemuSearchResponse {
    #[serde(default)]
    nodes: Vec<MemuNode>,
}

#[derive(Debug, Deserialize)]
struct MemuNode {
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    memory_type: Option<String>,
    #[serde(default)]
    happened_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    category_ids: Vec<String>,
    #[serde(default)]
    category_names: Vec<String>,
    #[serde(default)]
    score: Option<f32>,
}

fn is_memu_id(atom_id: &str) -> bool {
    atom_id.starts_with("memory:")
        || atom_id.starts_with("category:")
        || atom_id.starts_with("entity:")
}

fn slice_text(content: &str, offset: usize, limit: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return String::new();
    }
    if offset >= total {
        return format!(
            "[offset={} is past end of atom ({} total lines)]",
            offset, total
        );
    }

    let end = (offset + limit).min(total);
    let slice = lines[offset..end].join("\n");
    if offset == 0 && end == total {
        return slice;
    }

    let header = if end < total {
        format!(
            "[lines {}-{} of {}. {} more lines. Call get_atom again with offset={} to continue.]\n",
            offset + 1,
            end,
            total,
            total - end,
            end,
        )
    } else {
        format!(
            "[lines {}-{} of {} (end of atom).]\n",
            offset + 1,
            end,
            total
        )
    };
    format!("{}{}", header, slice)
}

fn render_memu_node(node: &MemuNode) -> String {
    let mut lines = vec![format!("kind: {}", node.kind), format!("id: {}", node.id)];
    if let Some(memory_type) = node
        .memory_type
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("memory_type: {}", memory_type));
    }
    if !node.category_names.is_empty() {
        lines.push(format!("categories: {}", node.category_names.join(", ")));
    }
    if let Some(happened_at) = node
        .happened_at
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("happened_at: {}", happened_at));
    }
    if let Some(created_at) = node.created_at.as_deref().filter(|value| !value.is_empty()) {
        lines.push(format!("created_at: {}", created_at));
    }
    let summary = node.summary.trim();
    if !summary.is_empty() {
        lines.push(String::new());
        lines.push(summary.to_string());
    }
    lines.join("\n")
}

fn memu_node_timestamp(node: &MemuNode) -> String {
    node.updated_at
        .as_deref()
        .or(node.happened_at.as_deref())
        .or(node.created_at.as_deref())
        .unwrap_or("1970-01-01T00:00:00Z")
        .to_string()
}

fn memu_node_to_atom(node: MemuNode) -> AtomWithTags {
    let text = node.summary.clone();
    let title = if node.label.is_empty() {
        node.id.clone()
    } else {
        node.label.clone()
    };
    let timestamp = memu_node_timestamp(&node);
    let tags = node
        .category_ids
        .iter()
        .zip(node.category_names.iter())
        .map(|(id, name)| Tag {
            id: format!("category:{id}"),
            name: name.clone(),
            parent_id: None,
            created_at: "1970-01-01T00:00:00Z".to_string(),
            is_autotag_target: false,
            autotag_description: String::new(),
        })
        .collect();
    AtomWithTags {
        atom: Atom {
            id: node.id,
            content: text.clone(),
            title,
            snippet: text,
            source_url: None,
            source: node.memory_type.or(Some(node.kind)),
            published_at: node.happened_at,
            created_at: node.created_at.unwrap_or_else(|| timestamp.clone()),
            updated_at: timestamp,
            embedding_status: "complete".to_string(),
            tagging_status: "skipped".to_string(),
            embedding_error: None,
            tagging_error: None,
            kind: AtomKind::Captured,
        },
        tags,
    }
}

fn memu_node_to_search_result(node: MemuNode) -> SemanticSearchResult {
    let score = node.score.unwrap_or(1.0);
    let summary = node.summary.clone();
    SemanticSearchResult {
        atom: memu_node_to_atom(node),
        similarity_score: score,
        matching_chunk_content: summary,
        matching_chunk_index: 0,
        match_snippet: None,
        match_offsets: None,
        match_count: None,
    }
}

async fn fetch_memu_node(
    config: &MemuToolConfig,
    atom_id: &str,
) -> Result<Option<MemuNode>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("memU client failed: {e}"))?;
    let response = client
        .get(format!("{}/memory/{}", config.base_url, atom_id))
        .query(&[
            ("user_id", config.user_id.as_str()),
            ("soul_id", config.soul_id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("memU get_atom request failed: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let status = response.status();
    if !status.is_success() {
        return Err(format!("memU get_atom failed ({status})"));
    }
    response
        .json::<MemuNode>()
        .await
        .map(Some)
        .map_err(|e| format!("memU get_atom returned invalid JSON: {e}"))
}

async fn forward_atomic_prompt_log(
    config: &MemuToolConfig,
    conversation_id: &str,
    model: &str,
    messages: &[Message],
) {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    else {
        return;
    };
    let _ = client
        .post(format!("{}/integration/atomic/prompt_log", config.base_url))
        .json(&AtomicPromptLogRequest {
            conversation_id,
            model,
            messages,
        })
        .send()
        .await;
}

async fn execute_search_atoms(
    storage: &StorageBackend,
    query: &str,
    limit: i32,
    since_days: Option<i32>,
    scope_tag_ids: &[String],
    external_settings: Option<std::collections::HashMap<String, String>>,
    memu_tool_config: Option<&MemuToolConfig>,
) -> Result<Vec<SemanticSearchResult>, String> {
    if let Some(config) = memu_tool_config {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("memU client failed: {e}"))?;
        let mut params = vec![
            ("q", query.to_string()),
            ("user_id", config.user_id.clone()),
            ("soul_id", config.soul_id.clone()),
            ("limit", limit.max(1).to_string()),
        ];
        if let Some(days) = since_days {
            params.push(("since_days", days.to_string()));
        }
        let response = client
            .get(format!("{}/integration/atomic/search", config.base_url))
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("memU search request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("memU search failed ({status})"));
        }
        return response
            .json::<MemuSearchResponse>()
            .await
            .map(|body| {
                body.nodes
                    .into_iter()
                    .map(memu_node_to_search_result)
                    .collect()
            })
            .map_err(|e| format!("memU search returned invalid JSON: {e}"));
    }

    // Try SQLite path first (uses full search module with settings resolution)
    if let Some(sqlite) = storage.as_sqlite() {
        let options = SearchOptions::new(query, SearchMode::Hybrid, limit)
            .with_threshold(0.3)
            .with_scope(scope_tag_ids.to_vec())
            .with_since_days(since_days);
        return crate::search::search_atoms_with_settings(&sqlite.db, options, external_settings)
            .await;
    }

    // Postgres path: use storage dispatch methods
    let settings = match external_settings {
        Some(s) => s,
        None => storage
            .get_all_settings_sync()
            .await
            .map_err(|e| e.to_string())?,
    };
    let config = ProviderConfig::from_settings(&settings);
    let tag_id = scope_tag_ids.first().map(|s| s.as_str());

    // Generate query embedding
    let provider = crate::providers::get_embedding_provider(&config).map_err(|e| e.to_string())?;
    let embed_config = crate::providers::EmbeddingConfig::new(config.embedding_model());
    let embeddings = provider
        .embed_batch(&[query.to_string()], &embed_config)
        .await
        .map_err(|e| e.to_string())?;

    let cutoff = since_days.map(crate::search::since_days_cutoff);
    let cutoff_ref = cutoff.as_deref();

    // Chat is an in-app conversational surface over the user's own KB;
    // finding atoms participate as first-class context just like captured
    // ones (mirrors the SQLite path that goes through SearchOptions with
    // its default KindFilter::All).
    let kinds = crate::models::KindFilter::All;
    let keyword = storage
        .keyword_search_sync(query, limit * 2, tag_id, cutoff_ref, &kinds)
        .await
        .map_err(|e| e.to_string())?;
    let semantic = if !embeddings.is_empty() && !embeddings[0].is_empty() {
        storage
            .vector_search_sync(&embeddings[0], limit * 2, 0.3, tag_id, cutoff_ref, &kinds)
            .await
            .map_err(|e| e.to_string())?
    } else {
        vec![]
    };

    Ok(crate::search::merge_search_results_rrf(
        semantic, keyword, limit,
    ))
}

/// Default line limit for a single `get_atom` call. Chosen to keep context
/// usage bounded on long atoms (imports, pasted articles) while being large
/// enough that most notes fit in one call.
const GET_ATOM_DEFAULT_LIMIT: usize = 500;
const GET_ATOM_MAX_LIMIT: usize = 2000;

async fn atom_matches_scope(
    storage: &StorageBackend,
    atom_id: &str,
    scope_tag_ids: &[String],
) -> Result<bool, String> {
    if scope_tag_ids.is_empty() {
        return Ok(true);
    }
    let atom_tags = storage
        .get_tag_ids_for_atoms_batch_impl(&[atom_id.to_string()])
        .await
        .map_err(|e| e.to_string())?;
    for scope_tag_id in scope_tag_ids {
        let hierarchy = storage
            .get_tag_hierarchy_impl(scope_tag_id)
            .await
            .map_err(|e| e.to_string())?;
        if hierarchy.iter().any(|tag_id| atom_tags.contains(tag_id)) {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn execute_get_atom(
    storage: &StorageBackend,
    atom_id: &str,
    offset: usize,
    limit: usize,
    scope_tag_ids: &[String],
    memu_tool_config: Option<&MemuToolConfig>,
) -> Result<Option<String>, String> {
    if let Some(config) = memu_tool_config.filter(|_| is_memu_id(atom_id)) {
        return Ok(fetch_memu_node(config, atom_id)
            .await?
            .map(|node| slice_text(&render_memu_node(&node), offset, limit)));
    }
    if !atom_matches_scope(storage, atom_id, scope_tag_ids).await? {
        return Err(format!(
            "Atom {atom_id} is outside this conversation's scope"
        ));
    }

    let Some(content) = storage
        .get_atom_content_impl(atom_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };

    Ok(Some(slice_text(&content, offset, limit)))
}

fn parse_optional_string_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_tag_ids_arg(args: &serde_json::Value) -> Vec<String> {
    args.get("tag_ids")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn execute_create_atom(
    storage: &StorageBackend,
    tool_args: &serde_json::Value,
    external_settings: Option<std::collections::HashMap<String, String>>,
    canvas_cache: Option<&crate::CanvasCache>,
    on_embedding_event: Arc<dyn Fn(EmbeddingEvent) + Send + Sync + 'static>,
) -> Result<AtomWithTags, String> {
    let content = tool_args["content"].as_str().unwrap_or("").to_string();
    if content.trim().is_empty() {
        return Err("Cannot create an empty atom".to_string());
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let request = crate::CreateAtomRequest {
        content: content.clone(),
        source_url: parse_optional_string_arg(tool_args, "source_url"),
        published_at: parse_optional_string_arg(tool_args, "published_at"),
        tag_ids: parse_tag_ids_arg(tool_args),
        skip_if_source_exists: false,
    };

    let atom = storage
        .insert_atom_impl(&id, &request, &now)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(cache) = canvas_cache {
        cache.invalidate();
    }

    enqueue_agent_pipeline_in_background(
        storage.clone(),
        id.clone(),
        "agent_create_atom".to_string(),
        external_settings,
        canvas_cache.cloned(),
        on_embedding_event,
    );

    Ok(atom)
}

async fn enqueue_and_process_agent_pipeline(
    storage: &StorageBackend,
    atom_id: &str,
    reason: &str,
    external_settings: Option<std::collections::HashMap<String, String>>,
    canvas_cache: Option<&crate::CanvasCache>,
    on_embedding_event: Arc<dyn Fn(EmbeddingEvent) + Send + Sync + 'static>,
) -> Result<(), String> {
    let job = crate::models::AtomPipelineJobRequest {
        atom_id: atom_id.to_string(),
        embed_requested: true,
        tag_requested: true,
        not_before: None,
        reason: reason.to_string(),
        replace_existing: false,
    };
    storage
        .enqueue_pipeline_jobs_sync(&[job])
        .await
        .map_err(|e| e.to_string())?;
    let callback = {
        let on_embedding_event = Arc::clone(&on_embedding_event);
        move |event| on_embedding_event(event)
    };
    match external_settings {
        Some(settings) => {
            crate::embedding::process_queued_pipeline_jobs_with_settings(
                storage.clone(),
                callback,
                settings,
                canvas_cache.cloned(),
            )
            .await?;
        }
        None => {
            crate::embedding::process_queued_pipeline_jobs(
                storage.clone(),
                callback,
                canvas_cache.cloned(),
            )
            .await?;
        }
    }
    Ok(())
}

fn enqueue_agent_pipeline_in_background(
    storage: StorageBackend,
    atom_id: String,
    reason: String,
    external_settings: Option<std::collections::HashMap<String, String>>,
    canvas_cache: Option<crate::CanvasCache>,
    on_embedding_event: Arc<dyn Fn(EmbeddingEvent) + Send + Sync + 'static>,
) {
    let failure_callback = Arc::clone(&on_embedding_event);
    crate::executor::spawn(async move {
        let result = enqueue_and_process_agent_pipeline(
            &storage,
            &atom_id,
            &reason,
            external_settings,
            canvas_cache.as_ref(),
            on_embedding_event,
        )
        .await;

        if let Err(error) = result {
            tracing::warn!(
                atom_id = %atom_id,
                reason = %reason,
                error = %error,
                "Agent mutation pipeline failed"
            );
            failure_callback(EmbeddingEvent::EmbeddingFailed { atom_id, error });
        }
    });
}

async fn execute_edit_atom(
    storage: &StorageBackend,
    tool_args: &serde_json::Value,
    external_settings: Option<std::collections::HashMap<String, String>>,
    canvas_cache: Option<&crate::CanvasCache>,
    on_embedding_event: Arc<dyn Fn(EmbeddingEvent) + Send + Sync + 'static>,
    memu_tool_config: Option<&MemuToolConfig>,
) -> Result<Option<AtomWithTags>, String> {
    let atom_id = tool_args["atom_id"].as_str().unwrap_or("");
    if let Some(config) = memu_tool_config {
        if !is_memu_id(atom_id) {
            return Err("memU-backed sessions can only edit memU atoms".to_string());
        }
        if atom_id.starts_with("entity:") {
            return Err("memU entity atoms are read-only in this slice".to_string());
        }
        let Some(existing) = fetch_memu_node(config, atom_id).await? else {
            return Ok(None);
        };
        let edits: Vec<AtomEditOperation> = serde_json::from_value(
            tool_args
                .get("edits")
                .cloned()
                .ok_or_else(|| "edits must be an array".to_string())?,
        )
        .map_err(|e| format!("edits must be valid edit operations: {}", e))?;
        let summary = apply_atom_edits(existing.summary.trim(), &edits)?;
        if summary == existing.summary.trim() {
            return Err("Edits did not change the atom content".to_string());
        }
        if summary.trim().is_empty() {
            return Err("Cannot update an atom to empty content".to_string());
        }

        let endpoint = if atom_id.starts_with("memory:") {
            "memory"
        } else {
            "category"
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("memU client failed: {e}"))?;
        let response = client
            .patch(format!("{}/{}/{}", config.base_url, endpoint, atom_id))
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ])
            .json(&json!({
                "summary": summary.trim(),
                "edited_by": "atomic:agent",
            }))
            .send()
            .await
            .map_err(|e| format!("memU edit_atom request failed: {e}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("memU edit_atom failed ({status}): {body}"));
        }
        return response
            .json::<MemuNode>()
            .await
            .map(|node| Some(memu_node_to_atom(node)))
            .map_err(|e| format!("memU edit_atom returned invalid JSON: {e}"));
    }

    let Some(existing) = storage
        .get_atom_impl(atom_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };

    let edits: Vec<AtomEditOperation> = serde_json::from_value(
        tool_args
            .get("edits")
            .cloned()
            .ok_or_else(|| "edits must be an array".to_string())?,
    )
    .map_err(|e| format!("edits must be valid edit operations: {}", e))?;
    let content = apply_atom_edits(&existing.atom.content, &edits)?;
    if content == existing.atom.content {
        return Err("Edits did not change the atom content".to_string());
    }
    if content.trim().is_empty() {
        return Err("Cannot update an atom to empty content".to_string());
    }

    let request = crate::UpdateAtomRequest {
        content: content.clone(),
        source_url: existing.atom.source_url,
        published_at: existing.atom.published_at,
        tag_ids: None,
    };
    let now = Utc::now().to_rfc3339();
    let atom = storage
        .update_atom_if_unchanged_impl(atom_id, &request, &now, &existing.atom.updated_at)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(cache) = canvas_cache {
        cache.invalidate();
    }

    enqueue_agent_pipeline_in_background(
        storage.clone(),
        atom_id.to_string(),
        "agent_edit_atom".to_string(),
        external_settings,
        canvas_cache.cloned(),
        on_embedding_event,
    );

    Ok(Some(atom))
}

// ==================== System Prompt ====================

fn get_system_prompt(scope_description: &str, memu_tool_config: Option<&MemuToolConfig>) -> String {
    let (intro, guidelines) = match memu_tool_config {
        Some(config) => (
            format!(
                "You are with {} in the app Atomic, where you can work on shaping your memories together. All of memory items and summaries are represented as individual atoms. In Atomic, {} is the user.",
                config.user_id, config.user_id
            ),
            "Guidelines for Atomic",
        ),
        None => (
            "You are a helpful AI assistant with access to the user's personal knowledge base. Your role is to answer questions by searching through and referencing the user's stored information.".to_string(),
            "Guidelines",
        ),
    };
    format!(
        r#"{intro}

{}

{guidelines}:
- Use search_atoms to find relevant information before answering, unless another available tool more directly addresses the user's request
- Only call create_atom or edit_atom when the user explicitly asks you to create or modify an atom
- Prefer targeted edit_atom operations. Use replace_all only for intentional full-content replacement
- When you create a new atom, include [[atom_id]] in the final response so the user can open it
- If the initial search doesn't find enough, try different search queries
- When you find relevant information, cite the exact [N] marker shown in the tool result; never renumber it
- Be honest if you cannot find information - do not make things up
- Keep responses concise but informative
- If the user asks about something not in their knowledge base, say so

When citing sources:
- Keep each source's tool-assigned marker, even when the numbers have gaps or start above [1]
- Place citations immediately after the relevant claim
- You can cite the same source multiple times if needed"#,
        scope_description
    )
}

fn chat_system_prompt(
    settings: &HashMap<String, String>,
    scope_description: &str,
    memu_tool_config: Option<&MemuToolConfig>,
    page_context: Option<&PageContext>,
    canvas_context: Option<&CanvasContext>,
) -> String {
    let base = get_system_prompt(scope_description, memu_tool_config);
    let mut prompt = match settings.get("chat_prompt").filter(|s| !s.is_empty()) {
        Some(prefix) => format!("{prefix}\n\n{base}"),
        None => base,
    };
    if page_context.is_some() {
        prompt.push_str(get_page_context_system_prompt());
    }
    if let Some(ctx) = canvas_context {
        prompt.push_str(&get_canvas_system_prompt(ctx));
    }
    prompt
}

const MEMU_RESPONSE_SENTENCES_SETTING: &str = "memu_response_sentences";
const MEMU_LOG_PROMPTS_SETTING: &str = "memu_log_prompts";

fn memu_response_sentence_limit(settings: &HashMap<String, String>) -> Option<usize> {
    settings
        .get(MEMU_RESPONSE_SENTENCES_SETTING)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn memu_prompt_logging_enabled(settings: &HashMap<String, String>) -> bool {
    matches!(
        settings.get(MEMU_LOG_PROMPTS_SETTING).map(String::as_str),
        Some("1" | "true" | "yes")
    )
}

fn append_memu_response_limit(mut messages: Vec<Message>, limit: usize) -> Vec<Message> {
    messages.push(Message::user(memu_response_limit_instruction(limit)));
    messages
}

fn memu_response_limit_instruction(limit: usize) -> String {
    format!("**respond with {limit} sentences or fewer**")
}

// ==================== Context Window Management ====================

/// Estimate token count for a message, including tool call content.
fn estimate_message_tokens(m: &Message) -> usize {
    let content_tokens = count_tokens(m.content.as_deref().unwrap_or(""));
    let tool_call_tokens = m.tool_calls.as_ref().map_or(0, |tcs| {
        tcs.iter()
            .map(|tc| {
                let args = tc.get_arguments().unwrap_or("");
                let name = tc.get_name().unwrap_or("");
                count_tokens(name) + count_tokens(args) + 10
            })
            .sum()
    });
    content_tokens + tool_call_tokens
}

/// A group of messages that must be kept together for API validity.
/// Either a single user/system message, or an assistant message followed
/// by its tool-result messages.
struct MessageGroup {
    start: usize,
    end: usize, // exclusive
    tokens: usize,
}

/// Group messages into atomic units that can't be split.
/// An assistant message with tool_calls and its subsequent tool-result messages
/// form one group. Everything else is its own group.
fn group_messages(messages: &[Message], message_tokens: &[usize]) -> Vec<MessageGroup> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == MessageRole::Assistant && messages[i].tool_calls.is_some() {
            // Start of a tool round: assistant + following tool results
            let start = i;
            let mut tokens = message_tokens[i];
            i += 1;
            while i < messages.len() && messages[i].role == MessageRole::Tool {
                tokens += message_tokens[i];
                i += 1;
            }
            groups.push(MessageGroup {
                start,
                end: i,
                tokens,
            });
        } else {
            groups.push(MessageGroup {
                start: i,
                end: i + 1,
                tokens: message_tokens[i],
            });
            i += 1;
        }
    }
    groups
}

/// Truncate message history to fit within the provider's context window.
/// Keeps the system prompt (first group) and the most recent group,
/// then includes as many recent groups as fit in the remaining budget.
/// Never splits assistant+tool-result pairs to maintain API validity.
/// Reserves ~30% of context for the assistant's response and tool results.
fn truncate_messages_to_context(
    messages: Vec<Message>,
    context_length: Option<usize>,
) -> Vec<Message> {
    let max_tokens = match context_length {
        Some(ctx_len) => (ctx_len as f64 * 0.7) as usize,
        None => return messages,
    };

    if messages.len() <= 2 {
        return messages;
    }

    let message_tokens: Vec<usize> = messages.iter().map(estimate_message_tokens).collect();
    let total: usize = message_tokens.iter().sum();
    if total <= max_tokens {
        return messages;
    }

    let groups = group_messages(&messages, &message_tokens);
    if groups.len() <= 2 {
        return messages; // System + one group, nothing safe to drop
    }

    // Always keep the leading system prefix and the last group. Atomic may store
    // a memU entry snapshot as a second system message after its base prompt.
    let system_prefix_groups = groups
        .iter()
        .take_while(|g| messages[g.start].role == MessageRole::System)
        .count()
        .max(1);
    if groups.len() <= system_prefix_groups + 1 {
        return messages; // System prefix + one group, nothing safe to drop
    }
    let first_tokens: usize = groups[..system_prefix_groups]
        .iter()
        .map(|g| g.tokens)
        .sum();
    let last_tokens = groups[groups.len() - 1].tokens;
    let mut budget = max_tokens.saturating_sub(first_tokens + last_tokens);

    // Work backwards through middle groups, keeping as many as fit
    let mut keep_from_group = groups.len() - 1;
    for gi in (system_prefix_groups..groups.len() - 1).rev() {
        if groups[gi].tokens > budget {
            break;
        }
        budget -= groups[gi].tokens;
        keep_from_group = gi;
    }

    // Build result from kept groups
    let mut result: Vec<Message> = Vec::new();
    for g in &groups[..system_prefix_groups] {
        result.extend(messages[g.start..g.end].to_vec());
    }
    for g in &groups[keep_from_group..] {
        result.extend(messages[g.start..g.end].to_vec());
    }

    tracing::info!(
        original_messages = messages.len(),
        truncated_messages = result.len(),
        groups_kept = system_prefix_groups + groups.len() - keep_from_group,
        max_tokens,
        "[chat] Truncated message history to fit context window"
    );

    result
}

// ==================== Helper: Convert stored messages to provider format ====================

/// Convert ChatMessage models from storage into provider Message format for the API.
fn chat_messages_to_provider_messages(
    messages: Vec<crate::models::ChatMessageWithContext>,
) -> Vec<Message> {
    messages
        .into_iter()
        .map(|m| {
            let role = match m.message.role.as_str() {
                "system" => MessageRole::System,
                "assistant" => MessageRole::Assistant,
                "tool" => MessageRole::Tool,
                _ => MessageRole::User,
            };
            Message {
                role,
                content: Some(m.message.content),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }
        })
        .collect()
}

pub struct ChatToolRecords {
    pub tool_calls: Vec<ChatToolCall>,
    pub citations: Vec<ChatCitation>,
    pub citation_index: i32,
    prior_citations: Vec<ChatCitation>,
}

impl ChatToolRecords {
    pub fn for_reply(mut self, content: &str) -> Self {
        static REFERENCES: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"\[(\d+)\]").expect("citation regex"));
        let referenced: HashSet<i32> = REFERENCES
            .captures_iter(content)
            .filter_map(|capture| capture[1].parse().ok())
            .collect();
        for mut citation in self.prior_citations.drain(..) {
            if referenced.contains(&citation.citation_index)
                && !self
                    .citations
                    .iter()
                    .any(|new| new.citation_index == citation.citation_index)
            {
                citation.id = Uuid::new_v4().to_string();
                citation.message_id.clear();
                self.citations.push(citation);
            }
        }
        self.citations
            .sort_by_key(|citation| citation.citation_index);
        self
    }
}

pub struct ChatToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Input for MCP's existing Atomic Claude request. Resume never resends Presence.
pub struct ClaudeChatInput {
    pub system_prompt: Option<String>,
    pub prompt: String,
}

/// Owned tool state shared by API chat and the scoped Claude bridge.
/// Callers serialize execution for the whole call, including citation allocation.
pub struct ChatToolSession {
    storage: StorageBackend,
    conversation_id: String,
    scope_tag_ids: Vec<String>,
    external_settings: HashMap<String, String>,
    memu_tool_config: Option<MemuToolConfig>,
    page_context: Option<PageContext>,
    canvas_context: Option<CanvasContext>,
    canvas_cache: Option<crate::CanvasCache>,
    on_event: ChatEventCallback,
    tools: Vec<ToolDefinition>,
    citations: Vec<ChatCitation>,
    citation_index: i32,
    prior_citations: Vec<ChatCitation>,
    durable_message_id: Option<String>,
    tool_calls_record: Vec<ChatToolCall>,
}

impl ChatToolSession {
    pub(crate) fn new(
        storage: StorageBackend,
        conversation_id: String,
        scope_tag_ids: Vec<String>,
        external_settings: HashMap<String, String>,
        memu_tool_config: Option<MemuToolConfig>,
        page_context: Option<PageContext>,
        canvas_context: Option<CanvasContext>,
        canvas_cache: Option<crate::CanvasCache>,
        on_event: Arc<dyn Fn(ChatEvent) + Send + Sync + 'static>,
    ) -> Self {
        let mut tools = get_tools(memu_tool_config.is_some());
        if page_context.is_some() {
            tools.extend(get_page_context_tools());
        }
        if canvas_context.is_some() {
            tools.extend(get_canvas_tools());
        }
        Self {
            storage,
            conversation_id,
            scope_tag_ids,
            external_settings,
            memu_tool_config,
            page_context,
            canvas_context,
            canvas_cache,
            on_event,
            tools,
            citations: Vec::new(),
            citation_index: 0,
            prior_citations: Vec::new(),
            durable_message_id: None,
            tool_calls_record: Vec::new(),
        }
    }

    /// Caller holds the sitting lock; `new_sitting` comes from its native binding,
    /// not from the number of saved messages. Retry controls belong to C2.
    pub async fn claude_input(&self, new_sitting: bool) -> Result<ClaudeChatInput, String> {
        let user_message_id = self
            .durable_message_id
            .as_deref()
            .ok_or_else(|| "Claude input requires a scoped, saved user turn".to_string())?;
        let conversation = self
            .storage
            .get_conversation_sync(&self.conversation_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Conversation not found".to_string())?;
        let user = conversation
            .messages
            .iter()
            .find(|row| row.message.id == user_message_id && row.message.role == "user")
            .ok_or_else(|| "Saved user turn not found".to_string())?;
        let system_prompt = if new_sitting {
            let presence = conversation
                .messages
                .iter()
                .find(|row| row.message.role == "system")
                .map(|row| row.message.content.as_str())
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| "This sitting has no saved Soul Presence".to_string())?;
            Some(format!(
                "{}\n\n{presence}",
                chat_system_prompt(
                    &self.external_settings,
                    "",
                    self.memu_tool_config.as_ref(),
                    None,
                    None,
                )
            ))
        } else {
            None
        };
        let mut context = self
            .storage
            .get_scope_description_sync(&self.scope_tag_ids)
            .await
            .map_err(|e| e.to_string())?;
        if self.page_context.is_some() {
            context.push_str(get_page_context_system_prompt());
        }
        if let Some(canvas) = &self.canvas_context {
            context.push_str(&get_canvas_system_prompt(canvas));
        }
        let mut prompt = format!(
            "<atomic_turn_context>\n{context}\n</atomic_turn_context>\n\n{}",
            user.message.content
        );
        if let Some(limit) = memu_response_sentence_limit(&self.external_settings) {
            prompt.push_str(&format!("\n\n{}", memu_response_limit_instruction(limit)));
        }
        Ok(ClaudeChatInput {
            system_prompt,
            prompt,
        })
    }

    pub(crate) async fn bind_user_turn(
        &mut self,
        message_id: &str,
        high_water: i32,
        prior: Vec<ChatCitation>,
        mut calls: Vec<ChatToolCall>,
        citations: Vec<ChatCitation>,
    ) -> Result<(), String> {
        let mut catalog = BTreeMap::<i32, ChatCitation>::new();
        for citation in prior.iter().chain(citations.iter()) {
            if let Some(existing) = catalog.get(&citation.citation_index) {
                if existing.atom_id != citation.atom_id
                    || existing.chunk_index != citation.chunk_index
                {
                    return Err("This sitting has conflicting citation numbers and cannot continue safely. Restore its original chat backend or start a new sitting.".to_string());
                }
            }
            catalog
                .entry(citation.citation_index)
                .or_insert_with(|| citation.clone());
        }
        self.citation_index = high_water.max(catalog.keys().next_back().copied().unwrap_or(0));
        self.prior_citations = catalog.into_values().collect();
        self.durable_message_id = Some(message_id.to_string());
        let mut interrupted = false;
        for call in &mut calls {
            if call.status == "running" {
                let previous_output = call.tool_output.take();
                call.status = "failed".to_string();
                call.tool_output = Some(json!({
                    "error": "Interrupted; outcome unknown. Do not repeat this tool.",
                    "outcome_unknown": true, "previous_output": previous_output,
                }));
                interrupted = true;
            }
        }
        self.tool_calls_record = calls;
        self.citations = citations;
        if interrupted {
            self.checkpoint().await?;
        }
        if self.tool_calls_record.iter().any(|call| {
            call.tool_output
                .as_ref()
                .and_then(|output| output.get("outcome_unknown"))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        }) {
            return Err("A previous tool was interrupted; its outcome is unknown. Do not resume or repeat it. Close out from saved history or start a new sitting.".to_string());
        }
        Ok(())
    }

    fn add_citation(&mut self, atom_id: String, chunk_index: Option<i32>, excerpt: String) -> i32 {
        self.citation_index += 1;
        self.citations.push(ChatCitation {
            id: Uuid::new_v4().to_string(),
            message_id: self.durable_message_id.clone().unwrap_or_default(),
            citation_index: self.citation_index,
            atom_id,
            chunk_index,
            excerpt,
            relevance_score: None,
        });
        self.citation_index
    }

    async fn checkpoint(&self) -> Result<(), String> {
        // Keep issued snapshots even if local-atom cleanup removed their rows mid-turn.
        if let Some(message_id) = &self.durable_message_id {
            self.storage
                .save_chat_tool_state_sync(
                    &self.conversation_id,
                    message_id,
                    &self.tool_calls_record,
                    &self.citations,
                    self.citation_index,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn finish(self) -> ChatToolRecords {
        ChatToolRecords {
            tool_calls: self.tool_calls_record,
            citations: self.citations,
            citation_index: self.citation_index,
            prior_citations: self.prior_citations,
        }
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub async fn execute(
        &mut self,
        call_id: &str,
        tool_name: &str,
        tool_args: serde_json::Value,
    ) -> Result<ChatToolResult, String> {
        let record_index = if let Some(index) = self
            .tool_calls_record
            .iter()
            .position(|record| record.id == call_id)
        {
            let record = &mut self.tool_calls_record[index];
            let not_executed = record
                .tool_output
                .as_ref()
                .and_then(|output| output.get("executed"))
                .and_then(serde_json::Value::as_bool)
                == Some(false);
            if !not_executed || record.tool_name != tool_name || record.tool_input != tool_args {
                return Err("This tool call is already recorded and was not run again. Review its saved outcome before continuing.".to_string());
            }
            record.status = "running".to_string();
            record.tool_output = None;
            record.completed_at = None;
            index
        } else {
            self.tool_calls_record.push(ChatToolCall {
                id: call_id.to_string(),
                message_id: self.durable_message_id.clone().unwrap_or_default(),
                tool_name: tool_name.to_string(),
                tool_input: tool_args.clone(),
                tool_output: None,
                status: "running".to_string(),
                created_at: Utc::now().to_rfc3339(),
                completed_at: None,
            });
            self.tool_calls_record.len() - 1
        };
        if let Err(error) = self.checkpoint().await {
            let record = self
                .tool_calls_record
                .get_mut(record_index)
                .expect("unstarted tool record");
            record.status = "failed".to_string();
            record.completed_at = Some(Utc::now().to_rfc3339());
            record.tool_output = Some(json!({"error": error, "executed": false}));
            return Err(format!("Tool was not executed: {error}"));
        }
        let on_embedding_event: Arc<dyn Fn(EmbeddingEvent) + Send + Sync + 'static> = {
            let on_event = Arc::clone(&self.on_event);
            let conversation_id = self.conversation_id.clone();
            Arc::new(move |event| {
                on_event(ChatEvent::AtomPipelineEvent {
                    conversation_id: conversation_id.clone(),
                    event,
                });
            })
        };

        // Emit tool start event
        (self.on_event)(ChatEvent::ToolStart {
            conversation_id: self.conversation_id.clone(),
            tool_call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            tool_input: tool_args.clone(),
        });

        // Execute tool
        let (tool_result, results_count, is_error) = match tool_name {
            _ if !self.tools.iter().any(|tool| tool.name == tool_name) => (
                format!("Tool unavailable in this session: {tool_name}"),
                0,
                true,
            ),
            "search_atoms" => {
                let query = tool_args["query"].as_str().unwrap_or("");
                let limit = tool_args["limit"].as_i64().unwrap_or(5) as i32;
                let since_days = tool_args
                    .get("since_days")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as i32)
                    .filter(|d| *d > 0);
                match execute_search_atoms(
                    &self.storage,
                    query,
                    limit,
                    since_days,
                    &self.scope_tag_ids,
                    Some(self.external_settings.clone()),
                    self.memu_tool_config.as_ref(),
                )
                .await
                {
                    Ok(results) => {
                        let count = results.len() as i32;
                        let first_index = self.citation_index + 1;
                        for result in results.iter() {
                            self.add_citation(
                                result.atom.atom.id.clone(),
                                Some(result.matching_chunk_index),
                                result.matching_chunk_content.chars().take(200).collect(),
                            );
                        }
                        let result_text = results
                            .iter()
                            .enumerate()
                            .map(|(i, r)| {
                                format!(
                                    "[{}] (atom_id: {}, similarity: {:.2})\n{}",
                                    first_index + i as i32,
                                    r.atom.atom.id,
                                    r.similarity_score,
                                    r.matching_chunk_content
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        (result_text, count, false)
                    }
                    Err(e) => (format!("Error: {}", e), 0, true),
                }
            }
            "get_atom" => {
                let atom_id = tool_args["atom_id"].as_str().unwrap_or("");
                let offset = tool_args
                    .get("offset")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(0);
                let limit = tool_args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| (v as usize).clamp(1, GET_ATOM_MAX_LIMIT))
                    .unwrap_or(GET_ATOM_DEFAULT_LIMIT);
                match execute_get_atom(
                    &self.storage,
                    atom_id,
                    offset,
                    limit,
                    &self.scope_tag_ids,
                    self.memu_tool_config.as_ref(),
                )
                .await
                {
                    Ok(Some(content)) => {
                        let citation_index = self.add_citation(
                            atom_id.to_string(),
                            None,
                            content.chars().take(200).collect(),
                        );
                        (
                            format!("[{}] (atom_id: {})\n{}", citation_index, atom_id, content),
                            1,
                            false,
                        )
                    }
                    Ok(None) => ("Atom not found".to_string(), 0, true),
                    Err(e) => (format!("Error: {}", e), 0, true),
                }
            }
            "get_current_page_context" => {
                match execute_get_current_page_context(
                    &self.storage,
                    self.page_context.as_ref(),
                    &self.scope_tag_ids,
                    self.memu_tool_config.as_ref(),
                )
                .await
                {
                    Ok(Some(mut context)) => {
                        let visible_atom_id = context
                            .get("visible_atom")
                            .and_then(|atom| atom.get("id"))
                            .and_then(|id| id.as_str())
                            .map(str::to_string);

                        if let Some(atom_id) = visible_atom_id {
                            let excerpt: String = context
                                .get("visible_atom")
                                .and_then(|atom| atom.get("snippet"))
                                .and_then(|snippet| snippet.as_str())
                                .unwrap_or("")
                                .chars()
                                .take(200)
                                .collect();
                            if !excerpt.is_empty() {
                                let citation_index = self.add_citation(atom_id, None, excerpt);
                                if let Some(atom) = context
                                    .get_mut("visible_atom")
                                    .and_then(|atom| atom.as_object_mut())
                                {
                                    atom.insert(
                                        "citation_index".to_string(),
                                        json!(citation_index),
                                    );
                                }
                            }
                        }

                        (
                            serde_json::to_string_pretty(&context)
                                .unwrap_or_else(|_| context.to_string()),
                            1,
                            false,
                        )
                    }
                    Ok(None) => ("No current page context was provided.".to_string(), 0, true),
                    Err(e) => (format!("Error: {}", e), 0, true),
                }
            }
            "create_atom" => {
                match execute_create_atom(
                    &self.storage,
                    &tool_args,
                    Some(self.external_settings.clone()),
                    self.canvas_cache.as_ref(),
                    Arc::clone(&on_embedding_event),
                )
                .await
                {
                    Ok(atom) => {
                        (self.on_event)(ChatEvent::AtomCreated {
                            conversation_id: self.conversation_id.clone(),
                            atom: atom.clone(),
                        });
                        self.add_citation(
                            atom.atom.id.clone(),
                            None,
                            atom.atom.snippet.chars().take(200).collect(),
                        );
                        (
                            serde_json::to_string_pretty(&json!({
                                "atom_id": atom.atom.id,
                                "title": atom.atom.title,
                                "snippet": atom.atom.snippet,
                                "reference": format!("[[{}]]", atom.atom.id),
                            }))
                            .unwrap_or_else(|_| atom.atom.id),
                            1,
                            false,
                        )
                    }
                    Err(e) => (format!("Error: {}", e), 0, true),
                }
            }
            "edit_atom" => {
                match execute_edit_atom(
                    &self.storage,
                    &tool_args,
                    Some(self.external_settings.clone()),
                    self.canvas_cache.as_ref(),
                    Arc::clone(&on_embedding_event),
                    self.memu_tool_config.as_ref(),
                )
                .await
                {
                    Ok(Some(atom)) => {
                        if self.memu_tool_config.is_none() {
                            (self.on_event)(ChatEvent::AtomUpdated {
                                conversation_id: self.conversation_id.clone(),
                                atom: atom.clone(),
                            });
                        }
                        self.add_citation(
                            atom.atom.id.clone(),
                            None,
                            atom.atom.snippet.chars().take(200).collect(),
                        );
                        (
                            serde_json::to_string_pretty(&json!({
                                "atom_id": atom.atom.id,
                                "title": atom.atom.title,
                                "snippet": atom.atom.snippet,
                                "reference": format!("[[{}]]", atom.atom.id),
                            }))
                            .unwrap_or_else(|_| atom.atom.id),
                            1,
                            false,
                        )
                    }
                    Ok(None) => ("Atom not found".to_string(), 0, true),
                    Err(e) => (format!("Error: {}", e), 0, true),
                }
            }
            "zoom_to_cluster" => {
                let cluster_label = tool_args["cluster_label"].as_str().unwrap_or("");
                (self.on_event)(ChatEvent::CanvasAction {
                    conversation_id: self.conversation_id.clone(),
                    action: "zoom_to_cluster".to_string(),
                    params: json!({ "cluster_label": cluster_label }),
                });
                (
                    format!("Zoomed canvas to cluster '{}'", cluster_label),
                    1,
                    false,
                )
            }
            "focus_atom" => {
                let atom_id = tool_args["atom_id"].as_str().unwrap_or("");
                (self.on_event)(ChatEvent::CanvasAction {
                    conversation_id: self.conversation_id.clone(),
                    action: "focus_atom".to_string(),
                    params: json!({ "atom_id": atom_id }),
                });
                (format!("Focused canvas on atom '{}'", atom_id), 1, false)
            }
            _ => (format!("Unknown tool: {}", tool_name), 0, true),
        };

        let record = self
            .tool_calls_record
            .get_mut(record_index)
            .expect("running tool record");
        record.tool_output = Some(serde_json::Value::String(tool_result.clone()));
        record.status = if is_error { "failed" } else { "complete" }.to_string();
        record.completed_at = Some(Utc::now().to_rfc3339());
        let checkpoint = self.checkpoint().await;

        // Emit tool complete event
        (self.on_event)(ChatEvent::ToolComplete {
            conversation_id: self.conversation_id.clone(),
            tool_call_id: call_id.to_string(),
            results_count,
            is_error,
        });
        checkpoint.map_err(|e| format!("Tool finished, but saving its outcome failed: {e}"))?;

        Ok(ChatToolResult {
            content: tool_result,
            is_error,
        })
    }
}

// ==================== Agent Loop ====================

struct AgentContext {
    messages: Vec<Message>,
    tools: ChatToolSession,
}

async fn run_agent_loop(
    provider_config: ProviderConfig,
    model: String,
    mut ctx: AgentContext,
) -> Result<ChatMessageWithContext, String> {
    let provider = create_streaming_llm_provider(&provider_config)
        .map_err(|e| format!("Failed to create streaming provider: {}", e))?;
    let max_iterations = 10;

    for _iteration in 0..max_iterations {
        let config = LlmConfig::new(&model).with_params(
            GenerationParams::new()
                .with_temperature(0.7)
                .with_max_tokens(4000),
        );

        // Accumulate streaming content. The Box callback captures an Arc<Mutex<String>>
        // because we can't capture `on_event` (lifetime/Send issues with Box<dyn Fn>).
        // We emit the accumulated content as a StreamDelta after the call completes.
        let accumulated_content = Arc::new(Mutex::new(String::new()));
        let accumulated_clone = Arc::clone(&accumulated_content);

        let on_delta = Box::new(move |delta: StreamDelta| {
            if let StreamDelta::Content(text) = delta {
                let mut content = accumulated_clone.lock().unwrap();
                content.push_str(&text);
            }
        });

        // Truncate messages if they've grown beyond context window (from tool results)
        let mut call_messages = truncate_messages_to_context(
            ctx.messages.clone(),
            provider_config.context_length_for_model(&model),
        );
        if ctx.tools.memu_tool_config.is_some() {
            if let Some(limit) = memu_response_sentence_limit(&ctx.tools.external_settings) {
                call_messages = append_memu_response_limit(call_messages, limit);
            }
        }
        let should_forward_prompt_log = memu_prompt_logging_enabled(&ctx.tools.external_settings);
        if should_forward_prompt_log {
            if let Some(config) = ctx.tools.memu_tool_config.as_ref() {
                let config = config.clone();
                let conversation_id = ctx.tools.conversation_id.clone();
                let model = model.clone();
                let messages = call_messages.clone();
                crate::executor::spawn(async move {
                    forward_atomic_prompt_log(&config, &conversation_id, &model, &messages).await;
                });
            }
        }

        let response = provider
            .complete_streaming_with_tools(&call_messages, ctx.tools.tools(), &config, on_delta)
            .await
            .map_err(|e| format!("API request failed: {}", e))?;

        // Emit the accumulated content as a stream delta
        if let Ok(content) = accumulated_content.lock() {
            if !content.is_empty() {
                (ctx.tools.on_event)(ChatEvent::StreamDelta {
                    conversation_id: ctx.tools.conversation_id.clone(),
                    content: content.clone(),
                });
            }
        }

        // Check if there are tool calls
        if let Some(tool_calls) = &response.tool_calls {
            // Add assistant message with tool calls to history
            if response.content.is_empty() {
                ctx.messages
                    .push(Message::assistant_with_tool_calls(tool_calls.clone()));
            } else {
                let mut msg = Message::assistant(&response.content);
                msg.tool_calls = Some(tool_calls.clone());
                ctx.messages.push(msg);
            }

            // Execute each tool call
            for tool_call in tool_calls {
                let tool_name = tool_call.get_name().unwrap_or_default();
                let tool_args_str = tool_call.get_arguments().unwrap_or_default();
                let tool_args: serde_json::Value =
                    serde_json::from_str(tool_args_str).unwrap_or(serde_json::Value::Null);

                let tool_result = ctx
                    .tools
                    .execute(&tool_call.id, tool_name, tool_args)
                    .await?;

                // Add tool result to messages
                ctx.messages
                    .push(Message::tool_result(&tool_call.id, tool_result.content));
            }
        } else {
            // No tool calls - we have the final answer
            let content = response.content;

            let conversation_id = ctx.tools.conversation_id.clone();
            let records = ctx.tools.finish();

            return Ok(ChatMessageWithContext {
                message: ChatMessage {
                    id: Uuid::new_v4().to_string(),
                    conversation_id,
                    role: "assistant".to_string(),
                    content,
                    created_at: Utc::now().to_rfc3339(),
                    message_index: 0, // Set when saving
                },
                tool_calls: records.tool_calls,
                citations: records.citations,
            });
        }
    }

    Err("Max iterations reached without completing".to_string())
}

// ==================== Public API ====================

/// Send a chat message and run the agent loop.
///
/// The `on_event` callback is invoked with streaming deltas, tool call events,
/// and completion/error events. This is the same pattern as `EmbeddingEvent`.
///
/// Returns the final assistant message with tool calls and citations.
pub async fn send_chat_message<F>(
    storage: StorageBackend,
    conversation_id: &str,
    content: &str,
    on_event: F,
) -> Result<ChatMessageWithContext, String>
where
    F: Fn(ChatEvent) + Send + Sync + 'static,
{
    send_chat_message_with_settings(storage, conversation_id, content, on_event, None).await
}

/// Like `send_chat_message` but with externally-provided settings (from registry).
pub async fn send_chat_message_with_settings<F>(
    storage: StorageBackend,
    conversation_id: &str,
    content: &str,
    on_event: F,
    external_settings: Option<std::collections::HashMap<String, String>>,
) -> Result<ChatMessageWithContext, String>
where
    F: Fn(ChatEvent) + Send + Sync + 'static,
{
    send_chat_message_with_canvas(
        storage,
        conversation_id,
        content,
        on_event,
        external_settings,
        None,
        None,
        None,
        None,
    )
    .await
}

/// Like `send_chat_message_with_settings` but with optional UI context for
/// page-aware and canvas-aware tools.
pub async fn send_chat_message_with_canvas<F>(
    storage: StorageBackend,
    conversation_id: &str,
    content: &str,
    on_event: F,
    external_settings: Option<std::collections::HashMap<String, String>>,
    memu_tool_config: Option<MemuToolConfig>,
    canvas_context: Option<CanvasContext>,
    page_context: Option<PageContext>,
    canvas_cache: Option<crate::CanvasCache>,
) -> Result<ChatMessageWithContext, String>
where
    F: Fn(ChatEvent) + Send + Sync + 'static,
{
    let on_event: ChatEventCallback = Arc::new(on_event);

    // Resolve settings (from registry if provided, otherwise from storage)
    let settings_map = match external_settings {
        Some(s) => s,
        None => storage
            .get_all_settings_sync()
            .await
            .map_err(|e| e.to_string())?,
    };

    // Get provider config and model from settings
    let (provider_config, model) = {
        let provider_config = ProviderConfig::from_settings(&settings_map);

        if provider_config.provider_type == ProviderType::OpenRouter
            && provider_config.openrouter_api_key.is_none()
        {
            return Err(
                "OpenRouter API key not configured. Please set it in Settings.".to_string(),
            );
        }

        let model = match provider_config.provider_type {
            ProviderType::Ollama => provider_config.llm_model().to_string(),
            ProviderType::OpenAICompat => provider_config.llm_model().to_string(),
            ProviderType::OpenRouter => settings_map
                .get("chat_model")
                .cloned()
                .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string()),
        };

        (provider_config, model)
    };

    // Save user message
    storage
        .save_message_sync(conversation_id, "user", content)
        .await
        .map_err(|e| e.to_string())?;

    // Get conversation context
    let scope_tag_ids = storage
        .get_scope_tag_ids_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let scope_description = storage
        .get_scope_description_sync(&scope_tag_ids)
        .await
        .map_err(|e| e.to_string())?;

    // Get conversation messages via get_conversation_sync and convert to provider format
    let conversation = storage
        .get_conversation_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let messages = match conversation {
        Some(conv) => chat_messages_to_provider_messages(conv.messages),
        None => Vec::new(),
    };

    // Build message history for API, with canvas context appended to system prompt
    let system_prompt = chat_system_prompt(
        &settings_map,
        &scope_description,
        memu_tool_config.as_ref(),
        page_context.as_ref(),
        canvas_context.as_ref(),
    );
    let mut api_messages = vec![Message::system(system_prompt)];
    api_messages.extend(messages);

    // Truncate to fit context window for providers with limited context
    let api_messages = truncate_messages_to_context(
        api_messages,
        provider_config.context_length_for_model(&model),
    );

    // Create agent context
    let ctx = AgentContext {
        messages: api_messages,
        tools: ChatToolSession::new(
            storage.clone(),
            conversation_id.to_string(),
            scope_tag_ids,
            settings_map,
            memu_tool_config,
            page_context,
            canvas_context,
            canvas_cache,
            Arc::clone(&on_event),
        ),
    };

    // Run agent loop with canvas context
    let mut result = run_agent_loop(provider_config, model, ctx).await?;

    // Publish text, records and citation numbering together before emitting completion.
    {
        let saved_msg = storage
            .save_chat_reply_sync(
                conversation_id,
                None,
                &result.message.content,
                &result.tool_calls,
                &result.citations,
                0, // API citations are per reply, not a native sitting high-water mark.
            )
            .await
            .map_err(|e| e.to_string())?;

        result.message = saved_msg;

        for tool_call in &mut result.tool_calls {
            tool_call.message_id = result.message.id.clone();
        }
        for citation in &mut result.citations {
            citation.message_id = result.message.id.clone();
        }
    }

    // Emit completion event
    on_event(ChatEvent::Complete {
        conversation_id: conversation_id.to_string(),
        message: result.clone(),
    });

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AtomicCore, CreateAtomRequest};
    use tempfile::TempDir;

    #[tokio::test]
    async fn lifecycle_admission_and_publication_survive_restart_without_reopening() {
        use crate::models::{AtomicChatBackend, AtomicChatPhase, AtomicChatTurnStatus};
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("atomic.db");
        let core = AtomicCore::open_or_create(&db_path).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conv.conversation.id;
        let backend = AtomicChatBackend::ClaudeCode;
        assert!(matches!(
            core.admit_chat_turn(cid, "WrongOwner", "TestSoul", &backend, "No admission")
                .await,
            Err(crate::AtomicCoreError::Validation(_))
        ));
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_admission BEFORE UPDATE OF atomic_chat_state ON conversations
             BEGIN SELECT RAISE(ABORT, 'fictional admission failure'); END;",
            )
            .unwrap();
        assert!(
            core.admit_chat_turn(cid, "TestOwner", "TestSoul", &backend, "Rolled back input")
                .await
                .is_err()
        );
        let unchanged = core.get_conversation(cid).await.unwrap().unwrap();
        assert!(
            unchanged.messages.is_empty() && unchanged.conversation.atomic_chat_state.is_none()
        );
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_admission;")
            .unwrap();
        let user = core
            .admit_chat_turn(cid, "TestOwner", "TestSoul", &backend, "A fictional turn")
            .await
            .unwrap();
        let reopened = AtomicCore::open_or_create(&db_path).unwrap();
        let saved = reopened.get_conversation(cid).await.unwrap().unwrap();
        let state = saved.conversation.atomic_chat_state.as_ref().unwrap();
        assert_eq!(state.backend, backend);
        assert_eq!(state.active.as_ref().unwrap().user_message_id, user.id);
        assert_eq!(
            state.active.as_ref().unwrap().status,
            AtomicChatTurnStatus::Running
        );
        assert!(
            reopened
                .admit_chat_turn(cid, "TestOwner", "TestSoul", &backend, "Duplicate")
                .await
                .is_err()
        );
        let mismatch = reopened
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &AtomicChatBackend::Api,
                "Wrong backend",
            )
            .await
            .err()
            .unwrap();
        assert!(
            mismatch
                .to_string()
                .contains("original claude_code setting")
        );
        let records = ChatToolRecords {
            tool_calls: Vec::new(),
            citations: Vec::new(),
            citation_index: 0,
            prior_citations: Vec::new(),
        };
        assert!(
            reopened
                .save_chat_reply(cid, Some("other-turn"), "Wrong result", &records)
                .await
                .is_err()
        );
        assert!(
            reopened
                .save_chat_reply(cid, None, "Unbound result", &records)
                .await
                .is_err()
        );
        assert!(
            reopened
                .fail_chat_turn(cid, "other-turn", "Stale error", true)
                .await
                .is_err()
        );
        reopened
            .set_chat_phase(
                cid,
                "TestOwner",
                "TestSoul",
                &backend,
                &AtomicChatPhase::Closing,
            )
            .await
            .unwrap();
        assert!(
            reopened
                .set_chat_phase(
                    cid,
                    "TestOwner",
                    "TestSoul",
                    &backend,
                    &AtomicChatPhase::Closed
                )
                .await
                .is_err()
        );
        reopened
            .database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_completion BEFORE UPDATE OF atomic_chat_state ON conversations
             BEGIN SELECT RAISE(ABORT, 'fictional completion failure'); END;",
            )
            .unwrap();
        assert!(
            reopened
                .save_chat_reply(cid, Some(&user.id), "Retained reply", &records)
                .await
                .is_err()
        );
        let retained = reopened.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(retained.messages.len(), 1);
        assert_eq!(
            retained
                .conversation
                .atomic_chat_state
                .unwrap()
                .active
                .unwrap()
                .user_message_id,
            user.id
        );
        reopened
            .database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_completion;")
            .unwrap();
        reopened
            .save_chat_reply(cid, Some(&user.id), "Retained reply", &records)
            .await
            .unwrap();
        let completed = reopened.get_conversation(cid).await.unwrap().unwrap();
        let state = completed.conversation.atomic_chat_state.unwrap();
        assert_eq!(state.phase, AtomicChatPhase::Closing);
        assert!(state.active.is_none());
        assert!(
            reopened
                .save_chat_reply(cid, Some(&user.id), "Duplicate reply", &records)
                .await
                .is_err()
        );
        reopened
            .set_chat_phase(
                cid,
                "TestOwner",
                "TestSoul",
                &backend,
                &AtomicChatPhase::Closed,
            )
            .await
            .unwrap();
        reopened
            .set_chat_phase(
                cid,
                "TestOwner",
                "TestSoul",
                &backend,
                &AtomicChatPhase::Closed,
            )
            .await
            .unwrap();
        assert!(
            reopened
                .set_chat_phase(
                    cid,
                    "TestOwner",
                    "TestSoul",
                    &backend,
                    &AtomicChatPhase::Open
                )
                .await
                .is_err()
        );
        assert!(
            reopened
                .admit_chat_turn(cid, "TestOwner", "TestSoul", &backend, "After closure")
                .await
                .is_err()
        );
        let listed = reopened
            .get_conversations(None, 20, 0, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        assert_eq!(
            listed[0]
                .conversation
                .atomic_chat_state
                .as_ref()
                .unwrap()
                .phase,
            AtomicChatPhase::Closed
        );
    }

    #[tokio::test]
    async fn lifecycle_failures_and_legacy_api_history_block_blind_replay() {
        use crate::models::{AtomicChatBackend, AtomicChatTurnStatus};
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("atomic.db");
        let core = AtomicCore::open_or_create(&db_path).unwrap();
        for uncertain in [false, true] {
            let conv = core
                .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
                .await
                .unwrap();
            let cid = &conv.conversation.id;
            let user = core
                .admit_chat_turn(
                    cid,
                    "TestOwner",
                    "TestSoul",
                    &AtomicChatBackend::Api,
                    "Fictional turn",
                )
                .await
                .unwrap();
            core.fail_chat_turn(cid, &user.id, "Fictional failure", uncertain)
                .await
                .unwrap();
            let reopened = AtomicCore::open_or_create(&db_path).unwrap();
            let stored = reopened.get_conversation(cid).await.unwrap().unwrap();
            let active = stored
                .conversation
                .atomic_chat_state
                .unwrap()
                .active
                .unwrap();
            assert_eq!(
                active.status,
                if uncertain {
                    AtomicChatTurnStatus::Uncertain
                } else {
                    AtomicChatTurnStatus::Failed
                }
            );
            assert_eq!(active.error.as_deref(), Some("Fictional failure"));
            assert!(
                reopened
                    .admit_chat_turn(
                        cid,
                        "TestOwner",
                        "TestSoul",
                        &AtomicChatBackend::Api,
                        "Do not replay"
                    )
                    .await
                    .is_err()
            );
            for phase in [
                crate::models::AtomicChatPhase::Closing,
                crate::models::AtomicChatPhase::Closed,
            ] {
                reopened
                    .set_chat_phase(
                        cid,
                        "TestOwner",
                        "TestSoul",
                        &AtomicChatBackend::Api,
                        &phase,
                    )
                    .await
                    .unwrap();
            }
            let closed = reopened.get_conversation(cid).await.unwrap().unwrap();
            let state = closed.conversation.atomic_chat_state.unwrap();
            assert_eq!(state.phase, crate::models::AtomicChatPhase::Closed);
            assert_eq!(
                state.active.unwrap(),
                active,
                "closure must preserve terminal failure/uncertainty"
            );
        }
        let legacy = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &legacy.conversation.id;
        core.save_message(cid, "user", "Existing API turn")
            .await
            .unwrap();
        core.save_message(cid, "assistant", "Existing API reply")
            .await
            .unwrap();
        assert!(
            core.admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &AtomicChatBackend::ClaudeCode,
                "No reconstruction"
            )
            .await
            .is_err()
        );
        core.admit_chat_turn(
            cid,
            "TestOwner",
            "TestSoul",
            &AtomicChatBackend::Api,
            "Continue API",
        )
        .await
        .unwrap();
        assert_eq!(
            core.get_conversation(cid)
                .await
                .unwrap()
                .unwrap()
                .messages
                .len(),
            3
        );
        let unanswered = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        core.save_message(
            &unanswered.conversation.id,
            "user",
            "Unanswered legacy turn",
        )
        .await
        .unwrap();
        let error = core
            .admit_chat_turn(
                &unanswered.conversation.id,
                "TestOwner",
                "TestSoul",
                &AtomicChatBackend::Api,
                "No duplicate",
            )
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("unanswered user turn"));
        for has_history in [false, true] {
            let conv = core
                .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
                .await
                .unwrap();
            let cid = &conv.conversation.id;
            let backend = if has_history {
                AtomicChatBackend::Api
            } else {
                AtomicChatBackend::ClaudeCode
            };
            if has_history {
                core.save_message(cid, "user", "Old turn").await.unwrap();
                core.save_message(cid, "assistant", "Old reply")
                    .await
                    .unwrap();
            }
            core.set_chat_phase(
                cid,
                "TestOwner",
                "TestSoul",
                &backend,
                &crate::models::AtomicChatPhase::Closing,
            )
            .await
            .unwrap();
            let closing = core.get_conversation(cid).await.unwrap().unwrap();
            assert_eq!(closing.messages.len(), if has_history { 2 } else { 0 });
            let state = closing.conversation.atomic_chat_state.unwrap();
            assert!(state.active.is_none());
            assert_eq!(state.phase, crate::models::AtomicChatPhase::Closing);
            core.set_chat_phase(
                cid,
                "TestOwner",
                "TestSoul",
                &backend,
                &crate::models::AtomicChatPhase::Closed,
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_lifecycle_state_is_an_error_not_a_missing_conversation() {
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE conversations SET atomic_chat_state='invalid-json' WHERE id=?1",
                [&conv.conversation.id],
            )
            .unwrap();
        assert!(core.get_conversation(&conv.conversation.id).await.is_err());
        assert!(
            core.get_conversation("missing-conversation")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn claude_input_separates_presence_from_current_turn_without_replaying_history() {
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conversation.conversation.id;
        let presence = "  Original fictional Soul Presence.\nKeep these bytes.  ";
        core.save_message(cid, "system", presence).await.unwrap();
        core.save_message(cid, "user", "Earlier user dialogue")
            .await
            .unwrap();
        core.save_message(cid, "assistant", "Earlier assistant dialogue")
            .await
            .unwrap();
        let user = core
            .save_message(cid, "user", "Discuss this chart")
            .await
            .unwrap();
        let settings = HashMap::from([
            (
                "chat_prompt".to_string(),
                "Custom Atomic prefix".to_string(),
            ),
            (MEMU_RESPONSE_SENTENCES_SETTING.to_string(), "4".to_string()),
            (
                "embedding_model".to_string(),
                "test-embedding-model".to_string(),
            ),
        ]);
        let scope = MemuToolConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            user_id: "TestOwner".to_string(),
            soul_id: "TestSoul".to_string(),
        };
        let session = core
            .chat_tool_session(
                cid,
                &user.id,
                settings.clone(),
                scope.clone(),
                Some(PageContext {
                    view: Some("atom".to_string()),
                    atom_id: None,
                    atom_title: None,
                    atom_snippet: Some("Hidden page contents".to_string()),
                    wiki_tag_id: None,
                    wiki_tag_name: None,
                    selected_tag_id: None,
                }),
                Some(CanvasContext {
                    clusters: vec![CanvasClusterSummary {
                        label: "Fictional astronomy".to_string(),
                        atom_count: 12,
                    }],
                }),
                |_| {},
            )
            .await
            .unwrap();
        let initial = session.claude_input(true).await.unwrap();
        let system = initial.system_prompt.unwrap();
        assert!(system.starts_with("Custom Atomic prefix\n\n"));
        assert!(system.ends_with(presence));
        assert!(!system.contains("Fictional astronomy"));
        assert!(!system.contains("get_current_page_context"));
        assert!(!system.contains("4 sentences"));
        assert!(initial.prompt.contains("Fictional astronomy"));
        assert!(initial.prompt.contains("get_current_page_context"));
        assert!(
            initial
                .prompt
                .ends_with("Discuss this chart\n\n**respond with 4 sentences or fewer**")
        );
        for omitted in [
            presence,
            "Custom Atomic prefix",
            "Earlier user dialogue",
            "Earlier assistant dialogue",
            "Hidden page contents",
        ] {
            assert!(!initial.prompt.contains(omitted));
        }
        let resumed = session.claude_input(false).await.unwrap();
        assert!(resumed.system_prompt.is_none());
        assert_eq!(resumed.prompt, initial.prompt);
        assert_eq!(
            session.external_settings, settings,
            "input assembly must not replace tool settings"
        );
        assert!(
            !session
                .tools()
                .iter()
                .any(|tool| tool.name == "create_atom")
        );

        let next = core
            .save_message(cid, "user", "Next saved turn")
            .await
            .unwrap();
        let next_session = core
            .chat_tool_session(
                cid,
                &next.id,
                HashMap::from([(MEMU_RESPONSE_SENTENCES_SETTING.to_string(), "2".to_string())]),
                scope.clone(),
                None,
                None,
                |_| {},
            )
            .await
            .unwrap();
        let next_input = next_session.claude_input(false).await.unwrap();
        assert!(next_input.system_prompt.is_none());
        assert!(
            next_input
                .prompt
                .ends_with("Next saved turn\n\n**respond with 2 sentences or fewer**")
        );
        assert!(!next_input.prompt.contains("Fictional astronomy"));
        assert!(!next_input.prompt.contains("get_current_page_context"));
        assert!(!next_input.prompt.contains("Discuss this chart"));

        let other = core
            .create_conversation(&[], None, Some(("TestOwner", "OtherSoul")))
            .await
            .unwrap();
        let other_user = core
            .save_message(&other.conversation.id, "user", "Other turn")
            .await
            .unwrap();
        let other_session = core
            .chat_tool_session(
                &other.conversation.id,
                &other_user.id,
                HashMap::new(),
                MemuToolConfig {
                    soul_id: "OtherSoul".to_string(),
                    ..scope
                },
                None,
                None,
                |_| {},
            )
            .await
            .unwrap();
        assert!(
            other_session
                .claude_input(true)
                .await
                .err()
                .unwrap()
                .contains("no saved Soul Presence")
        );
        assert_eq!(
            core.get_conversation(cid)
                .await
                .unwrap()
                .unwrap()
                .messages
                .len(),
            5
        );
    }

    #[test]
    fn shared_system_prompt_preserves_api_layout() {
        let settings = HashMap::from([("chat_prompt".to_string(), "Custom prefix".to_string())]);
        let page: PageContext = serde_json::from_value(json!({"view": "atom"})).unwrap();
        let canvas = CanvasContext { clusters: vec![] };
        let scope = "Only the selected scope";
        assert_eq!(
            chat_system_prompt(&settings, scope, None, Some(&page), Some(&canvas)),
            format!(
                "Custom prefix\n\n{}{}{}",
                get_system_prompt(scope, None),
                get_page_context_system_prompt(),
                get_canvas_system_prompt(&canvas)
            )
        );
        assert_eq!(
            chat_system_prompt(&HashMap::new(), scope, None, None, None),
            get_system_prompt(scope, None)
        );
    }

    #[tokio::test]
    async fn durable_tools_preserve_numbering_retry_and_reply_membership() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path, query_param},
        };
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("atomic.db");
        let core = AtomicCore::open_or_create(&db_path).unwrap();
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conversation.conversation.id;
        let prior_message = core
            .save_message(cid, "assistant", "Earlier [2] and [7]")
            .await
            .unwrap();
        let prior = ChatToolRecords {
            tool_calls: Vec::new(),
            prior_citations: Vec::new(),
            citation_index: 7,
            citations: [2, 7]
                .into_iter()
                .map(|number| ChatCitation {
                    id: Uuid::new_v4().to_string(),
                    message_id: prior_message.id.clone(),
                    citation_index: number,
                    atom_id: format!("memory:old-{number}"),
                    chunk_index: Some(0),
                    excerpt: "Older evidence".to_string(),
                    relevance_score: None,
                })
                .collect(),
        };
        core.save_chat_tool_records(cid, &prior_message.id, &prior)
            .await
            .unwrap();
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM chat_citations WHERE citation_index=7", [])
            .unwrap();
        let server = MockServer::start().await;
        let captured_path = db_path.clone();
        Mock::given(method("GET"))
            .and(path("/integration/atomic/search"))
            .and(query_param("user_id", "TestOwner"))
            .and(query_param("soul_id", "TestSoul"))
            .respond_with(move |_request: &wiremock::Request| {
                let con = rusqlite::Connection::open(&captured_path).unwrap();
                let running: i32 = con
                    .query_row(
                        "SELECT COUNT(*) FROM chat_tool_calls WHERE status='running'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    running, 1,
                    "running state must commit before the tool starts"
                );
                ResponseTemplate::new(200).set_body_json(json!({"nodes": [
                    {"id": "memory:one", "kind": "memory", "summary": "A fictional voyage"},
                    {"id": "memory:two", "kind": "memory", "summary": "A fictional observatory"},
                ]}))
            })
            .expect(3)
            .mount(&server)
            .await;
        let scope = MemuToolConfig {
            base_url: server.uri(),
            user_id: "TestOwner".to_string(),
            soul_id: "TestSoul".to_string(),
        };
        let user = core
            .save_message(cid, "user", "Find the voyage")
            .await
            .unwrap();
        let mut session = core
            .chat_tool_session(
                cid,
                &user.id,
                HashMap::new(),
                scope.clone(),
                None,
                None,
                |_| {},
            )
            .await
            .unwrap();
        let result = session
            .execute(
                "native-first-search",
                "search_atoms",
                json!({"query": "voyage"}),
            )
            .await
            .unwrap();
        assert!(result.content.starts_with("[8]"));
        assert!(result.content.contains("[9]"));
        assert!(
            session
                .execute("native-unavailable", "create_atom", json!({}))
                .await
                .unwrap()
                .is_error
        );
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        let saved = saved
            .messages
            .iter()
            .find(|row| row.message.id == user.id)
            .unwrap();
        assert_eq!(saved.tool_calls.len(), 2);
        assert!(saved.tool_calls.iter().any(|call| call.status == "failed"));
        assert_eq!(
            saved
                .citations
                .iter()
                .map(|citation| citation.citation_index)
                .collect::<Vec<_>>(),
            [8, 9]
        );
        drop(session); // A failed provider turn must not discard completed tools.
        let reopened = AtomicCore::open_or_create(&db_path).unwrap();
        let mut retry = reopened
            .chat_tool_session(
                cid,
                &user.id,
                HashMap::new(),
                scope.clone(),
                None,
                None,
                |_| {},
            )
            .await
            .unwrap();
        assert!(
            retry
                .execute(
                    "native-first-search",
                    "search_atoms",
                    json!({"query": "voyage"})
                )
                .await
                .is_err()
        );
        let reply_text = "Earlier [2], new [8], and no known source for [777].";
        let records = retry.finish().for_reply(reply_text);
        assert_eq!(
            records
                .citations
                .iter()
                .map(|citation| citation.citation_index)
                .collect::<Vec<_>>(),
            [2, 8, 9]
        );
        let before_publication = reopened.get_conversation(cid).await.unwrap().unwrap();
        for failure in [
            "BEFORE INSERT ON chat_citations WHEN NEW.citation_index=9",
            "BEFORE UPDATE OF atomic_citation_index ON conversations",
        ] {
            reopened
                .database()
                .unwrap()
                .conn
                .lock()
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TRIGGER fail_publication {failure}
                    BEGIN SELECT RAISE(ABORT, 'fictional publication failure'); END;"
                ))
                .unwrap();
            assert!(
                reopened
                    .save_chat_reply(cid, None, reply_text, &records)
                    .await
                    .is_err()
            );
            let unchanged = reopened.get_conversation(cid).await.unwrap().unwrap();
            assert_eq!(unchanged.messages.len(), before_publication.messages.len());
            assert_eq!(
                unchanged.conversation.updated_at,
                before_publication.conversation.updated_at
            );
            let unanswered = unchanged
                .messages
                .iter()
                .find(|row| row.message.id == user.id)
                .unwrap();
            assert_eq!(unanswered.tool_calls.len(), 2);
            assert_eq!(unanswered.citations.len(), 2);
            let db = reopened.database().unwrap();
            let conn = db.conn.lock().unwrap();
            let indexed: i32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM chat_messages_fts WHERE content=?1",
                    [reply_text],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                indexed, 0,
                "failed publication must not leak a search result"
            );
            conn.execute_batch("DROP TRIGGER fail_publication;")
                .unwrap();
        }
        let reply = reopened
            .save_chat_reply(cid, None, reply_text, &records)
            .await
            .unwrap();
        let saved = reopened.get_conversation(cid).await.unwrap().unwrap();
        let old = saved
            .messages
            .iter()
            .find(|row| row.message.id == prior_message.id)
            .unwrap();
        assert_eq!(old.citations[0].id, prior.citations[0].id);
        let answered = saved
            .messages
            .iter()
            .find(|row| row.message.id == user.id)
            .unwrap();
        assert!(answered.tool_calls.is_empty() && answered.citations.is_empty());
        let published = saved
            .messages
            .iter()
            .find(|row| row.message.id == reply.id)
            .unwrap();
        assert_eq!(published.tool_calls.len(), 2);
        assert_eq!(published.citations.len(), 3);
        let next_user = reopened
            .save_message(cid, "user", "Find another detail")
            .await
            .unwrap();
        assert!(
            reopened
                .save_chat_tool_records(cid, &next_user.id, &records)
                .await
                .is_err(),
            "a new turn must not take records from an older reply"
        );
        let mut next = reopened
            .chat_tool_session(
                cid,
                &next_user.id,
                HashMap::new(),
                scope.clone(),
                None,
                None,
                |_| {},
            )
            .await
            .unwrap();
        let result = next
            .execute(
                "native-next-search",
                "search_atoms",
                json!({"query": "detail"}),
            )
            .await
            .unwrap();
        assert!(result.content.starts_with("[10]"));
        drop(next);
        reopened
            .database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM chat_citations WHERE message_id=?1",
                [&next_user.id],
            )
            .unwrap();
        let after_cleanup = reopened
            .save_message(cid, "user", "After cleanup")
            .await
            .unwrap();
        let mut after_cleanup = reopened
            .chat_tool_session(
                cid,
                &after_cleanup.id,
                HashMap::new(),
                scope.clone(),
                None,
                None,
                |_| {},
            )
            .await
            .unwrap();
        let result = after_cleanup
            .execute(
                "after-cleanup-search",
                "search_atoms",
                json!({"query": "detail"}),
            )
            .await
            .unwrap();
        assert!(
            result.content.starts_with("[12]"),
            "deleted failed-turn citations must not reset the stored high-water mark"
        );

        let other = core
            .create_conversation(&[], None, Some(("TestOwner", "OtherSoul")))
            .await
            .unwrap();
        let other_id = &other.conversation.id;
        let other_user = core
            .save_message(other_id, "user", "Unrelated sitting")
            .await
            .unwrap();
        assert!(
            core.save_chat_tool_records(cid, &other_user.id, &records)
                .await
                .is_err()
        );
        let other_scope = MemuToolConfig {
            soul_id: "OtherSoul".to_string(),
            ..scope
        };
        let mut attempt = core
            .chat_tool_session(
                other_id,
                &other_user.id,
                HashMap::new(),
                other_scope,
                None,
                None,
                |_| {},
            )
            .await
            .unwrap()
            .finish();
        attempt.tool_calls = records.tool_calls.clone();
        attempt.tool_calls[0].id = "fresh-before-conflict".to_string();
        attempt.citations = records.citations.clone();
        attempt.citation_index = 100;
        assert!(
            core.save_chat_tool_records(other_id, &other_user.id, &attempt)
                .await
                .is_err()
        );
        assert!(
            core.save_chat_reply(other_id, None, "Must not publish across sittings", &attempt)
                .await
                .is_err()
        );
        let other = core.get_conversation(other_id).await.unwrap().unwrap();
        assert_eq!(
            other.messages.len(),
            1,
            "conflicting records must roll back the assistant row"
        );
        assert!(
            other.messages[0].tool_calls.is_empty(),
            "conflict must roll back earlier inserts"
        );
        assert_eq!(
            core.storage
                .get_chat_citation_index_sync(other_id)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn checkpoint_failure_prevents_tool_execution() {
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conversation.conversation.id;
        let user = core
            .save_message(cid, "user", "Focus the canvas")
            .await
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let mut session = core
            .chat_tool_session(
                cid,
                &user.id,
                HashMap::new(),
                MemuToolConfig {
                    base_url: "http://127.0.0.1:1".to_string(),
                    user_id: "TestOwner".to_string(),
                    soul_id: "TestSoul".to_string(),
                },
                None,
                Some(CanvasContext {
                    clusters: Vec::new(),
                }),
                move |event| captured.lock().unwrap().push(event),
            )
            .await
            .unwrap();
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch("CREATE TRIGGER fail_start BEFORE INSERT ON chat_tool_calls BEGIN SELECT RAISE(ABORT, 'fictional save failure'); END;")
            .unwrap();
        assert!(
            session
                .execute(
                    "unstarted-tool",
                    "focus_atom",
                    json!({"atom_id": "fictional"})
                )
                .await
                .err()
                .unwrap()
                .contains("Tool was not executed")
        );
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(session.tool_calls_record[0].status, "failed");
        assert_eq!(
            session.tool_calls_record[0].tool_output.as_ref().unwrap()["executed"],
            false
        );
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_start;")
            .unwrap();
        session
            .execute("other-tool", "focus_atom", json!({"atom_id": "another"}))
            .await
            .unwrap();
        assert!(
            session
                .execute(
                    "unstarted-tool",
                    "focus_atom",
                    json!({"atom_id": "changed"})
                )
                .await
                .is_err()
        );
        session
            .execute(
                "unstarted-tool",
                "focus_atom",
                json!({"atom_id": "fictional"}),
            )
            .await
            .unwrap();
        let records = session.finish();
        assert_eq!(records.tool_calls.len(), 2);
        assert_eq!(records.tool_calls[0].status, "complete");
        assert!(
            records.tool_calls[0]
                .tool_output
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap()
                .contains("fictional")
        );
        assert!(
            records.tool_calls[1]
                .tool_output
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap()
                .contains("another")
        );
    }

    #[tokio::test]
    async fn stopped_writer_running_records_become_unknown_and_refuse_continuation() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("atomic.db");
        let core = AtomicCore::open_or_create(&db_path).unwrap();
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conversation.conversation.id;
        let user = core
            .save_message(cid, "user", "An interrupted turn")
            .await
            .unwrap();
        let scope = MemuToolConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            user_id: "TestOwner".to_string(),
            soul_id: "TestSoul".to_string(),
        };
        let mut session = core
            .chat_tool_session(
                cid,
                &user.id,
                HashMap::new(),
                scope.clone(),
                None,
                None,
                |_| {},
            )
            .await
            .unwrap();
        session.tool_calls_record.push(ChatToolCall {
            id: "interrupted-tool".to_string(),
            message_id: user.id.clone(),
            tool_name: "edit_atom".to_string(),
            tool_input: json!({"atom_id": "memory:fictional"}),
            tool_output: Some(json!({"partial": "known observation"})),
            status: "running".to_string(),
            created_at: Utc::now().to_rfc3339(),
            completed_at: None,
        });
        session.checkpoint().await.unwrap();
        drop(session); // Simulated stopped writer, not a live tool being cancelled.
        let reopened = AtomicCore::open_or_create(&db_path).unwrap();
        for _ in 0..2 {
            let error = reopened
                .chat_tool_session(
                    cid,
                    &user.id,
                    HashMap::new(),
                    scope.clone(),
                    None,
                    None,
                    |_| {},
                )
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("outcome is unknown"));
        }
        let saved = reopened.get_conversation(cid).await.unwrap().unwrap();
        let call = &saved.messages[0].tool_calls[0];
        assert_eq!(call.status, "failed");
        assert_eq!(call.tool_output.as_ref().unwrap()["outcome_unknown"], true);
        assert_eq!(
            call.tool_output.as_ref().unwrap()["previous_output"]["partial"],
            "known observation"
        );
        assert!(call.completed_at.is_none());
    }

    #[tokio::test]
    async fn ambiguous_api_citations_refuse_backend_migration_without_rewriting_history() {
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conversation.conversation.id;
        for source in ["one", "two"] {
            core.storage
                .save_chat_reply_sync(
                    cid,
                    None,
                    "API reply [1]",
                    &[],
                    &[ChatCitation {
                        id: Uuid::new_v4().to_string(),
                        message_id: String::new(),
                        citation_index: 1,
                        atom_id: format!("memory:{source}"),
                        chunk_index: None,
                        excerpt: source.to_string(),
                        relevance_score: None,
                    }],
                    0,
                )
                .await
                .unwrap();
        }
        let user = core.save_message(cid, "user", "Continue").await.unwrap();
        let error = core
            .chat_tool_session(
                cid,
                &user.id,
                HashMap::new(),
                MemuToolConfig {
                    base_url: "http://127.0.0.1:1".to_string(),
                    user_id: "TestOwner".to_string(),
                    soul_id: "TestSoul".to_string(),
                },
                None,
                None,
                |_| {},
            )
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("Restore its original chat backend")
        );
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(saved.messages[0].citations[0].atom_id, "memory:one");
        assert_eq!(saved.messages[1].citations[0].atom_id, "memory:two");
    }

    #[tokio::test]
    async fn failed_completion_checkpoint_retains_the_outcome_without_replaying() {
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let conversation = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conversation.conversation.id;
        let user = core
            .save_message(cid, "user", "Focus the canvas")
            .await
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let mut session = core
            .chat_tool_session(
                cid,
                &user.id,
                HashMap::new(),
                MemuToolConfig {
                    base_url: "http://127.0.0.1:1".to_string(),
                    user_id: "TestOwner".to_string(),
                    soul_id: "TestSoul".to_string(),
                },
                None,
                Some(CanvasContext {
                    clusters: Vec::new(),
                }),
                move |event| captured.lock().unwrap().push(event),
            )
            .await
            .unwrap();
        core.database().unwrap().conn.lock().unwrap().execute_batch(
            "CREATE TRIGGER fail_completion BEFORE UPDATE ON chat_tool_calls WHEN NEW.status='complete'
             BEGIN SELECT RAISE(ABORT, 'fictional save failure'); END;"
        ).unwrap();
        assert!(
            session
                .execute(
                    "completed-tool",
                    "focus_atom",
                    json!({"atom_id": "fictional"})
                )
                .await
                .err()
                .unwrap()
                .contains("Tool finished")
        );
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(saved.messages[0].tool_calls[0].status, "running");
        assert!(
            session
                .execute(
                    "completed-tool",
                    "focus_atom",
                    json!({"atom_id": "fictional"})
                )
                .await
                .is_err()
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, ChatEvent::CanvasAction { .. }))
                .count(),
            1
        );
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            ChatEvent::ToolComplete {
                is_error: false,
                ..
            }
        )));
        let records = session.finish();
        assert_eq!(records.tool_calls[0].status, "complete");
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_completion;")
            .unwrap();
        core.save_chat_tool_records(cid, &user.id, &records)
            .await
            .unwrap();
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(saved.messages[0].tool_calls[0].status, "complete");
    }

    #[test]
    fn citation_counter_migration_upgrades_v23_and_is_repeatable() {
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).unwrap();
        let database = core.database().unwrap();
        let con = database.conn.lock().unwrap();
        con.execute_batch(
            "ALTER TABLE conversations DROP COLUMN atomic_citation_index;
             ALTER TABLE conversations DROP COLUMN atomic_chat_state; PRAGMA user_version=23;",
        )
        .unwrap();
        crate::db::Database::run_migrations(&con).unwrap();
        crate::db::Database::run_migrations(&con).unwrap();
        let version: i32 = con
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 25);
        let columns: i32 = con.query_row("SELECT COUNT(*) FROM pragma_table_info('conversations') WHERE name='atomic_citation_index'", [], |row| row.get(0)).unwrap();
        assert_eq!(columns, 1);
    }

    #[tokio::test]
    async fn chat_tool_session_preserves_dispatch_citations_and_events() {
        let dir = TempDir::new().expect("create tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).expect("open core");
        let atom = core
            .create_atom(
                CreateAtomRequest {
                    content: "# Observatory\n\nA fictional observatory tracks bright comets."
                        .to_string(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .expect("create atom")
            .expect("atom was not skipped");
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let mut session = ChatToolSession::new(
            core.storage.clone(),
            "test-conversation".to_string(),
            Vec::new(),
            HashMap::new(),
            None,
            Some(PageContext {
                view: Some("atom".to_string()),
                atom_id: Some(atom.atom.id.clone()),
                atom_title: None,
                atom_snippet: None,
                wiki_tag_id: None,
                wiki_tag_name: None,
                selected_tag_id: None,
            }),
            None,
            None,
            Arc::new(move |event| captured.lock().unwrap().push(event)),
        );
        for (call_id, expected) in [("call-1", "[1]"), ("call-2", "[2]")] {
            let result = session
                .execute(call_id, "get_atom", json!({"atom_id": atom.atom.id}))
                .await
                .expect("dispatch tool");
            assert!(!result.is_error);
            assert!(result.content.starts_with(expected));
            assert!(result.content.contains("bright comets"));
        }
        let page = session
            .execute("call-page", "get_current_page_context", json!({}))
            .await
            .expect("dispatch tool");
        let page: serde_json::Value = serde_json::from_str(&page.content).expect("page result");
        assert_eq!(page["visible_atom"]["citation_index"], 3);
        let refused = session
            .execute(
                "call-unavailable",
                "focus_atom",
                json!({"atom_id": atom.atom.id}),
            )
            .await
            .expect("dispatch tool");
        assert!(refused.is_error);
        assert!(refused.content.contains("Tool unavailable"));
        assert_eq!(session.citations.len(), 3);
        assert_eq!(session.tool_calls_record.len(), 4);
        assert_eq!(session.tool_calls_record[3].status, "failed");
        let record = &session.tool_calls_record[0];
        assert_eq!(record.id, "call-1");
        assert_eq!(record.tool_input["atom_id"], atom.atom.id);
        assert!(
            record
                .tool_output
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("[1]")
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 8);
        assert!(
            matches!(&events[0], ChatEvent::ToolStart { tool_call_id, .. } if tool_call_id == "call-1")
        );
        assert!(
            matches!(&events[1], ChatEvent::ToolComplete { tool_call_id, results_count: 1, .. } if tool_call_id == "call-1")
        );
        assert!(matches!(
            &events[7],
            ChatEvent::ToolComplete {
                results_count: 0,
                is_error: true,
                ..
            }
        ));
        let records = session.finish();
        assert_eq!(records.tool_calls.len(), 4);
        assert_eq!(records.citations.len(), 3);
        assert_eq!(records.citations[2].citation_index, 3);
        assert_eq!(records.citations[2].atom_id, atom.atom.id);
    }

    #[tokio::test]
    async fn chat_tool_session_keeps_integrated_create_atom_unavailable() {
        let dir = TempDir::new().expect("create tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).expect("open core");
        let mut session = ChatToolSession::new(
            core.storage.clone(),
            "test-conversation".to_string(),
            Vec::new(),
            HashMap::new(),
            Some(MemuToolConfig {
                base_url: "http://127.0.0.1:1".to_string(),
                user_id: "TestOwner".to_string(),
                soul_id: "TestSoul".to_string(),
            }),
            None,
            None,
            None,
            Arc::new(|_| {}),
        );
        assert!(
            !session
                .tools()
                .iter()
                .any(|tool| tool.name == "create_atom")
        );
        let result = session
            .execute(
                "call-refused",
                "create_atom",
                json!({"content": "must not be created"}),
            )
            .await
            .expect("dispatch tool");
        assert!(result.is_error);
        assert!(result.content.contains("Tool unavailable"));
        assert!(session.citations.is_empty());
        assert_eq!(session.tool_calls_record.len(), 1);
    }

    #[tokio::test]
    async fn get_atom_enforces_conversation_tag_scope() {
        let dir = TempDir::new().expect("create tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("atomic.db")).expect("open core");
        let birds = core
            .create_tag("Birds", None)
            .await
            .expect("create scope tag");
        let pelicans = core
            .create_tag("Pelicans", Some(&birds.id))
            .await
            .expect("create child tag");
        let private = core
            .create_tag("Private", None)
            .await
            .expect("create private tag");
        let atom = core
            .create_atom(
                CreateAtomRequest {
                    content: "Pelicans dive for fish.".to_string(),
                    tag_ids: vec![pelicans.id],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .expect("create atom")
            .expect("atom was not skipped");

        let allowed = execute_get_atom(
            &core.storage,
            &atom.atom.id,
            0,
            GET_ATOM_DEFAULT_LIMIT,
            &[birds.id],
            None,
        )
        .await
        .expect("parent scope admits child-tagged atom")
        .expect("atom exists");
        assert!(allowed.contains("Pelicans dive for fish."));

        let refused = execute_get_atom(
            &core.storage,
            &atom.atom.id,
            0,
            GET_ATOM_DEFAULT_LIMIT,
            &[private.id.clone()],
            None,
        )
        .await
        .expect_err("out-of-scope atom must be refused");
        assert!(refused.contains("outside this conversation's scope"));
        assert!(!refused.contains("Pelicans dive for fish."));

        let context = PageContext {
            view: Some("atom".to_string()),
            atom_id: Some(atom.atom.id.clone()),
            atom_title: Some("Private".to_string()),
            atom_snippet: Some("must not leak".to_string()),
            wiki_tag_id: None,
            wiki_tag_name: None,
            selected_tag_id: None,
        };
        let refused =
            execute_get_current_page_context(&core.storage, Some(&context), &[private.id], None)
                .await
                .expect_err("page context must enforce conversation scope");
        assert!(refused.contains("outside this conversation's scope"));
    }

    #[test]
    fn truncate_keeps_all_leading_system_messages() {
        let messages = vec![
            Message::system("atomic base prompt"),
            Message::system("memu entry snapshot"),
            Message::user("older user message ".repeat(200)),
            Message::assistant("older assistant message ".repeat(200)),
            Message::user("latest"),
        ];

        let truncated = truncate_messages_to_context(messages, Some(80));
        let contents: Vec<&str> = truncated
            .iter()
            .filter_map(|m| m.content.as_deref())
            .collect();

        assert!(contents.contains(&"atomic base prompt"));
        assert!(contents.contains(&"memu entry snapshot"));
        assert!(contents.contains(&"latest"));
        assert!(!contents.iter().any(|c| c.starts_with("older user message")));
    }

    #[test]
    fn memu_response_limit_is_last_transient_user_message() {
        let messages =
            append_memu_response_limit(vec![Message::system("system"), Message::user("hello")], 4);

        let last = messages.last().expect("reminder");
        assert_eq!(last.role, MessageRole::User);
        assert_eq!(
            last.content.as_deref(),
            Some("**respond with 4 sentences or fewer**")
        );
    }

    #[test]
    fn memu_system_prompt_uses_user_id() {
        let prompt = get_system_prompt(
            "You have access to ALL atoms.",
            Some(&MemuToolConfig {
                base_url: "http://127.0.0.1:8099".to_string(),
                user_id: "Marcos".to_string(),
                soul_id: "Siri".to_string(),
            }),
        );

        assert!(prompt.contains("You are with Marcos in the app Atomic"));
        assert!(prompt.contains("In Atomic, Marcos is the user."));
        assert!(prompt.contains("Guidelines for Atomic:"));
        assert!(!prompt.contains("helpful AI assistant"));
    }

    #[test]
    fn memu_node_to_atom_uses_stable_category_ids() {
        let atom = memu_node_to_atom(MemuNode {
            id: "memory:m1".to_string(),
            kind: "memory".to_string(),
            label: "Memory".to_string(),
            summary: "Summary".to_string(),
            memory_type: Some("episode".to_string()),
            happened_at: None,
            created_at: None,
            updated_at: None,
            category_ids: vec!["c1".to_string()],
            category_names: vec!["Core".to_string()],
            score: None,
        });

        assert_eq!(atom.tags[0].id, "category:c1");
        assert_eq!(atom.tags[0].name, "Core");
    }

    #[test]
    fn memu_id_prefixes_include_entities() {
        assert!(is_memu_id("memory:m1"));
        assert!(is_memu_id("category:c1"));
        assert!(is_memu_id("entity:e1"));
        assert!(!is_memu_id("local-atom"));
    }

    #[test]
    fn default_system_prompt_is_unchanged_without_memu() {
        let prompt = get_system_prompt("You have access to ALL atoms.", None);

        assert!(prompt.contains("You are a helpful AI assistant"));
        assert!(prompt.contains("Guidelines:"));
        assert!(!prompt.contains("Guidelines for Atomic:"));
        assert!(
            prompt
                .contains("cite the exact [N] marker shown in the tool result; never renumber it")
        );
        assert!(!prompt.contains("N is a sequential number"));
    }
}
