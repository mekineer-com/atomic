//! Chat agent loop with tool calling and streaming
//!
//! Provides the agentic chat loop that searches the knowledge base,
//! retrieves atoms, and generates responses with citations.
//! Uses a callback-based event system (same pattern as EmbeddingEvent).

use crate::atom_edit::{AtomEditOperation, apply_atom_edits};
use crate::chunking::count_tokens;
use crate::embedding::EmbeddingEvent;
use crate::models::{
    Atom, AtomKind, AtomWithTags, ChatCitation, ChatMessageWithContext, ChatToolCall,
    SemanticSearchResult, Tag,
};
use crate::providers::traits::LlmConfig;
use crate::providers::types::{
    GenerationParams, Message, MessageRole, StreamDelta, ToolCall, ToolDefinition,
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
pub const CHAT_RESPONSE_MAX_TOKENS: u32 = 4000;

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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CanvasContext {
    pub clusters: Vec<CanvasClusterSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    description: String,
    summaries_revision: Option<i64>,
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

enum EditAtomError {
    Failed(String),
    OutcomeUnknown(String),
}

impl From<String> for EditAtomError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

async fn execute_edit_atom(
    storage: &StorageBackend,
    tool_args: &serde_json::Value,
    external_settings: Option<std::collections::HashMap<String, String>>,
    canvas_cache: Option<&crate::CanvasCache>,
    on_embedding_event: Arc<dyn Fn(EmbeddingEvent) + Send + Sync + 'static>,
    memu_tool_config: Option<&MemuToolConfig>,
) -> Result<Option<AtomWithTags>, EditAtomError> {
    let atom_id = tool_args["atom_id"].as_str().unwrap_or("");
    if let Some(config) = memu_tool_config {
        if !is_memu_id(atom_id) {
            return Err("memU-backed sessions can only edit memU atoms"
                .to_string()
                .into());
        }
        if atom_id.starts_with("entity:") {
            return Err("memU entity atoms are read-only in this slice"
                .to_string()
                .into());
        }
        let Some(mut existing) = fetch_memu_node(config, atom_id).await? else {
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
            return Err("Edits did not change the atom content".to_string().into());
        }
        if summary.trim().is_empty() {
            return Err("Cannot update an atom to empty content".to_string().into());
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
        let mut payload = json!({
            "summary": summary.trim(),
            "displayed_summary": existing.summary,
            "edited_by": "atomic:agent",
        });
        if endpoint == "category" {
            payload["summaries_revision"] = json!(
                existing
                    .summaries_revision
                    .ok_or_else(|| "Category review revision is missing".to_string())?
            );
            payload["displayed_title"] = json!(existing.label);
            payload["displayed_description"] = json!(existing.description);
        }
        let response = client
            .patch(format!("{}/{}/{}", config.base_url, endpoint, atom_id))
            .query(&[
                ("user_id", config.user_id.as_str()),
                ("soul_id", config.soul_id.as_str()),
            ])
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                let message = format!("memU edit_atom request failed: {e}");
                if e.is_connect() {
                    EditAtomError::Failed(message)
                } else {
                    EditAtomError::OutcomeUnknown(message)
                }
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = format!("memU edit_atom failed ({status}): {body}");
            return Err(if status.is_server_error() {
                EditAtomError::OutcomeUnknown(message)
            } else {
                EditAtomError::Failed(message)
            });
        }
        // The successful write is authoritative; its optional body is not needed.
        existing.summary = summary.trim().to_string();
        return Ok(Some(memu_node_to_atom(existing)));
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
        return Err("Edits did not change the atom content".to_string().into());
    }
    if content.trim().is_empty() {
        return Err("Cannot update an atom to empty content".to_string().into());
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

fn chat_identity(memu_tool_config: Option<&MemuToolConfig>) -> (String, &'static str) {
    match memu_tool_config {
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
    }
}

fn get_system_prompt(scope_description: &str, memu_tool_config: Option<&MemuToolConfig>) -> String {
    let (intro, guidelines) = chat_identity(memu_tool_config);
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
fn chat_messages_to_provider_messages<'a>(
    messages: impl IntoIterator<Item = &'a crate::models::ChatMessageWithContext>,
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
                content: Some(m.message.content.clone()),
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
    pub fn from_saved_message(message: &ChatMessageWithContext) -> Self {
        Self {
            tool_calls: message.tool_calls.clone(),
            citations: message.citations.clone(),
            citation_index: 0,
            prior_citations: vec![],
        }
    }
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

fn saved_tool_output(call: &ChatToolCall) -> Result<String, String> {
    match &call.tool_output {
        Some(serde_json::Value::String(output)) => Ok(output.clone()),
        Some(output) => Ok(output.to_string()),
        None => Err("Saved tool call has no recorded outcome".to_string()),
    }
}

/// New input is inserted once; saved input must already have managed admission.
pub enum ApiChatInput<'a> {
    New(&'a str),
    Saved(&'a str),
    Recap { id: &'a str, instruction: &'a str },
}

fn recap_messages(
    conversation: &crate::ConversationWithMessages,
    control_id: &str,
    instruction: &str,
) -> Vec<Message> {
    let mut messages = chat_messages_to_provider_messages(
        crate::chat::dialogue_messages(conversation, Some(control_id))
            .into_iter()
            .filter(|row| !row.message.content.trim().is_empty()),
    );
    let mut actions = Vec::new();
    for row in &conversation.messages {
        for call in &row.tool_calls {
            let mut input = serde_json::Map::new();
            for key in ["atom_id", "query", "cluster_label"] {
                if let Some(value) = call.tool_input.get(key).and_then(|v| v.as_str()) {
                    input.insert(
                        key.into(),
                        json!(value.chars().take(160).collect::<String>()),
                    );
                }
            }
            if let Some(edits) = call.tool_input.get("edits").and_then(|v| v.as_array()) {
                input.insert(
                    "operations".into(),
                    json!(
                        edits
                            .iter()
                            .filter_map(|edit| edit.get("operation").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                    ),
                );
            }
            let unknown = call.outcome_unknown();
            let output = call.tool_output.as_ref();
            let text = output.and_then(|v| v.as_str());
            let outcome = if unknown {
                json!("unknown")
            } else if call.status == "failed" {
                json!(
                    text.or_else(|| output.and_then(|v| v.get("error")?.as_str()))
                        .unwrap_or("failed")
                        .chars()
                        .take(240)
                        .collect::<String>()
                )
            } else if call.tool_name == "edit_atom" {
                let result =
                    text.and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
                json!({"atom_id": result.as_ref().and_then(|v| v.get("atom_id")),
                    "title": result.as_ref().and_then(|v| v.get("title"))})
            } else if text == Some("") {
                json!("no results")
            } else {
                json!("completed")
            };
            actions.push(json!({
                "tool": call.tool_name,
                "input": input,
                "status": call.status,
                "outcome": outcome,
            }));
        }
    }
    messages.push(Message::user(format!(
        "{instruction}\n\nRecorded actions (status is authoritative; unknown outcomes are not successes):\n{}",
        serde_json::to_string(&actions).expect("serializable action digest")
    )));
    messages
}

// Pure request assembly: admission/checkpointing stays with the executing caller.
fn api_request_messages(
    conversation: &crate::ConversationWithMessages,
    tools: &ChatToolSession,
    scope_description: &str,
    recap: Option<(&str, &str)>,
    new_content: Option<&str>,
) -> Result<Vec<Message>, String> {
    let system_prompt = if recap.is_some() {
        let (intro, _) = chat_identity(tools.memu_tool_config.as_ref());
        let base = format!(
            "{intro}\n\n{scope_description}\n\nReflect on this sitting using only the supplied dialogue and recorded actions. No tools are available. Distinguish completed actions, failures and unknown outcomes; do not invent missing results."
        );
        match tools
            .external_settings
            .get("chat_prompt")
            .filter(|s| !s.is_empty())
        {
            Some(prefix) => format!("{prefix}\n\n{base}"),
            None => base,
        }
    } else {
        chat_system_prompt(
            &tools.external_settings,
            scope_description,
            tools.memu_tool_config.as_ref(),
            tools.page_context.as_ref(),
            tools.canvas_context.as_ref(),
        )
    };
    let mut messages = vec![Message::system(system_prompt)];
    if let Some((control_id, instruction)) = recap {
        messages.extend(recap_messages(conversation, control_id, instruction));
    } else {
        let dialogue = if tools.memu_tool_config.is_some() {
            crate::chat::dialogue_messages(conversation, None)
        } else {
            conversation.messages.iter().collect()
        };
        messages.extend(chat_messages_to_provider_messages(dialogue));
        if let Some(content) = new_content {
            messages.push(Message::user(content));
        }
    }
    for call in &tools.tool_calls_record {
        messages.push(Message::assistant_with_tool_calls(vec![ToolCall {
            id: call.id.clone(),
            call_type: Some("function".to_string()),
            function: None,
            name: Some(call.tool_name.clone()),
            arguments: Some(call.tool_input.to_string()),
        }]));
        messages.push(Message::tool_result(&call.id, saved_tool_output(call)?));
    }
    Ok(messages)
}

/// Project the untrimmed API request without saving input or checkpointing tools.
pub(crate) async fn estimate_chat_request_tokens(
    storage: StorageBackend,
    conversation_id: &str,
    request: &crate::AtomicChatRequest,
    settings: HashMap<String, String>,
    scope: MemuToolConfig,
    fresh_sitting: bool,
) -> Result<usize, String> {
    let mut conversation = storage
        .get_conversation_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Conversation not found")?;
    if conversation.conversation.user_id.as_deref() != Some(scope.user_id.as_str())
        || conversation.conversation.soul_id.as_deref() != Some(scope.soul_id.as_str())
    {
        return Err("Chat owner/Soul does not match".to_string());
    }
    if fresh_sitting {
        conversation.messages.retain(|row| row.message.role == "system");
    }
    let last = conversation
        .messages
        .iter()
        .rev()
        .find(|row| matches!(row.message.role.as_str(), "user" | "assistant"))
        .map(|row| &row.message);
    let state = crate::chat::bound_chat_state(
        conversation.conversation.atomic_chat_state.clone(),
        &crate::AtomicChatBackend::Api,
        last.map(|message| message.role.as_str()),
    )
    .map_err(|error| error.to_string())?;
    let (saved_input, admission_content) = crate::chat::chat_admission_input(&state, last, request);
    let (page, canvas) = match request {
        crate::AtomicChatRequest::New {
            content,
            page_context,
            canvas_context,
            ..
        } => {
            if content.is_empty() && page_context.is_none() && canvas_context.is_none() {
                state
                    .active
                    .as_ref()
                    .filter(|_| state.phase == crate::AtomicChatPhase::Open)
                    .map(|turn| (turn.page_context.clone(), turn.canvas_context.clone()))
                    .unwrap_or_default()
            } else {
                (page_context.clone(), canvas_context.clone())
            }
        }
        crate::AtomicChatRequest::Retry { user_message_id } => {
            let active = state.active.as_ref();
            if active.is_some_and(|turn| turn.user_message_id != *user_message_id) {
                return Err("Retry sizing requires the matching saved turn".to_string());
            }
            active
                .map(|turn| (turn.page_context.clone(), turn.canvas_context.clone()))
                .unwrap_or_default()
        }
        crate::AtomicChatRequest::Recap { .. } => (None, None),
    };
    let tags = storage
        .get_scope_tag_ids_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let description = storage
        .get_scope_description_sync(&tags)
        .await
        .map_err(|e| e.to_string())?;
    let mut tools = ChatToolSession::new(
        storage,
        conversation_id.into(),
        tags,
        settings,
        Some(scope),
        page,
        canvas,
        None,
        Arc::new(|_| {}),
    );
    let mut recap = None;
    let mut content = None;
    match request {
        crate::AtomicChatRequest::New { .. } => {
            content = Some(admission_content);
            // Standing-context projection includes already-known results from the
            // unfinished turn, never inventing an output for an in-flight tool.
            if admission_content.is_empty() && state.phase == crate::AtomicChatPhase::Open {
                let user = state
                    .active
                    .as_ref()
                    .and_then(|turn| {
                        conversation
                            .messages
                            .iter()
                            .find(|row| row.message.id == turn.user_message_id)
                    })
                    .or_else(|| {
                        last.filter(|message| message.role == "user")
                            .and_then(|message| {
                                conversation
                                    .messages
                                    .iter()
                                    .find(|row| row.message.id == message.id)
                            })
                    });
                if let Some(user) = user {
                    tools.tool_calls_record = user
                        .tool_calls
                        .iter()
                        .filter(|call| {
                            !call.outcome_unknown() && call.tool_output.is_some()
                        })
                        .cloned()
                        .collect();
                }
            }
        }
        crate::AtomicChatRequest::Retry { user_message_id } => {
            let user = conversation
                .messages
                .iter()
                .find(|row| row.message.id == *user_message_id && row.message.role == "user")
                .ok_or("Saved user turn not found")?;
            // Do not bind: binding can reconcile/checkpoint interrupted work.
            tools.tool_calls_record = user.tool_calls.clone();
            tools.require_known_outcomes()?;
        }
        crate::AtomicChatRequest::Recap { .. } => {
            tools.tools.clear();
            recap = Some((
                saved_input
                    .as_ref()
                    .map(|message| message.id.as_str())
                    .unwrap_or(""),
                admission_content,
            ));
        }
    }
    let messages = api_request_messages(&conversation, &tools, &description, recap, content)?;
    let messages = tools.with_response_guidance(messages);
    let schemas = serde_json::to_string(tools.tools()).map_err(|e| e.to_string())?;
    // Framing overhead is conservative, not provider-reported token usage.
    Ok(messages
        .iter()
        .map(|message| estimate_message_tokens(message) + 8)
        .sum::<usize>()
        + count_tokens(&schemas)
        + tools.tools().len() * 16)
}

/// Input for MCP's existing Atomic Claude request. Resume never resends Presence.
pub struct ClaudeChatInput {
    pub system_prompt: Option<String>,
    pub prompt: String,
}

/// Build native input without admission, tool binding or checkpoint writes.
pub(crate) async fn project_claude_chat_input(
    storage: StorageBackend,
    conversation_id: &str,
    request: &crate::AtomicChatRequest,
    settings: HashMap<String, String>,
    scope: MemuToolConfig,
    new_sitting: bool,
    continuation: Option<&str>,
) -> Result<(ClaudeChatInput, usize), String> {
    let conversation = storage
        .get_conversation_sync(conversation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Conversation not found")?;
    if conversation.conversation.user_id.as_deref() != Some(scope.user_id.as_str())
        || conversation.conversation.soul_id.as_deref() != Some(scope.soul_id.as_str())
    {
        return Err("Chat owner/Soul does not match".into());
    }
    let (content, page, canvas) = match request {
        crate::AtomicChatRequest::New {
            content,
            page_context,
            canvas_context,
        } => (
            content.as_str(),
            page_context.clone(),
            canvas_context.clone(),
        ),
        crate::AtomicChatRequest::Retry { user_message_id } => {
            let saved = conversation
                .conversation
                .atomic_chat_state
                .as_ref()
                .and_then(|state| state.active.as_ref())
                .filter(|turn| turn.user_message_id == *user_message_id)
                .ok_or("Retry requires its matching saved turn")?;
            let user = conversation
                .messages
                .iter()
                .find(|row| row.message.id == *user_message_id && row.message.role == "user")
                .ok_or("Saved user turn not found")?;
            (
                continuation.unwrap_or(&user.message.content),
                saved.page_context.clone(),
                saved.canvas_context.clone(),
            )
        }
        crate::AtomicChatRequest::Recap { instruction } => {
            if new_sitting {
                return Err("Recap must resume the existing native sitting".into());
            }
            return Ok((
                ClaudeChatInput {
                    system_prompt: None,
                    prompt: instruction.clone(),
                },
                count_tokens(instruction) + 8,
            ));
        }
    };
    let tags = storage
        .get_scope_tag_ids_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let session = ChatToolSession::new(
        storage,
        conversation_id.into(),
        tags,
        settings,
        Some(scope),
        page,
        canvas,
        None,
        Arc::new(|_| {}),
    );
    let input = session
        .claude_input_for(&conversation, new_sitting, content)
        .await?;
    let schemas = serde_json::to_string(session.tools()).map_err(|e| e.to_string())?;
    let tokens = count_tokens(&input.prompt)
        + input
            .system_prompt
            .as_deref()
            .map(count_tokens)
            .unwrap_or(0)
        + count_tokens(&schemas)
        + session.tools().len() * 16
        + 16;
    Ok((input, tokens))
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
    restored_call_count: usize,
    sitting_wide_citations: bool,
}

impl ChatToolSession {
    fn with_response_guidance(&self, messages: Vec<Message>) -> Vec<Message> {
        if self.memu_tool_config.is_some() {
            if let Some(limit) = memu_response_sentence_limit(&self.external_settings) {
                return append_memu_response_limit(messages, limit);
            }
        }
        messages
    }
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
            restored_call_count: 0,
            sitting_wide_citations: false,
        }
    }

    async fn claude_input_for(
        &self,
        conversation: &crate::ConversationWithMessages,
        new_sitting: bool,
        content: &str,
    ) -> Result<ClaudeChatInput, String> {
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
        let mut prompt =
            format!("<atomic_turn_context>\n{context}\n</atomic_turn_context>\n\n{content}");
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
        high_water: Option<i32>,
        prior: Vec<ChatCitation>,
        mut calls: Vec<ChatToolCall>,
        citations: Vec<ChatCitation>,
        recovering: bool,
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
        self.sitting_wide_citations = high_water.is_some();
        self.citation_index = high_water
            .unwrap_or(0)
            .max(catalog.keys().next_back().copied().unwrap_or(0));
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
        self.restored_call_count = calls.len();
        self.tool_calls_record = calls;
        self.citations = citations;
        if interrupted {
            self.checkpoint().await?;
        }
        if recovering { Ok(()) } else { self.require_known_outcomes() }
    }

    fn require_known_outcomes(&self) -> Result<(), String> {
        if self.tool_calls_record.iter().any(ChatToolCall::outcome_unknown) {
            return Err("A previous tool's outcome is unknown. Do not resume or repeat it. Close out from saved history or start a new sitting.".to_string());
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
                    if self.sitting_wide_citations {
                        self.citation_index
                    } else {
                        0
                    },
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

    pub fn binding(&self) -> Option<(&str, &str, &str, &str)> {
        let scope = self.memu_tool_config.as_ref()?;
        Some((&scope.user_id, &scope.soul_id, &self.conversation_id,
            self.durable_message_id.as_deref()?))
    }

    pub async fn execute(
        &mut self,
        call_id: &str,
        tool_name: &str,
        tool_args: serde_json::Value,
    ) -> Result<ChatToolResult, String> {
        self.require_known_outcomes()?;
        let existing = self
            .tool_calls_record
            .iter()
            .position(|record| record.id == call_id);
        if let Some(index) = existing {
            let record = &self.tool_calls_record[index];
            if record.tool_name != tool_name || record.tool_input != tool_args {
                return Err("This tool call ID already identifies a different request".to_string());
            }
        }
        if tool_name == "edit_atom" {
            if let Some(index) = self.tool_calls_record[..self.restored_call_count]
                .iter()
                .position(|record| {
                    record.status == "complete"
                        && record.tool_name == tool_name
                        && record.tool_input == tool_args
                })
            {
                let content = saved_tool_output(&self.tool_calls_record[index])?;
                return Ok(ChatToolResult {
                    content,
                    is_error: false,
                });
            }
        }
        let record_index = if let Some(index) = existing {
            let record = &mut self.tool_calls_record[index];
            let not_executed = record
                .tool_output
                .as_ref()
                .and_then(|output| output.get("executed"))
                .and_then(serde_json::Value::as_bool)
                == Some(false);
            if !not_executed {
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
        let mut outcome_unknown = false;
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
                    Err(EditAtomError::Failed(error)) => (format!("Error: {error}"), 0, true),
                    Err(EditAtomError::OutcomeUnknown(error)) => {
                        outcome_unknown = true;
                        (
                            format!("Edit outcome unknown: {error}. Do not repeat this edit."),
                            0,
                            true,
                        )
                    }
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
        record.tool_output = Some(if outcome_unknown {
            json!({"error": tool_result, "outcome_unknown": true})
        } else {
            serde_json::Value::String(tool_result.clone())
        });
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
        self.require_known_outcomes()?;

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
    recap: bool,
}

async fn run_agent_loop(
    provider_config: ProviderConfig,
    model: String,
    mut ctx: AgentContext,
) -> Result<(String, ChatToolRecords), String> {
    let provider = create_streaming_llm_provider(&provider_config)
        .map_err(|e| format!("Failed to create streaming provider: {}", e))?;
    let max_iterations = 10;

    for _iteration in 0..max_iterations {
        let config = LlmConfig::new(&model).with_params(
            GenerationParams::new()
                .with_temperature(0.7)
                .with_max_tokens(CHAT_RESPONSE_MAX_TOKENS),
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
        let mut call_messages = if ctx.tools.memu_tool_config.is_some() {
            ctx.messages.clone()
        } else {
            truncate_messages_to_context(
                ctx.messages.clone(),
                provider_config.context_length_for_model(&model),
            )
        };
        call_messages = ctx.tools.with_response_guidance(call_messages);
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
        if let Some(tool_calls) = response
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            if ctx.recap {
                return Err("Recap returned tool calls despite having no tools".to_string());
            }
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
            return Ok((response.content, ctx.tools.finish()));
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
        ApiChatInput::New(content),
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
    input: ApiChatInput<'_>,
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

    let mut conversation = storage
        .get_conversation_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Conversation not found")?;
    if let Some(scope) = &memu_tool_config {
        if conversation.conversation.user_id.as_deref() != Some(scope.user_id.as_str())
            || conversation.conversation.soul_id.as_deref() != Some(scope.soul_id.as_str())
        {
            return Err("Chat owner/Soul does not match".to_string());
        }
    }
    let recap_instruction = match &input {
        ApiChatInput::Recap { instruction, .. } => Some(*instruction),
        _ => None,
    };
    let (user_message_id, expected_user_message_id, page_context, canvas_context) = match input {
        ApiChatInput::New(content) => {
            if memu_tool_config.is_some() || conversation.conversation.atomic_chat_state.is_some() {
                return Err(
                    "This sitting requires saved-turn admission before API execution".to_string(),
                );
            }
            let message = storage
                .save_message_sync(conversation_id, "user", content)
                .await
                .map_err(|e| e.to_string())?;
            let id = message.id.clone();
            conversation.messages.push(ChatMessageWithContext {
                message,
                tool_calls: vec![],
                citations: vec![],
            });
            (id, None, page_context, canvas_context)
        }
        ApiChatInput::Saved(id) => {
            if memu_tool_config.is_none() {
                return Err("Saved API turns require an integrated owner/Soul scope".to_string());
            }
            let state = conversation
                .conversation
                .atomic_chat_state
                .as_ref()
                .ok_or("Saved API execution requires an admitted turn")?;
            if state.backend != crate::AtomicChatBackend::Api {
                return Err("This sitting requires claude_code=true. Restore its original backend or start a new sitting.".to_string());
            }
            let active = state
                .active
                .as_ref()
                .filter(|turn| {
                    turn.user_message_id == id
                        && turn.status == crate::AtomicChatTurnStatus::Running
                })
                .ok_or("Saved API execution requires the matching running ordinary turn")?;
            (
                id.to_string(),
                Some(id.to_string()),
                active.page_context.clone(),
                active.canvas_context.clone(),
            )
        }
        ApiChatInput::Recap { id, .. } => {
            let state = conversation
                .conversation
                .atomic_chat_state
                .as_ref()
                .ok_or("Recap requires managed admission")?;
            if memu_tool_config.is_none()
                || state.backend != crate::AtomicChatBackend::Api
                || !state.recap.as_ref().is_some_and(|recap| {
                    recap.request_message_id == id
                        && matches!(recap.outcome, crate::AtomicChatRecapOutcome::Running)
                })
            {
                return Err("Recap requires the matching scoped running API control".to_string());
            }
            (id.to_string(), Some(id.to_string()), None, None)
        }
    };

    // Get conversation context
    let scope_tag_ids = storage
        .get_scope_tag_ids_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let scope_description = storage
        .get_scope_description_sync(&scope_tag_ids)
        .await
        .map_err(|e| e.to_string())?;

    let mut tools = ChatToolSession::new(
        storage.clone(),
        conversation_id.to_string(),
        scope_tag_ids,
        settings_map,
        memu_tool_config,
        page_context,
        canvas_context,
        canvas_cache,
        Arc::clone(&on_event),
    );
    if recap_instruction.is_some() {
        tools.tools.clear();
    } else if tools.memu_tool_config.is_some() {
        let turn = conversation
            .messages
            .iter()
            .find(|row| row.message.id == user_message_id)
            .ok_or("Saved user turn not found")?;
        tools
            .bind_user_turn(
                &user_message_id,
                None,
                vec![],
                turn.tool_calls.clone(),
                turn.citations.clone(), false,
            )
            .await?;
    }
    let api_messages = api_request_messages(
        &conversation,
        &tools,
        &scope_description,
        recap_instruction.map(|instruction| (user_message_id.as_str(), instruction)),
        None,
    )?;

    // Truncate to fit context window for providers with limited context
    let api_messages = if tools.memu_tool_config.is_some() {
        api_messages
    } else {
        truncate_messages_to_context(
            api_messages,
            provider_config.context_length_for_model(&model),
        )
    };

    // Create agent context
    let ctx = AgentContext {
        messages: api_messages,
        tools,
        recap: recap_instruction.is_some(),
    };

    // Run agent loop with canvas context
    let (content, mut records) = run_agent_loop(provider_config, model, ctx).await?;
    if recap_instruction.is_some() && content.trim().is_empty() {
        return Err("The recap was empty. Retry close-out to generate it again.".to_string());
    }

    // Publish text, records and citation numbering together before emitting completion.
    let saved_msg = storage
        .save_chat_reply_sync(
            conversation_id,
            expected_user_message_id.as_deref(),
            &content,
            &records.tool_calls,
            &records.citations,
            0, // API citations are per reply, not a native sitting high-water mark.
            None,
        )
        .await
        .map_err(|e| e.to_string())?;

    for tool_call in &mut records.tool_calls {
        tool_call.message_id = saved_msg.id.clone();
    }
    for citation in &mut records.citations {
        citation.message_id = saved_msg.id.clone();
    }
    let result = ChatMessageWithContext {
        message: saved_msg,
        tool_calls: records.tool_calls,
        citations: records.citations,
    };
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

    fn new_chat_request(content: &str) -> crate::AtomicChatRequest {
        crate::AtomicChatRequest::New {
            content: content.to_string(),
            page_context: None,
            canvas_context: None,
        }
    }

    fn api_test_settings(url: &str) -> HashMap<String, String> {
        [
            ("provider", "openai_compat"),
            ("openai_compat_base_url", url),
            ("openai_compat_api_key", "fictional-key"),
            ("openai_compat_llm_model", "fictional-model"),
            ("openai_compat_context_length", "200000"),
            ("memu_log_prompts", "false"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    fn api_test_response(delta: serde_json::Value) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":delta,"finish_reason":null}]})
            ))
    }

    #[tokio::test]
    async fn request_projection_is_read_only_and_matches_captured_retry_context() {
        use crate::{
            AtomicChatBackend as Backend, AtomicChatPhase as Phase, AtomicChatRequest as Request,
        };
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("projection.db")).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conv.conversation.id;
        core.save_message(cid, "system", "Fictional original Presence")
            .await
            .unwrap();
        let older_user = core
            .save_message(cid, "user", "Older fictional question")
            .await
            .unwrap();
        let older_reply = core
            .save_message(cid, "assistant", "Older fictional answer")
            .await
            .unwrap();
        let scope = MemuToolConfig {
            base_url: "http://127.0.0.1:9".into(),
            user_id: "TestOwner".into(),
            soul_id: "TestSoul".into(),
        };
        let settings = api_test_settings("http://127.0.0.1:9");
        let before = serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap();
        let plain = core
            .estimate_chat_request_tokens(
                cid,
                &new_chat_request("Fictional input"),
                settings.clone(),
                scope.clone(),
            )
            .await
            .unwrap();
        let request = Request::New {
            content: "Fictional input".into(),
            page_context: Some(
                serde_json::from_value(json!({"view":"atom_reader", "atom_id":"memory:fictional"}))
                    .unwrap(),
            ),
            canvas_context: None,
        };
        let captured = core
            .estimate_chat_request_tokens(cid, &request, settings.clone(), scope.clone())
            .await
            .unwrap();
        assert!(captured > plain);
        let mut guided = settings.clone();
        guided.insert(MEMU_RESPONSE_SENTENCES_SETTING.into(), "7".into());
        assert!(
            core.estimate_chat_request_tokens(cid, &request, guided, scope.clone())
                .await
                .unwrap()
                > captured
        );
        let large = new_chat_request(&"Fictional evidence ".repeat(20_000));
        assert!(
            core.estimate_chat_request_tokens(cid, &large, settings.clone(), scope.clone())
                .await
                .unwrap()
                > plain + 10_000
        );
        assert_eq!(
            serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap(),
            before
        );
        let wrong_scope = MemuToolConfig {
            soul_id: "OtherSoul".into(),
            ..scope.clone()
        };
        assert!(
            core.estimate_chat_request_tokens(cid, &request, settings.clone(), wrong_scope)
                .await
                .is_err()
        );
        let user = core
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &request,
                200_000,
            )
            .await
            .unwrap();
        core.fail_chat_turn(cid, &user.id, "Fictional failure before model use", false, None)
            .await
            .unwrap();
        let before = serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap();
        let retry = Request::Retry {
            user_message_id: user.id.clone(),
        };
        assert_eq!(
            core.estimate_chat_request_tokens(cid, &retry, settings.clone(), scope.clone())
                .await
                .unwrap(),
            captured
        );
        assert_eq!(
            serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap(),
            before
        );
        assert!(
            core.estimate_chat_request_tokens(
                cid,
                &Request::Retry {
                    user_message_id: older_user.id
                },
                settings.clone(),
                scope.clone()
            )
            .await
            .is_err()
        );
        let mut records = ChatToolRecords {
            tool_calls: vec![ChatToolCall {
                id: "older-read".into(),
                message_id: older_reply.id.clone(),
                tool_name: "get_atom".into(),
                tool_input: json!({"atom_id":"memory:older"}),
                tool_output: Some(json!("Omitted older tool output ".repeat(5_000))),
                status: "complete".into(),
                created_at: Utc::now().to_rfc3339(),
                completed_at: None,
            }],
            citations: vec![],
            prior_citations: vec![],
            citation_index: 0,
        };
        core.save_chat_tool_records(cid, &older_reply.id, &records)
            .await
            .unwrap();
        assert_eq!(
            core.estimate_chat_request_tokens(cid, &retry, settings.clone(), scope.clone())
                .await
                .unwrap(),
            captured
        );
        let call = &mut records.tool_calls[0];
        call.id = "current-known-failure".into();
        call.message_id = user.id.clone();
        call.status = "failed".into();
        call.tool_output = Some(json!("Error: fictional known failure ".repeat(100)));
        core.save_chat_tool_records(cid, &user.id, &records)
            .await
            .unwrap();
        let before = serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap();
        assert!(
            core.estimate_chat_request_tokens(cid, &retry, settings.clone(), scope.clone())
                .await
                .unwrap()
                > captured
        );
        assert_eq!(
            serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap(),
            before
        );
        core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closing)
            .await
            .unwrap();
        core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closed)
            .await
            .unwrap();
        let standing = new_chat_request("");
        let closed_estimate = core
            .estimate_chat_request_tokens(cid, &standing, settings.clone(), scope.clone())
            .await
            .unwrap();
        records.tool_calls[0].tool_output =
            Some(json!("Historical failed tool output ".repeat(5_000)));
        core.save_chat_tool_records(cid, &user.id, &records)
            .await
            .unwrap();
        assert_eq!(
            core.estimate_chat_request_tokens(cid, &standing, settings, scope)
                .await
                .unwrap(),
            closed_estimate
        );
    }

    #[tokio::test]
    async fn projection_does_not_checkpoint_interrupted_tools_and_sizes_recap_digest() {
        use crate::AtomicChatRequest as Request;
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("projection-tools.db")).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conv.conversation.id;
        let user = core
            .save_message(cid, "user", "Fictional interrupted request")
            .await
            .unwrap();
        let scope = MemuToolConfig {
            base_url: "http://127.0.0.1:9".into(),
            user_id: "TestOwner".into(),
            soul_id: "TestSoul".into(),
        };
        let settings = api_test_settings("http://127.0.0.1:9");
        let recap = Request::Recap {
            instruction: "Fictional recap instruction".into(),
        };
        let without_tools = core
            .estimate_chat_request_tokens(cid, &recap, settings.clone(), scope.clone())
            .await
            .unwrap();
        let standing = new_chat_request("");
        let standing_before = core
            .estimate_chat_request_tokens(cid, &standing, settings.clone(), scope.clone())
            .await
            .unwrap();
        core.save_chat_tool_records(
            cid,
            &user.id,
            &ChatToolRecords {
                tool_calls: vec![
                    ChatToolCall {
                        id: "unfinished-read".into(),
                        message_id: user.id.clone(),
                        tool_name: "get_atom".into(),
                        tool_input: json!({"atom_id":"memory:fictional"}),
                        tool_output: None,
                        status: "running".into(),
                        created_at: Utc::now().to_rfc3339(),
                        completed_at: None,
                    },
                    ChatToolCall {
                        id: "known-read".into(),
                        message_id: user.id.clone(),
                        tool_name: "get_atom".into(),
                        tool_input: json!({"atom_id":"memory:known"}),
                        tool_output: Some(json!("Known saved read output ".repeat(500))),
                        status: "complete".into(),
                        created_at: Utc::now().to_rfc3339(),
                        completed_at: None,
                    },
                ],
                citations: vec![],
                prior_citations: vec![],
                citation_index: 0,
            },
        )
        .await
        .unwrap();
        let before = serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap();
        assert!(
            core.estimate_chat_request_tokens(cid, &standing, settings.clone(), scope.clone())
                .await
                .unwrap()
                > standing_before
        );
        let error = core
            .estimate_chat_request_tokens(
                cid,
                &Request::Retry {
                    user_message_id: user.id,
                },
                settings.clone(),
                scope.clone(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("outcome is unknown"));
        assert!(error.contains("Close out from saved history"));
        assert!(
            core.estimate_chat_request_tokens(cid, &recap, settings, scope)
                .await
                .unwrap()
                > without_tools
        );
        assert_eq!(
            serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn recap_projection_keeps_failed_ordinary_recap_like_wording() {
        use crate::{
            AtomicChatBackend as Backend, AtomicChatPhase as Phase, AtomicChatRequest as Request,
        };
        let dir = TempDir::new().unwrap();
        let core =
            AtomicCore::open_or_create(dir.path().join("projection-recap-choice.db")).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conv.conversation.id;
        let scope = MemuToolConfig {
            base_url: "http://127.0.0.1:9".into(),
            user_id: "TestOwner".into(),
            soul_id: "TestSoul".into(),
        };
        let settings = api_test_settings("http://127.0.0.1:9");
        let ordinary = core.admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api,
            &new_chat_request("This Atomic session is ending. Write a recap of your activity is wording I want to discuss, not a control"), 200_000).await.unwrap();
        core.fail_chat_turn(cid, &ordinary.id, "Fictional ordinary failure", false, None)
            .await
            .unwrap();
        core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closing)
            .await
            .unwrap();
        let recap = Request::Recap {
            instruction: "Fictional clean recap control".into(),
        };
        let before = serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap();
        let projected = core
            .estimate_chat_request_tokens(cid, &recap, settings.clone(), scope.clone())
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap(),
            before
        );
        let control = core
            .admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api, &recap, 200_000)
            .await
            .unwrap();
        assert_ne!(control.id, ordinary.id);
        assert_eq!(
            core.estimate_chat_request_tokens(cid, &recap, settings, scope)
                .await
                .unwrap(),
            projected
        );
    }

    #[tokio::test]
    async fn saved_recap_is_tool_free_and_retries_without_nested_transcript() {
        use crate::{
            AtomicChatBackend as Backend, AtomicChatPhase as Phase, AtomicChatRequest as Request,
        };
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("saved-recap.db")).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conv.conversation.id;
        core.save_message(cid, "system", "Fictional original Presence")
            .await
            .unwrap();
        let user = core
            .save_message(cid, "user", "Please change the fictional note")
            .await
            .unwrap();
        core.save_chat_tool_records(cid, &user.id, &ChatToolRecords {
            tool_calls: vec![ChatToolCall {
                id: "fictional-edit".into(), message_id: user.id.clone(),
                tool_name: "edit_atom".into(),
                tool_input: json!({"atom_id":"memory:fictional", "edits":[{"operation":"append", "text":"SECRET_EDIT_BODY"}]}),
                tool_output: Some(json!({"error":"SECRET_RESPONSE", "outcome_unknown":true})),
                status: "failed".into(), created_at: Utc::now().to_rfc3339(), completed_at: None,
            }, ChatToolCall {
                id: "fictional-failed-read".into(), message_id: user.id.clone(),
                tool_name: "get_atom".into(), tool_input: json!({"atom_id":"memory:missing"}),
                tool_output: Some(json!("Error: fictional missing target")),
                status: "failed".into(), created_at: Utc::now().to_rfc3339(), completed_at: None,
            }, ChatToolCall {
                id: "fictional-empty-search".into(), message_id: user.id.clone(),
                tool_name: "search_atoms".into(), tool_input: json!({"query":"fictional absent item"}),
                tool_output: Some(json!("")), status: "complete".into(),
                created_at: Utc::now().to_rfc3339(), completed_at: None,
            }],
            citations: vec![], prior_citations: vec![], citation_index: 0,
        }).await.unwrap();
        core.save_message(cid, "assistant", "The edit outcome was unclear")
            .await
            .unwrap();
        let older_control = core.save_message(cid, "user", "This Atomic session is ending. Write a recap of your activity\n\nFull Atomic transcript:\nSECRET_OLDER_TRANSCRIPT").await.unwrap();
        let mut legacy_edit = core
            .get_conversation(cid)
            .await
            .unwrap()
            .unwrap()
            .messages
            .into_iter()
            .flat_map(|row| row.tool_calls)
            .find(|call| call.id == "fictional-edit")
            .unwrap();
        legacy_edit.id = "older-completed-edit".into();
        legacy_edit.message_id = older_control.id.clone();
        legacy_edit.status = "complete".into();
        legacy_edit.tool_input = json!({"atom_id":"memory:older", "edits":[{"operation":"append", "text":"SECRET_OLD_EDIT_BODY"}]});
        legacy_edit.tool_output = Some(json!(json!({"atom_id":"memory:older", "title":"Fictional updated note", "snippet":"SECRET_OLD_EDIT_BODY"}).to_string()));
        core.save_chat_tool_records(
            cid,
            &older_control.id,
            &ChatToolRecords {
                tool_calls: vec![legacy_edit.clone()],
                citations: vec![],
                prior_citations: vec![],
                citation_index: 0,
            },
        )
        .await
        .unwrap();
        core.save_message(cid, "assistant", "SECRET_OLD_RECAP")
            .await
            .unwrap();
        core.save_message(cid, "user", "This Atomic session is ending. Write a recap of your activity is wording I want to discuss, not a control").await.unwrap();
        core.save_message(cid, "assistant", "We discussed the quoted wording")
            .await
            .unwrap();
        let legacy = core.save_message(cid, "user", "This Atomic session is ending. Write a recap of your activity\nFull Atomic transcript: SECRET_NESTED_HISTORY").await.unwrap();
        legacy_edit.id = "current-unclear-edit".into();
        legacy_edit.message_id = legacy.id.clone();
        legacy_edit.status = "running".into();
        legacy_edit.tool_input = json!({"atom_id":"memory:current-control"});
        legacy_edit.tool_output = None;
        core.save_chat_tool_records(
            cid,
            &legacy.id,
            &ChatToolRecords {
                tool_calls: vec![legacy_edit],
                citations: vec![],
                prior_citations: vec![],
                citation_index: 0,
            },
        )
        .await
        .unwrap();
        core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closing)
            .await
            .unwrap();
        let request = Request::Recap {
            instruction: "Clean server recap instruction".into(),
        };
        let control = core
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &request,
                200_000,
            )
            .await
            .unwrap();
        assert_eq!(control.id, legacy.id);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let scope = MemuToolConfig {
            base_url: server.uri(),
            user_id: "TestOwner".into(),
            soul_id: "TestSoul".into(),
        };
        let settings = api_test_settings(&server.uri());
        assert!(
            core.execute_saved_chat_recap(
                cid,
                &control.id,
                "Clean server recap instruction",
                |_| {},
                settings.clone(),
                scope.clone()
            )
            .await
            .is_err()
        );
        core.fail_chat_turn(cid, &control.id, "Fictional provider failure", false, None)
            .await
            .unwrap();
        let retried = core
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &request,
                200_000,
            )
            .await
            .unwrap();
        assert_eq!(retried.id, control.id);
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(api_test_response(
                json!({"content":"Saved fictional recap"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let result = core
            .execute_saved_chat_recap(
                cid,
                &control.id,
                "Clean server recap instruction",
                |_| {},
                settings.clone(),
                scope.clone(),
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(|tools| tools.is_empty()))
        );
        let serialized = body.to_string();
        for absent in [
            "SECRET_EDIT_BODY",
            "SECRET_RESPONSE",
            "SECRET_NESTED_HISTORY",
            "SECRET_OLDER_TRANSCRIPT",
            "SECRET_OLD_EDIT_BODY",
            "SECRET_OLD_RECAP",
            "tool_call_id",
            "Use search_atoms",
        ] {
            assert!(!serialized.contains(absent), "unexpected {absent}");
        }
        for once in [
            "Fictional original Presence",
            "Please change the fictional note",
            "Clean server recap instruction",
            "wording I want to discuss, not a control",
            "We discussed the quoted wording",
            "Error: fictional missing target",
            "no results",
            "Fictional updated note",
        ] {
            assert_eq!(serialized.matches(once).count(), 1);
        }
        let messages = body["messages"].as_array().unwrap();
        let dialogue: Vec<_> = messages[1..messages.len() - 1]
            .iter()
            .map(|row| row["content"].as_str().unwrap())
            .collect();
        assert_eq!(
            dialogue,
            vec![
                "Fictional original Presence",
                "Please change the fictional note",
                "The edit outcome was unclear",
                "This Atomic session is ending. Write a recap of your activity is wording I want to discuss, not a control",
                "We discussed the quoted wording",
            ]
        );
        let (_, digest) = messages.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .rsplit_once('\n')
            .unwrap();
        let actions: serde_json::Value = serde_json::from_str(digest).unwrap();
        let current = actions
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["input"]["atom_id"] == "memory:current-control")
            .unwrap();
        assert_eq!(current["outcome"], "unknown");
        let older = actions
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["input"]["atom_id"] == "memory:older")
            .unwrap();
        assert_eq!(older["status"], "complete");
        assert_eq!(older["outcome"]["title"], "Fictional updated note");
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(saved.messages.last().unwrap().message.id, result.message.id);
        assert!(
            core.admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &request,
                200_000
            )
            .await
            .is_err()
        );
        assert!(
            core.execute_saved_chat_recap(
                cid,
                &control.id,
                "Clean server recap instruction",
                |_| {},
                settings,
                scope
            )
            .await
            .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn saved_api_retry_restores_results_without_editing_twice_or_reusing_old_citations() {
        use crate::{AtomicChatBackend as Backend, AtomicChatRequest as Request};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{body_json, method, path},
        };
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("api-retry.db")).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conv.conversation.id;
        core.save_message(cid, "system", "Fictional Soul Presence")
            .await
            .unwrap();
        core.save_message(cid, "user", "An earlier question")
            .await
            .unwrap();
        let old = core
            .save_message(cid, "assistant", "Earlier source [1]")
            .await
            .unwrap();
        core.save_chat_tool_records(
            cid,
            &old.id,
            &ChatToolRecords {
                tool_calls: vec![],
                citation_index: 40,
                prior_citations: vec![],
                citations: vec![ChatCitation {
                    id: Uuid::new_v4().to_string(),
                    message_id: old.id.clone(),
                    citation_index: 1,
                    atom_id: "memory:older".into(),
                    chunk_index: None,
                    excerpt: "Older evidence".into(),
                    relevance_score: None,
                }],
            },
        )
        .await
        .unwrap();
        let server = MockServer::start().await;
        let scope = MemuToolConfig {
            base_url: server.uri(),
            user_id: "TestOwner".into(),
            soul_id: "TestSoul".into(),
        };
        let args = json!({"atom_id":"memory:fictional", "edits":[{"operation":"append","text":"\naddition"}]});
        let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = Arc::clone(&requests);
        Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(move |request: &wiremock::Request| {
            let number = { let mut rows = captured.lock().unwrap(); rows.push(serde_json::from_slice(&request.body).unwrap()); rows.len() - 1 };
            match number {
                0 | 2 | 4 => api_test_response(json!({"tool_calls":[{"index":0, "id":format!("edit-{number}"), "type":"function", "function":{"name":"edit_atom", "arguments":args.to_string()}}]})),
                1 => ResponseTemplate::new(500).set_body_string("Fictional failure after completed edit"),
                _ => api_test_response(json!({"content":"Edited [1]."})),
            }
        }).expect(6).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/memory/memory:fictional"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"memory:fictional","summary":"Base text"})),
            )
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/memory/memory:fictional"))
            .and(body_json(
                json!({"summary":"Base text\naddition","displayed_summary":"Base text","edited_by":"atomic:agent"}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("Unusable optional response body"),
            )
            .expect(2)
            .mount(&server)
            .await;
        let settings = api_test_settings(&server.uri());
        let mut request = new_chat_request("Append to the fictional memory");
        if let Request::New { page_context, .. } = &mut request {
            *page_context = Some(
                serde_json::from_value(json!({"view":"reader", "atom_id":"memory:fictional"}))
                    .unwrap(),
            );
        }
        let user = core
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &request,
                200_000,
            )
            .await
            .unwrap();
        assert!(
            core.execute_saved_chat_turn(cid, &user.id, |_| {}, settings.clone(), scope.clone())
                .await
                .is_err()
        );
        let failed = core.get_conversation(cid).await.unwrap().unwrap();
        let turn = failed
            .messages
            .iter()
            .find(|row| row.message.id == user.id)
            .unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].status, "complete");
        let output = saved_tool_output(&turn.tool_calls[0]).unwrap();
        core.fail_chat_turn(cid, &user.id, "Fictional model failure", false, None)
            .await
            .unwrap();
        core.admit_chat_turn(
            cid,
            "TestOwner",
            "TestSoul",
            &Backend::Api,
            &Request::Retry {
                user_message_id: user.id.clone(),
            },
            200_000,
        )
        .await
        .unwrap();
        let result = core
            .execute_saved_chat_turn(cid, &user.id, |_| {}, settings.clone(), scope.clone())
            .await
            .unwrap();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.citations.len(), 1);
        assert_eq!(result.citations[0].citation_index, 1);
        assert_eq!(result.citations[0].atom_id, "memory:fictional");
        assert_eq!(
            core.storage
                .get_chat_citation_index_sync(cid)
                .await
                .unwrap(),
            40
        );
        let again = core
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &new_chat_request("Append again intentionally"),
                200_000,
            )
            .await
            .unwrap();
        let result = core
            .execute_saved_chat_turn(cid, &again.id, |_| {}, settings, scope)
            .await
            .unwrap();
        assert_eq!(result.citations[0].citation_index, 1);
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        assert!(
            saved
                .conversation
                .atomic_chat_state
                .unwrap()
                .active
                .is_none()
        );
        assert_eq!(
            saved
                .messages
                .iter()
                .filter(|row| row.message.content == "Append to the fictional memory")
                .count(),
            1
        );
        let requests = requests.lock().unwrap();
        let replay = requests[2]["messages"].as_array().unwrap();
        assert_eq!(
            replay
                .iter()
                .filter(|message| message["role"] == "user"
                    && message["content"] == "Append to the fictional memory")
                .count(),
            1
        );
        assert!(
            requests[2]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == "get_current_page_context")
        );
        assert_eq!(replay.last().unwrap()["content"], output);
        for pair in replay
            .windows(2)
            .filter(|pair| pair[0]["tool_calls"].is_array())
        {
            assert_eq!(pair[1]["role"], "tool");
            assert_eq!(pair[1]["tool_call_id"], pair[0]["tool_calls"][0]["id"]);
        }
    }

    #[tokio::test]
    async fn unknown_memu_write_is_checkpointed_and_cannot_repeat() {
        use crate::{AtomicChatBackend as Backend, AtomicChatRequest as Request};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{body_json, method, path},
        };
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("unknown-write.db")).unwrap();
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
                &Backend::Api,
                &new_chat_request("Append to a fictional dossier"),
                200_000,
            )
            .await
            .unwrap();
        let server = MockServer::start().await;
        let summary = Arc::new(Mutex::new("Base text".to_string()));
        let read_summary = Arc::clone(&summary);
        Mock::given(method("GET")).and(path("/memory/category:fictional"))
            .respond_with(move |_: &wiremock::Request| ResponseTemplate::new(200).set_body_json(json!({
                "id":"category:fictional", "label":"Fictional dossier", "description":"A fictional description",
                "summary":read_summary.lock().unwrap().clone(), "summaries_revision":7
            }))).expect(1).mount(&server).await;
        let written_summary = Arc::clone(&summary);
        Mock::given(method("PATCH")).and(path("/category/category:fictional"))
            .and(body_json(json!({"summary":"Base text\naddition", "displayed_summary":"Base text",
                "displayed_title":"Fictional dossier", "displayed_description":"A fictional description",
                "summaries_revision":7, "edited_by":"atomic:agent"})))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                *written_summary.lock().unwrap() = body["summary"].as_str().unwrap().to_string();
                ResponseTemplate::new(500).set_body_string("Fictional error after commit")
            }).expect(1).mount(&server).await;
        let mut tools = ChatToolSession::new(
            core.storage.clone(),
            cid.to_string(),
            vec![],
            HashMap::new(),
            Some(MemuToolConfig {
                base_url: server.uri(),
                user_id: "TestOwner".into(),
                soul_id: "TestSoul".into(),
            }),
            None,
            None,
            None,
            Arc::new(|_| {}),
        );
        tools
            .bind_user_turn(&user.id, None, vec![], vec![], vec![], false)
            .await
            .unwrap();
        let args = json!({"atom_id":"category:fictional", "edits":[{"operation":"append", "text":"\naddition"}]});
        assert!(
            tools
                .execute("edit-first", "edit_atom", args.clone())
                .await
                .err()
                .unwrap()
                .contains("unknown")
        );
        assert!(
            tools
                .execute("edit-again", "edit_atom", args)
                .await
                .err()
                .unwrap()
                .contains("unknown")
        );
        assert_eq!(*summary.lock().unwrap(), "Base text\naddition");
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        let call = &saved.messages.last().unwrap().tool_calls[0];
        assert_eq!(call.status, "failed");
        assert_eq!(call.tool_output.as_ref().unwrap()["outcome_unknown"], true);
        let recovered = core.saved_chat_tool_records(cid, &user.id, MemuToolConfig {
            base_url: server.uri(), user_id: "TestOwner".into(), soul_id: "TestSoul".into(),
        }).await.unwrap();
        assert_eq!(recovered.tool_calls.len(), 1);
        assert_eq!(recovered.tool_calls[0].tool_output.as_ref().unwrap()["outcome_unknown"], true);
        assert_eq!(*summary.lock().unwrap(), "Base text\naddition");
        // Prove the saved tool flag blocks Retry even if the caller misses uncertainty.
        core.fail_chat_turn(cid, &user.id, "Unknown edit outcome", false, None)
            .await
            .unwrap();
        assert!(
            core.admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &Request::Retry {
                    user_message_id: user.id
                },
                200_000
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown")
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn saved_api_replay_preserves_known_failures_without_restoring_capabilities() {
        use crate::AtomicChatBackend as Backend;
        use wiremock::{
            Mock, MockServer,
            matchers::{method, path},
        };
        for provider in ["openai_compat", "ollama"] {
            let dir = TempDir::new().unwrap();
            let core = AtomicCore::open_or_create(dir.path().join("api-history.db")).unwrap();
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
                    &Backend::Api,
                    &new_chat_request("Continue from saved results"),
                    200_000,
                )
                .await
                .unwrap();
            let calls = vec![
                ChatToolCall {
                    id: "old-page".into(),
                    message_id: user.id.clone(),
                    tool_name: "get_current_page_context".into(),
                    tool_input: json!({}),
                    tool_output: Some(json!("Previous page\nverbatim")),
                    status: "complete".into(),
                    created_at: user.created_at.clone(),
                    completed_at: None,
                },
                ChatToolCall {
                    id: "failed-read".into(),
                    message_id: user.id.clone(),
                    tool_name: "get_atom".into(),
                    tool_input: json!({"atom_id":"memory:missing"}),
                    tool_output: Some(json!({"error":"Fictional missing memory"})),
                    status: "failed".into(),
                    created_at: user.created_at.clone(),
                    completed_at: None,
                },
            ];
            core.storage
                .save_chat_tool_state_sync(cid, &user.id, &calls, &[], 0)
                .await
                .unwrap();
            let server = MockServer::start().await;
            let response = if provider == "ollama" {
                wiremock::ResponseTemplate::new(200).set_body_string(
                format!("{}\n", json!({"message":{"role":"assistant","content":"Saved evidence considered."},"done":true}))
            )
            } else {
                api_test_response(json!({"content":"Saved evidence considered."}))
            };
            Mock::given(method("POST"))
                .and(path(if provider == "ollama" {
                    "/api/chat"
                } else {
                    "/v1/chat/completions"
                }))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let mut settings = api_test_settings(&server.uri());
            if provider == "ollama" {
                settings.extend([
                    ("provider".to_string(), "ollama".to_string()),
                    ("ollama_host".to_string(), server.uri()),
                    (
                        "ollama_llm_model".to_string(),
                        "fictional-model".to_string(),
                    ),
                    ("ollama_context_length".to_string(), "200000".to_string()),
                ]);
            }
            let scope = MemuToolConfig {
                base_url: server.uri(),
                user_id: "TestOwner".into(),
                soul_id: "TestSoul".into(),
            };
            let wrong = MemuToolConfig {
                soul_id: "OtherSoul".into(),
                ..scope.clone()
            };
            assert!(
                core.execute_saved_chat_turn(cid, &user.id, |_| {}, settings.clone(), wrong)
                    .await
                    .is_err()
            );
            assert!(
                core.execute_saved_chat_turn(
                    cid,
                    "other-turn",
                    |_| {},
                    settings.clone(),
                    scope.clone()
                )
                .await
                .is_err()
            );
            core.execute_saved_chat_turn(cid, &user.id, |_| {}, settings.clone(), scope.clone())
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let request: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert!(
                !request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == "get_current_page_context")
            );
            let results: Vec<_> = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "tool")
                .collect();
            assert_eq!(results[0]["content"], "Previous page\nverbatim");
            assert_eq!(
                results[1]["content"],
                "{\"error\":\"Fictional missing memory\"}"
            );
            let call = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["tool_calls"].is_array())
                .unwrap();
            assert_eq!(
                call["tool_calls"][0]["function"]["arguments"].is_object(),
                provider == "ollama"
            );
            let user = core
                .admit_chat_turn(
                    cid,
                    "TestOwner",
                    "TestSoul",
                    &Backend::Api,
                    &new_chat_request("Interrupted work"),
                    200_000,
                )
                .await
                .unwrap();
            let mut interrupted = calls[0].clone();
            interrupted.id = "interrupted".into();
            interrupted.message_id = user.id.clone();
            interrupted.status = "running".into();
            interrupted.tool_output = None;
            core.storage
                .save_chat_tool_state_sync(cid, &user.id, &[interrupted], &[], 0)
                .await
                .unwrap();
            assert!(
                core.execute_saved_chat_turn(cid, &user.id, |_| {}, settings, scope)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("unknown")
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            let saved = core.get_conversation(cid).await.unwrap().unwrap();
            assert_eq!(
                saved.messages.last().unwrap().tool_calls[0]
                    .tool_output
                    .as_ref()
                    .unwrap()["outcome_unknown"],
                true
            );
        }
    }

    #[tokio::test]
    async fn lifecycle_retry_reuses_saved_input_context_and_allowance() {
        use crate::{AtomicChatBackend as Backend, AtomicChatRequest as Request};
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("retry.db");
        let core = AtomicCore::open_or_create(&path).unwrap();
        let conv = core
            .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
            .await
            .unwrap();
        let cid = &conv.conversation.id;
        let request = Request::New {
            content: "Inspect this fictional page".into(),
            page_context: Some(
                serde_json::from_value(json!({"view":"reader", "atom_id":"fictional-page"}))
                    .unwrap(),
            ),
            canvas_context: Some(CanvasContext {
                clusters: vec![CanvasClusterSummary {
                    label: "Fictional cluster".into(),
                    atom_count: 2,
                }],
            }),
        };
        let user = core
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &request,
                200_000,
            )
            .await
            .unwrap();
        core.fail_chat_turn(cid, &user.id, "Fictional network failure", false, None)
            .await
            .unwrap();
        let before = core
            .get_conversation(cid)
            .await
            .unwrap()
            .unwrap()
            .conversation
            .atomic_chat_state
            .unwrap();
        let retry = Request::Retry {
            user_message_id: user.id.clone(),
        };
        for (owner, request, capacity) in [
            ("WrongOwner", retry.clone(), 128_000),
            (
                "TestOwner",
                Request::Retry {
                    user_message_id: "other-turn".into(),
                },
                128_000,
            ),
            ("TestOwner", retry.clone(), 20_000),
        ] {
            assert!(
                core.admit_chat_turn(cid, owner, "TestSoul", &Backend::Api, &request, capacity)
                    .await
                    .is_err()
            );
        }
        let unchanged = core.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(unchanged.messages.len(), 1);
        assert_eq!(unchanged.conversation.atomic_chat_state.unwrap(), before);
        let reused = core
            .admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api, &retry, 128_000)
            .await
            .unwrap();
        assert_eq!(reused.id, user.id);
        let reopened = AtomicCore::open_or_create(&path).unwrap();
        let saved = reopened.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(saved.messages.len(), 1);
        let state = saved.conversation.atomic_chat_state.unwrap();
        assert_eq!(state.context_window_tokens, Some(128_000));
        assert_eq!(
            state.active.as_ref().unwrap().page_context,
            before.active.as_ref().unwrap().page_context
        );
        assert_eq!(
            state.active.as_ref().unwrap().canvas_context,
            before.active.as_ref().unwrap().canvas_context
        );
        assert!(
            core.admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api, &retry, 128_000)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn lifecycle_recap_retries_and_publishes_without_erasing_failed_turn() {
        use crate::{
            AtomicChatBackend as Backend, AtomicChatPhase as Phase,
            AtomicChatRecapOutcome as Outcome, AtomicChatRequest as Request,
        };
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("recap.db")).unwrap();
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
                &Backend::Api,
                &new_chat_request("Fictional request"),
                200_000,
            )
            .await
            .unwrap();
        let recap = Request::Recap {
            instruction: "Fictional recap control".into(),
        };
        assert!(
            core.admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api, &recap, 200_000)
                .await
                .is_err()
        );
        core.fail_chat_turn(cid, &user.id, "Fictional unknown outcome", true, None)
            .await
            .unwrap();
        let ordinary = core
            .get_conversation(cid)
            .await
            .unwrap()
            .unwrap()
            .conversation
            .atomic_chat_state
            .unwrap()
            .active;
        core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closing)
            .await
            .unwrap();
        assert!(
            core.admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &Backend::Api,
                &Request::Retry {
                    user_message_id: user.id.clone()
                },
                200_000
            )
            .await
            .is_err()
        );
        let control = core
            .admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api, &recap, 200_000)
            .await
            .unwrap();
        assert!(
            core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closed)
                .await
                .is_err()
        );
        core.fail_chat_turn(cid, &control.id, "Fictional recap failure", true, None)
            .await
            .unwrap();
        assert!(
            core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closed)
                .await
                .is_err()
        );
        let retried = core
            .admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api, &recap, 200_000)
            .await
            .unwrap();
        assert_eq!(retried.id, control.id);
        let records = ChatToolRecords {
            tool_calls: vec![],
            citations: vec![],
            citation_index: 0,
            prior_citations: vec![],
        };
        assert!(
            core.save_chat_reply(cid, Some(&user.id), "Not the recap result", &records, None)
                .await
                .is_err()
        );
        core.database().unwrap().conn.lock().unwrap().execute_batch(
            "CREATE TRIGGER fail_recap BEFORE UPDATE OF atomic_chat_state ON conversations BEGIN SELECT RAISE(ABORT, 'fictional save failure'); END;"
        ).unwrap();
        assert!(
            core.save_chat_reply(cid, Some(&control.id), "Fictional summary", &records, None)
                .await
                .is_err()
        );
        let failed_save = core.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(failed_save.messages.len(), 2);
        assert_eq!(
            failed_save
                .conversation
                .atomic_chat_state
                .unwrap()
                .recap
                .unwrap()
                .outcome,
            Outcome::Running
        );
        core.database()
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_recap;")
            .unwrap();
        let reply = core
            .save_chat_reply(cid, Some(&control.id), "Fictional summary", &records, None)
            .await
            .unwrap();
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        let state = saved.conversation.atomic_chat_state.unwrap();
        assert_eq!(state.phase, Phase::Closing);
        assert_eq!(state.active, ordinary);
        assert_eq!(
            state.recap.unwrap().outcome,
            Outcome::Complete {
                reply_message_id: reply.id.clone()
            }
        );
        assert!(
            core.admit_chat_turn(cid, "TestOwner", "TestSoul", &Backend::Api, &recap, 200_000)
                .await
                .unwrap_err()
                .to_string()
                .contains("Reuse")
        );
        assert_eq!(
            core.get_conversation(cid)
                .await
                .unwrap()
                .unwrap()
                .messages
                .len(),
            3
        );
        core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closed)
            .await
            .unwrap();
        let saved = core.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(
            saved.messages.last().unwrap().message.content,
            "Fictional summary"
        );
        assert_eq!(
            saved.conversation.atomic_chat_state.unwrap().active,
            ordinary
        );
    }

    #[tokio::test]
    async fn lifecycle_unknown_native_recap_stays_blocked_after_api_recovery() {
        use crate::{
            AtomicChatBackend as Backend, AtomicChatPhase as Phase,
            AtomicChatRecapOutcome as Outcome, AtomicChatRequest as Request,
        };
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("recap-unknown.db")).unwrap();
        for backend in [Backend::Api, Backend::ClaudeCode] {
            let conv = core
                .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
                .await
                .unwrap();
            let cid = &conv.conversation.id;
            core.set_chat_phase(cid, "TestOwner", "TestSoul", &backend, &Phase::Closing)
                .await
                .unwrap();
            let request = Request::Recap {
                instruction: "Fictional tool-free recap".into(),
            };
            let control = core
                .admit_chat_turn(cid, "TestOwner", "TestSoul", &backend, &request, 200_000)
                .await
                .unwrap();
            core.fail_chat_turn(cid, &control.id, "Fictional lost result", true, None)
                .await
                .unwrap();
            let before = core
                .get_conversation(cid)
                .await
                .unwrap()
                .unwrap()
                .conversation
                .atomic_chat_state
                .unwrap();
            assert_eq!(before.context_window_tokens, Some(200_000));
            assert!(matches!(
                before.recap.as_ref().unwrap().outcome,
                Outcome::Uncertain { .. }
            ));
            let retry = core
                .admit_chat_turn(cid, "TestOwner", "TestSoul", &backend, &request, 128_000)
                .await;
            if backend == Backend::Api {
                assert_eq!(retry.unwrap().id, control.id);
            } else {
                assert!(retry.is_err());
                assert_eq!(
                    core.get_conversation(cid)
                        .await
                        .unwrap()
                        .unwrap()
                        .conversation
                        .atomic_chat_state
                        .unwrap(),
                    before
                );
            }
        }
    }

    #[tokio::test]
    async fn lifecycle_legacy_retry_distinguishes_recap_and_unknown_effects() {
        use crate::{
            AtomicChatBackend as Backend, AtomicChatPhase as Phase, AtomicChatRequest as Request,
        };
        let dir = TempDir::new().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("legacy.db")).unwrap();
        for (content, tool_status) in [
            ("Fictional legacy request", None),
            ("Fictional interrupted request", Some("failed")),
            ("Fictional running tool", Some("running")),
            (
                "This Atomic session is ending. Write a recap of your activity with TestOwner",
                None,
            ),
        ] {
            let conv = core
                .create_conversation(&[], None, Some(("TestOwner", "TestSoul")))
                .await
                .unwrap();
            let cid = &conv.conversation.id;
            let user = core.save_message(cid, "user", content).await.unwrap();
            if let Some(status) = tool_status {
                let call = ChatToolCall {
                    id: Uuid::new_v4().to_string(),
                    message_id: user.id.clone(),
                    tool_name: "edit_atom".into(),
                    tool_input: json!({}),
                    tool_output: (status != "running").then(|| json!({"outcome_unknown":true})),
                    status: status.into(),
                    created_at: user.created_at.clone(),
                    completed_at: None,
                };
                core.storage
                    .save_chat_tool_state_sync(cid, &user.id, &[call], &[], 0)
                    .await
                    .unwrap();
            }
            let result = core
                .admit_chat_turn(
                    cid,
                    "TestOwner",
                    "TestSoul",
                    &Backend::Api,
                    &Request::Retry {
                        user_message_id: user.id.clone(),
                    },
                    200_000,
                )
                .await;
            if tool_status.is_some() {
                assert!(result.unwrap_err().to_string().contains("unknown"));
            } else if content.starts_with("This Atomic") {
                assert!(result.unwrap_err().to_string().contains("legacy recap"));
                core.set_chat_phase(cid, "TestOwner", "TestSoul", &Backend::Api, &Phase::Closing)
                    .await
                    .unwrap();
                let recap = core
                    .admit_chat_turn(
                        cid,
                        "TestOwner",
                        "TestSoul",
                        &Backend::Api,
                        &Request::Recap {
                            instruction: "Do not insert another control".into(),
                        },
                        200_000,
                    )
                    .await
                    .unwrap();
                assert_eq!(recap.id, user.id);
            } else {
                let reused = result.unwrap();
                assert_eq!(reused.id, user.id);
                let state = core
                    .get_conversation(cid)
                    .await
                    .unwrap()
                    .unwrap()
                    .conversation
                    .atomic_chat_state
                    .unwrap();
                assert!(state.active.unwrap().page_context.is_none());
            }
            assert_eq!(
                core.get_conversation(cid)
                    .await
                    .unwrap()
                    .unwrap()
                    .messages
                    .len(),
                1
            );
        }
        let old: crate::AtomicChatState = serde_json::from_value(json!({"backend":"api", "phase":"open", "active":{"user_message_id":"old", "status":"failed", "error":null}})).unwrap();
        assert!(old.recap.is_none() && old.context_window_tokens.is_none());
        assert!(old.active.unwrap().canvas_context.is_none());
    }

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
            core.admit_chat_turn(
                cid,
                "WrongOwner",
                "TestSoul",
                &backend,
                &new_chat_request("No admission"),
                200_000
            )
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
            core.admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &backend,
                &new_chat_request("Rolled back input"),
                200_000
            )
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
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &backend,
                &new_chat_request("A fictional turn"),
                200_000,
            )
            .await
            .unwrap();
        let reopened = AtomicCore::open_or_create(&db_path).unwrap();
        let saved = reopened.get_conversation(cid).await.unwrap().unwrap();
        let state = saved.conversation.atomic_chat_state.as_ref().unwrap();
        assert_eq!(state.backend, backend);
        let first_attempt = state.active.as_ref().unwrap().native_attempt_id.clone().unwrap();
        assert_eq!(state.active.as_ref().unwrap().user_message_id, user.id);
        assert_eq!(
            state.active.as_ref().unwrap().status,
            AtomicChatTurnStatus::Running
        );
        assert!(
            reopened
                .admit_chat_turn(
                    cid,
                    "TestOwner",
                    "TestSoul",
                    &backend,
                    &new_chat_request("Duplicate"),
                    200_000
                )
                .await
                .is_err()
        );
        let mismatch = reopened
            .admit_chat_turn(
                cid,
                "TestOwner",
                "TestSoul",
                &AtomicChatBackend::Api,
                &new_chat_request("Wrong backend"),
                200_000,
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
        let usage = crate::AtomicChatUsage {
            turn_id: user.id.clone(), message_id: "native-message".into(),
            input_tokens: 12, cache_creation_input_tokens: 30,
            cache_read_input_tokens: 100, output_tokens: 8,
        };
        let outcome = |usage: &crate::AtomicChatUsage, attempt: &str| crate::AtomicChatNativeOutcome { attempt_id: attempt.into(), usage: Some(usage.clone()), input_recorded: Some(true), context_tokens: Some(150) };
        reopened.fail_chat_turn(cid, &user.id, "Recorded native failure", false, Some(&outcome(&usage, &first_attempt)))
            .await.unwrap();
        let failed = reopened.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(failed.conversation.atomic_chat_state.as_ref().unwrap().active.as_ref().unwrap().native_input_recorded, Some(true));
        assert_eq!(failed.conversation.atomic_chat_state.as_ref().unwrap().native_context_tokens, Some(150));
        assert_eq!(failed.conversation.atomic_chat_state.unwrap().native_usage, Some(usage.clone()));
        let before = serde_json::to_value(reopened.get_conversation(cid).await.unwrap()).unwrap();
        let retry = crate::AtomicChatRequest::Retry { user_message_id: user.id.clone() };
        let native_scope = MemuToolConfig { base_url: "http://127.0.0.1:1".into(),
            user_id: "TestOwner".into(), soul_id: "TestSoul".into() };
        let (projected, tokens) = reopened.project_claude_chat_input(cid, &retry,
            HashMap::new(), native_scope.clone(), false, Some("Continue the recorded turn.")).await.unwrap();
        assert!(projected.system_prompt.is_none());
        assert!(projected.prompt.ends_with("Continue the recorded turn."));
        assert!(!projected.prompt.contains("A fictional turn"));
        assert!(tokens > count_tokens(&projected.prompt));
        let recap = crate::AtomicChatRequest::Recap { instruction: "Recap without tools.".into() };
        let (projected, tokens) = reopened.project_claude_chat_input(cid, &recap,
            HashMap::new(), native_scope.clone(), false, None).await.unwrap();
        assert!(projected.system_prompt.is_none());
        assert_eq!(projected.prompt, "Recap without tools.");
        assert_eq!(tokens, count_tokens(&projected.prompt) + 8);
        assert!(reopened.project_claude_chat_input(cid, &recap, HashMap::new(),
            native_scope, true, None).await.is_err());
        assert_eq!(serde_json::to_value(reopened.get_conversation(cid).await.unwrap()).unwrap(), before);
        assert!(reopened.fail_chat_turn(cid, &user.id, "Late failure", false, None)
            .await.is_err());
        reopened.admit_chat_turn(cid, "TestOwner", "TestSoul", &backend,
            &crate::AtomicChatRequest::Retry { user_message_id: user.id.clone() }, 200_000)
            .await.unwrap();
        let retry_state = reopened.get_conversation(cid).await.unwrap().unwrap().conversation.atomic_chat_state.unwrap();
        let retry_attempt = retry_state.active.unwrap().native_attempt_id.unwrap();
        assert_ne!(retry_attempt, first_attempt);
        assert!(reopened.fail_chat_turn(cid, &user.id, "Stale native attempt", true, Some(&outcome(&usage, &first_attempt))).await.is_err());
        let final_usage = crate::AtomicChatUsage { input_tokens: 20, ..usage.clone() };
        let wrong_usage = crate::AtomicChatUsage { turn_id: "other-turn".into(), ..final_usage.clone() };
        assert!(reopened.save_chat_reply(cid, Some(&user.id), "Wrong usage", &records, Some(&outcome(&wrong_usage, &retry_attempt)))
            .await.is_err());
        assert!(
            reopened
                .save_chat_reply(cid, Some("other-turn"), "Wrong result", &records, None)
                .await
                .is_err()
        );
        assert!(
            reopened
                .save_chat_reply(cid, None, "Unbound result", &records, None)
                .await
                .is_err()
        );
        assert!(
            reopened
                .fail_chat_turn(cid, "other-turn", "Stale error", true, None)
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
                .save_chat_reply(cid, Some(&user.id), "Retained reply", &records, Some(&outcome(&final_usage, &retry_attempt)))
                .await
                .is_err()
        );
        let retained = reopened.get_conversation(cid).await.unwrap().unwrap();
        assert_eq!(retained.messages.len(), 1);
        assert_eq!(retained.conversation.atomic_chat_state.as_ref().unwrap().native_usage, Some(usage));
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
        reopened.fail_chat_turn(cid, &user.id, "Lost delivery; native result recoverable", true,
            Some(&crate::AtomicChatNativeOutcome { attempt_id: retry_attempt.clone(), usage: None, input_recorded: Some(false), context_tokens: None })).await.unwrap();
        assert_eq!(reopened.get_conversation(cid).await.unwrap().unwrap().conversation.atomic_chat_state.unwrap().native_context_tokens, None);
        assert_eq!(reopened.get_conversation(cid).await.unwrap().unwrap().conversation.atomic_chat_state.unwrap().active.unwrap().native_input_recorded, Some(true));
        reopened
            .save_chat_reply(cid, Some(&user.id), "Retained reply", &records, Some(&outcome(&final_usage, &retry_attempt)))
            .await
            .unwrap();
        let completed = reopened.get_conversation(cid).await.unwrap().unwrap();
        let state = completed.conversation.atomic_chat_state.unwrap();
        assert_eq!(state.phase, AtomicChatPhase::Closing);
        assert!(state.active.is_none());
        assert_eq!(state.native_usage, Some(final_usage));
        assert!(
            reopened
                .save_chat_reply(cid, Some(&user.id), "Duplicate reply", &records, None)
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
                .admit_chat_turn(
                    cid,
                    "TestOwner",
                    "TestSoul",
                    &backend,
                    &new_chat_request("After closure"),
                    200_000
                )
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
                    &new_chat_request("Fictional turn"),
                    200_000,
                )
                .await
                .unwrap();
            core.fail_chat_turn(cid, &user.id, "Fictional failure", uncertain, None)
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
            if uncertain {
                let error = reopened
                    .admit_chat_turn(
                        cid,
                        "TestOwner",
                        "TestSoul",
                        &AtomicChatBackend::Api,
                        &crate::AtomicChatRequest::Retry {
                            user_message_id: user.id.clone(),
                        },
                        200_000,
                    )
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("Close out"));
            }
            assert!(
                reopened
                    .admit_chat_turn(
                        cid,
                        "TestOwner",
                        "TestSoul",
                        &AtomicChatBackend::Api,
                        &new_chat_request("Do not replay"),
                        200_000
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
                &new_chat_request("No reconstruction"),
                200_000
            )
            .await
            .is_err()
        );
        core.admit_chat_turn(
            cid,
            "TestOwner",
            "TestSoul",
            &AtomicChatBackend::Api,
            &new_chat_request("Continue API"),
            200_000,
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
                &new_chat_request("No duplicate"),
                200_000,
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
        let before = serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap();
        let draft = crate::AtomicChatRequest::New {
            content: user.content.clone(),
            page_context: Some(PageContext {
                view: Some("atom".to_string()),
                atom_id: None,
                atom_title: None,
                atom_snippet: Some("Hidden page contents".to_string()),
                wiki_tag_id: None,
                wiki_tag_name: None,
                selected_tag_id: None,
            }),
            canvas_context: Some(CanvasContext {
                clusters: vec![CanvasClusterSummary {
                    label: "Fictional astronomy".to_string(),
                    atom_count: 12,
                }],
            }),
        };
        let (initial, tokens) = core
            .project_claude_chat_input(cid, &draft, settings.clone(), scope.clone(), true, None)
            .await
            .unwrap();
        assert!(tokens > count_tokens(&initial.prompt));
        let mut other = scope.clone();
        other.soul_id = "OtherSoul".into();
        assert!(core
            .project_claude_chat_input(cid, &draft, settings.clone(), other, true, None)
            .await
            .is_err());
        assert_eq!(
            serde_json::to_value(core.get_conversation(cid).await.unwrap()).unwrap(),
            before
        );
        let system = initial.system_prompt.unwrap();
        assert!(system.starts_with("Custom Atomic prefix\n\n"));
        assert!(system.ends_with(presence));
        assert!(!system.contains("Fictional astronomy"));
        assert!(!system.contains("get_current_page_context"));
        assert!(!system.contains("4 sentences"));
        assert!(initial.prompt.contains("Fictional astronomy"));
        assert!(initial.prompt.contains("get_current_page_context"));
        assert!(initial
            .prompt
            .ends_with("Discuss this chart\n\n**respond with 4 sentences or fewer**"));
        for omitted in [
            presence,
            "Custom Atomic prefix",
            "Earlier user dialogue",
            "Earlier assistant dialogue",
            "Hidden page contents",
        ] {
            assert!(!initial.prompt.contains(omitted));
        }
        let (resumed, _) = core
            .project_claude_chat_input(cid, &draft, settings.clone(), scope.clone(), false, None)
            .await
            .unwrap();
        assert!(resumed.system_prompt.is_none());
        assert_eq!(resumed.prompt, initial.prompt);

        let next = core
            .save_message(cid, "user", "Next saved turn")
            .await
            .unwrap();
        let next_request = crate::AtomicChatRequest::New {
            content: next.content,
            page_context: None,
            canvas_context: None,
        };
        let (next_input, _) = core
            .project_claude_chat_input(
                cid,
                &next_request,
                HashMap::from([(MEMU_RESPONSE_SENTENCES_SETTING.to_string(), "2".to_string())]),
                scope.clone(),
                false,
                None,
            )
            .await
            .unwrap();
        assert!(next_input.system_prompt.is_none());
        assert!(next_input
            .prompt
            .ends_with("Next saved turn\n\n**respond with 2 sentences or fewer**"));
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
        let other_request = crate::AtomicChatRequest::New {
            content: other_user.content,
            page_context: None,
            canvas_context: None,
        };
        assert!(core
            .project_claude_chat_input(
                &other.conversation.id,
                &other_request,
                HashMap::new(),
                MemuToolConfig {
                    soul_id: "OtherSoul".into(),
                    ..scope
                },
                true,
                None
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("no saved Soul Presence"));
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
                    .save_chat_reply(cid, None, reply_text, &records, None)
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
            .save_chat_reply(cid, None, reply_text, &records, None)
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
            core.save_chat_reply(other_id, None, "Must not publish across sittings", &attempt, None)
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
                    None,
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
                user_id: "TestOwner".to_string(),
                soul_id: "TestSoul".to_string(),
            }),
        );

        assert!(prompt.contains("You are with TestOwner in the app Atomic"));
        assert!(prompt.contains("In Atomic, TestOwner is the user."));
        assert!(prompt.contains("Guidelines for Atomic:"));
        assert!(!prompt.contains("helpful AI assistant"));
    }

    #[test]
    fn memu_node_to_atom_uses_stable_category_ids() {
        let atom = memu_node_to_atom(MemuNode {
            id: "memory:m1".to_string(),
            kind: "memory".to_string(),
            label: "Memory".to_string(),
            description: String::new(),
            summaries_revision: None,
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
