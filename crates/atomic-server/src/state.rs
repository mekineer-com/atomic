//! Application state and server event types

use crate::export_jobs::ExportJobManager;
use crate::log_buffer::LogBuffer;
use atomic_core::{AtomicCore, DatabaseManager};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, Mutex as AsyncMutex};

const SETUP_CLAIM_LIMIT: usize = 10;
const SETUP_CLAIM_WINDOW: Duration = Duration::from_secs(60);

/// Hashed setup token configured through ATOMIC_SETUP_TOKEN.
pub struct SetupToken {
    hash: String,
}

impl SetupToken {
    pub fn from_raw(raw: String) -> Option<Self> {
        let token = raw.trim();
        if token.is_empty() {
            return None;
        }
        Some(Self {
            hash: hash_setup_token(token),
        })
    }

    pub fn verify(&self, candidate: &str) -> bool {
        hash_setup_token(candidate.trim()) == self.hash
    }
}

fn hash_setup_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Small in-memory limiter for the public setup claim endpoint.
pub struct SetupClaimLimiter {
    attempts: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl SetupClaimLimiter {
    pub fn new() -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
        }
    }

    pub fn check(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut attempts = match self.attempts.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let entries = attempts.entry(ip).or_default();
        while entries
            .front()
            .is_some_and(|at| now.duration_since(*at) > SETUP_CLAIM_WINDOW)
        {
            entries.pop_front();
        }
        if entries.len() >= SETUP_CLAIM_LIMIT {
            return false;
        }
        entries.push_back(now);
        true
    }
}

#[derive(Clone)]
pub struct MemuSessionConfig {
    pub base_url: String,
}

/// Shared application state for all route handlers
pub struct AppState {
    pub manager: Arc<DatabaseManager>,
    pub event_tx: broadcast::Sender<ServerEvent>,
    /// Public URL for OAuth discovery (set via --public-url CLI flag)
    pub public_url: Option<String>,
    /// In-memory ring buffer for recent log lines (for user export)
    pub log_buffer: LogBuffer,
    /// memU session-start endpoint used to seed every Atomic chat with Siri context.
    pub memu_session: Option<MemuSessionConfig>,
    /// Background database export jobs and temporary artifacts.
    pub export_jobs: ExportJobManager,
    /// Optional setup token required for first-run claims.
    pub setup_token: Option<SetupToken>,
    /// Explicit unsafe opt-out from requiring ATOMIC_SETUP_TOKEN for setup claims.
    pub dangerously_skip_setup_token: bool,
    /// Serializes setup claims inside this process.
    pub setup_claim_lock: AsyncMutex<()>,
    /// Rate-limits setup claim attempts by client IP.
    pub setup_claim_limiter: SetupClaimLimiter,
}

impl AppState {
    /// Resolve which database core to use for a request.
    /// Checks X-Atomic-Database header, then ?db= query param, then falls back to active.
    pub async fn resolve_core(
        &self,
        req: &actix_web::HttpRequest,
    ) -> Result<AtomicCore, atomic_core::AtomicCoreError> {
        self.resolve_core_with_id(req).await.map(|(core, _)| core)
    }

    pub async fn resolve_core_with_id(
        &self,
        req: &actix_web::HttpRequest,
    ) -> Result<(AtomicCore, String), atomic_core::AtomicCoreError> {
        if self.memu_session.is_some() {
            let (user_id, soul_id) = openalma_identity(req)?;
            return self
                .resolve_workspace_core(&user_id, &soul_id, requested_database(req).as_deref())
                .await;
        }

        // Check X-Atomic-Database header
        if let Some(db_id) = req
            .headers()
            .get("X-Atomic-Database")
            .and_then(|v| v.to_str().ok())
        {
            return Ok((self.manager.get_core(db_id).await?, db_id.to_string()));
        }

        // Check ?db= query parameter
        if let Some(db_id) = req.query_string().split('&').find_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            if parts.next()? == "db" {
                parts.next()
            } else {
                None
            }
        }) {
            return Ok((self.manager.get_core(db_id).await?, db_id.to_string()));
        }

        // Default to active database
        let db_id = self.manager.active_id()?;
        Ok((self.manager.get_core(&db_id).await?, db_id))
    }

    pub fn workspace_database_id(
        &self,
        user_id: &str,
        soul_id: &str,
    ) -> Result<Option<String>, atomic_core::AtomicCoreError> {
        let registry = self.manager.registry().ok_or_else(|| {
            atomic_core::AtomicCoreError::Configuration(
                "OpenAlma Soul workspaces require SQLite".to_string(),
            )
        })?;
        Ok(registry
            .get_all_settings()?
            .remove(&workspace_setting_key(user_id, soul_id)))
    }

    pub fn bind_workspace_database(
        &self,
        user_id: &str,
        soul_id: &str,
        database_id: &str,
    ) -> Result<(), atomic_core::AtomicCoreError> {
        let registry = self.manager.registry().ok_or_else(|| {
            atomic_core::AtomicCoreError::Configuration(
                "OpenAlma Soul workspaces require SQLite".to_string(),
            )
        })?;
        registry.set_setting(&workspace_setting_key(user_id, soul_id), database_id)
    }

    pub async fn resolve_workspace_core(
        &self,
        user_id: &str,
        soul_id: &str,
        requested: Option<&str>,
    ) -> Result<(AtomicCore, String), atomic_core::AtomicCoreError> {
        let expected_id = self
            .workspace_database_id(user_id, soul_id)?
            .ok_or_else(|| {
                atomic_core::AtomicCoreError::Configuration(
                    "OpenAlma Soul workspace is not initialized".to_string(),
                )
            })?;
        if let Some(requested) = requested {
            let (databases, _) = self.manager.list_databases().await?;
            let resolved = databases
                .iter()
                .find(|db| db.id == requested || db.name.eq_ignore_ascii_case(requested))
                .map(|db| db.id.as_str())
                .ok_or_else(|| {
                    atomic_core::AtomicCoreError::NotFound(format!("Database '{}'", requested))
                })?;
            if resolved != expected_id {
                return Err(atomic_core::AtomicCoreError::Configuration(
                    "Database does not belong to the selected OpenAlma Soul".to_string(),
                ));
            }
        }
        Ok((self.manager.get_core(&expected_id).await?, expected_id))
    }
}

fn requested_database(req: &actix_web::HttpRequest) -> Option<String> {
    req.headers()
        .get("X-Atomic-Database")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            req.query_string().split('&').find_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                (parts.next()? == "db").then(|| parts.next().unwrap_or_default().to_string())
            })
        })
}

pub(crate) fn openalma_identity(
    req: &actix_web::HttpRequest,
) -> Result<(String, String), atomic_core::AtomicCoreError> {
    let value = |name| {
        req.headers()
            .get(name)
            .and_then(|header| header.to_str().ok())
            .filter(|header| !header.is_empty())
            .and_then(|header| {
                reqwest::Url::parse(&format!("http://localhost/?value={header}"))
                    .ok()?
                    .query_pairs()
                    .find_map(|(key, value)| (key == "value").then(|| value.into_owned()))
            })
            .ok_or_else(|| {
                atomic_core::AtomicCoreError::Configuration(format!("{name} is required"))
            })
    };
    Ok((value("X-OpenAlma-User")?, value("X-OpenAlma-Soul")?))
}

fn workspace_setting_key(user_id: &str, soul_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update([0]);
    hasher.update(soul_id.as_bytes());
    format!("openalma.soul_database.{:x}", hasher.finalize())
}

/// Events broadcast to WebSocket clients
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    // Embedding pipeline events
    EmbeddingStarted {
        atom_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        database_id: Option<String>,
    },
    EmbeddingComplete {
        atom_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        database_id: Option<String>,
    },
    EmbeddingFailed {
        atom_id: String,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        database_id: Option<String>,
    },
    TaggingComplete {
        atom_id: String,
        tags_extracted: Vec<String>,
        new_tags_created: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        database_id: Option<String>,
    },
    TaggingFailed {
        atom_id: String,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        database_id: Option<String>,
    },
    TaggingSkipped {
        atom_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        database_id: Option<String>,
    },
    BatchProgress {
        batch_id: String,
        phase: String,
        completed: usize,
        total: usize,
    },
    PipelineQueueStarted {
        run_id: String,
        total_jobs: usize,
        embedding_total: usize,
    },
    PipelineQueueProgress {
        run_id: String,
        stage: String,
        completed: usize,
        total: usize,
    },
    PipelineQueueCompleted {
        run_id: String,
        total_jobs: usize,
        failed_jobs: usize,
    },
    EventsLagged {
        skipped: u64,
    },

    // Atom lifecycle events
    AtomCreated {
        atom: atomic_core::AtomWithTags,
        user_id: Option<String>,
        soul_id: Option<String>,
        database_id: Option<String>,
    },
    AtomUpdated {
        atom: atomic_core::AtomWithTags,
        user_id: Option<String>,
        soul_id: Option<String>,
        database_id: Option<String>,
    },
    MemuReviewsChanged {
        category_id: String,
        summaries_revision: i64,
        pending: bool,
        user_id: String,
        soul_id: String,
    },

    /// The per-DB `dashboard.featured_report_id` pointer changed.
    /// Broadcast on every write through the dashboard route so the
    /// BriefingWidget and any open detail-view star can refetch.
    /// `report_id` is `None` when the pointer was cleared.
    DashboardFeaturedChanged {
        report_id: Option<String>,
    },

    // Import progress events
    ImportProgress {
        current: i32,
        total: i32,
        current_file: String,
        status: String,
    },

    // Ingestion pipeline events
    IngestionFetchStarted {
        url: String,
        request_id: String,
    },
    IngestionFetchComplete {
        url: String,
        request_id: String,
        content_length: usize,
    },
    IngestionFetchFailed {
        url: String,
        request_id: String,
        error: String,
    },
    IngestionSkipped {
        url: String,
        request_id: String,
        reason: String,
    },
    IngestionComplete {
        request_id: String,
        atom_id: String,
        url: String,
        title: String,
    },
    IngestionFailed {
        request_id: String,
        url: String,
        error: String,
    },
    FeedPollComplete {
        feed_id: String,
        new_items: i32,
        skipped: i32,
        errors: i32,
    },
    FeedPollFailed {
        feed_id: String,
        error: String,
    },

    // Chat streaming events
    ChatStreamDelta {
        database_id: String,
        conversation_id: String,
        content: String,
    },
    ChatToolStart {
        database_id: String,
        conversation_id: String,
        tool_call_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    ChatToolComplete {
        database_id: String,
        conversation_id: String,
        tool_call_id: String,
        results_count: i32,
    },
    ChatComplete {
        database_id: String,
        conversation_id: String,
        message: atomic_core::ChatMessageWithContext,
    },
    ChatCanvasAction {
        database_id: String,
        conversation_id: String,
        action: String,
        params: serde_json::Value,
    },
    ChatError {
        database_id: String,
        conversation_id: String,
        error: String,
    },
}

impl ServerEvent {
    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::ChatStreamDelta {
                conversation_id, ..
            }
            | Self::ChatToolStart {
                conversation_id, ..
            }
            | Self::ChatToolComplete {
                conversation_id, ..
            }
            | Self::ChatComplete {
                conversation_id, ..
            }
            | Self::ChatCanvasAction {
                conversation_id, ..
            }
            | Self::ChatError {
                conversation_id, ..
            } => Some(conversation_id),
            _ => None,
        }
    }

    pub fn database_id(&self) -> Option<&str> {
        match self {
            Self::ChatStreamDelta { database_id, .. }
            | Self::ChatToolStart { database_id, .. }
            | Self::ChatToolComplete { database_id, .. }
            | Self::ChatComplete { database_id, .. }
            | Self::ChatCanvasAction { database_id, .. }
            | Self::ChatError { database_id, .. } => Some(database_id),
            Self::AtomCreated { database_id, .. } | Self::AtomUpdated { database_id, .. } => {
                database_id.as_deref()
            }
            Self::EmbeddingStarted { database_id, .. }
            | Self::EmbeddingComplete { database_id, .. }
            | Self::EmbeddingFailed { database_id, .. }
            | Self::TaggingComplete { database_id, .. }
            | Self::TaggingFailed { database_id, .. }
            | Self::TaggingSkipped { database_id, .. } => database_id.as_deref(),
            _ => None,
        }
    }

    pub fn owner(&self) -> Option<(&str, &str)> {
        match self {
            Self::AtomCreated {
                user_id, soul_id, ..
            }
            | Self::AtomUpdated {
                user_id, soul_id, ..
            } => Some((user_id.as_deref()?, soul_id.as_deref()?)),
            Self::MemuReviewsChanged {
                user_id, soul_id, ..
            } => Some((user_id, soul_id)),
            _ => None,
        }
    }

    pub fn with_database_id(mut self, value: String) -> Self {
        match &mut self {
            Self::EmbeddingStarted { database_id, .. }
            | Self::EmbeddingComplete { database_id, .. }
            | Self::EmbeddingFailed { database_id, .. }
            | Self::TaggingComplete { database_id, .. }
            | Self::TaggingFailed { database_id, .. }
            | Self::TaggingSkipped { database_id, .. } => *database_id = Some(value),
            _ => {}
        }
        self
    }

    pub fn from_chat(
        event: atomic_core::ChatEvent,
        database_id: String,
        owner: Option<(String, String)>,
    ) -> Self {
        match event {
            atomic_core::ChatEvent::StreamDelta {
                conversation_id,
                content,
            } => Self::ChatStreamDelta {
                database_id,
                conversation_id,
                content,
            },
            atomic_core::ChatEvent::ToolStart {
                conversation_id,
                tool_call_id,
                tool_name,
                tool_input,
            } => Self::ChatToolStart {
                database_id,
                conversation_id,
                tool_call_id,
                tool_name,
                tool_input,
            },
            atomic_core::ChatEvent::ToolComplete {
                conversation_id,
                tool_call_id,
                results_count,
            } => Self::ChatToolComplete {
                database_id,
                conversation_id,
                tool_call_id,
                results_count,
            },
            atomic_core::ChatEvent::Complete {
                conversation_id,
                message,
            } => Self::ChatComplete {
                database_id,
                conversation_id,
                message,
            },
            atomic_core::ChatEvent::CanvasAction {
                conversation_id,
                action,
                params,
            } => Self::ChatCanvasAction {
                database_id,
                conversation_id,
                action,
                params,
            },
            atomic_core::ChatEvent::AtomCreated { atom, .. } => Self::AtomCreated {
                atom,
                user_id: owner.as_ref().map(|(user_id, _)| user_id.clone()),
                soul_id: owner.as_ref().map(|(_, soul_id)| soul_id.clone()),
                database_id: Some(database_id),
            },
            atomic_core::ChatEvent::AtomUpdated { atom, .. } => Self::AtomUpdated {
                atom,
                user_id: owner.as_ref().map(|(user_id, _)| user_id.clone()),
                soul_id: owner.as_ref().map(|(_, soul_id)| soul_id.clone()),
                database_id: Some(database_id),
            },
            atomic_core::ChatEvent::AtomPipelineEvent { event, .. } => {
                Self::from(event).with_database_id(database_id)
            }
            atomic_core::ChatEvent::Error {
                conversation_id,
                error,
            } => Self::ChatError {
                database_id,
                conversation_id,
                error,
            },
        }
    }
}

impl From<atomic_core::EmbeddingEvent> for ServerEvent {
    fn from(event: atomic_core::EmbeddingEvent) -> Self {
        match event {
            atomic_core::EmbeddingEvent::Started { atom_id } => ServerEvent::EmbeddingStarted {
                atom_id,
                database_id: None,
            },
            atomic_core::EmbeddingEvent::EmbeddingComplete { atom_id } => {
                ServerEvent::EmbeddingComplete {
                    atom_id,
                    database_id: None,
                }
            }
            atomic_core::EmbeddingEvent::EmbeddingFailed { atom_id, error } => {
                ServerEvent::EmbeddingFailed {
                    atom_id,
                    error,
                    database_id: None,
                }
            }
            atomic_core::EmbeddingEvent::TaggingComplete {
                atom_id,
                tags_extracted,
                new_tags_created,
            } => ServerEvent::TaggingComplete {
                atom_id,
                tags_extracted,
                new_tags_created,
                database_id: None,
            },
            atomic_core::EmbeddingEvent::TaggingFailed { atom_id, ref error } => {
                tracing::warn!(atom_id, error = %error, "Tagging failed");
                ServerEvent::TaggingFailed {
                    atom_id,
                    error: error.clone(),
                    database_id: None,
                }
            }
            atomic_core::EmbeddingEvent::TaggingSkipped { atom_id } => {
                ServerEvent::TaggingSkipped {
                    atom_id,
                    database_id: None,
                }
            }
            atomic_core::EmbeddingEvent::BatchProgress {
                batch_id,
                phase,
                completed,
                total,
            } => ServerEvent::BatchProgress {
                batch_id,
                phase,
                completed,
                total,
            },
            atomic_core::EmbeddingEvent::PipelineQueueStarted {
                run_id,
                total_jobs,
                embedding_total,
            } => ServerEvent::PipelineQueueStarted {
                run_id,
                total_jobs,
                embedding_total,
            },
            atomic_core::EmbeddingEvent::PipelineQueueProgress {
                run_id,
                stage,
                completed,
                total,
            } => ServerEvent::PipelineQueueProgress {
                run_id,
                stage,
                completed,
                total,
            },
            atomic_core::EmbeddingEvent::PipelineQueueCompleted {
                run_id,
                total_jobs,
                failed_jobs,
            } => ServerEvent::PipelineQueueCompleted {
                run_id,
                total_jobs,
                failed_jobs,
            },
        }
    }
}

impl From<atomic_core::IngestionEvent> for ServerEvent {
    fn from(event: atomic_core::IngestionEvent) -> Self {
        match event {
            atomic_core::IngestionEvent::FetchStarted { url, request_id } => {
                ServerEvent::IngestionFetchStarted { url, request_id }
            }
            atomic_core::IngestionEvent::FetchComplete {
                url,
                request_id,
                content_length,
            } => ServerEvent::IngestionFetchComplete {
                url,
                request_id,
                content_length,
            },
            atomic_core::IngestionEvent::FetchFailed {
                url,
                request_id,
                error,
            } => ServerEvent::IngestionFetchFailed {
                url,
                request_id,
                error,
            },
            atomic_core::IngestionEvent::Skipped {
                url,
                request_id,
                reason,
            } => ServerEvent::IngestionSkipped {
                url,
                request_id,
                reason,
            },
            atomic_core::IngestionEvent::IngestionComplete {
                request_id,
                atom_id,
                url,
                title,
            } => ServerEvent::IngestionComplete {
                request_id,
                atom_id,
                url,
                title,
            },
            atomic_core::IngestionEvent::IngestionFailed {
                request_id,
                url,
                error,
            } => ServerEvent::IngestionFailed {
                request_id,
                url,
                error,
            },
            atomic_core::IngestionEvent::FeedPollComplete {
                feed_id,
                new_items,
                skipped,
                errors,
            } => ServerEvent::FeedPollComplete {
                feed_id,
                new_items,
                skipped,
                errors,
            },
            atomic_core::IngestionEvent::FeedPollFailed { feed_id, error } => {
                ServerEvent::FeedPollFailed { feed_id, error }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_started_conversion() {
        let event = atomic_core::EmbeddingEvent::Started {
            atom_id: "a1".into(),
        };
        let server_event = ServerEvent::from(event);
        match server_event {
            ServerEvent::EmbeddingStarted { atom_id, .. } => assert_eq!(atom_id, "a1"),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_embedding_complete_conversion() {
        let event = atomic_core::EmbeddingEvent::EmbeddingComplete {
            atom_id: "a2".into(),
        };
        match ServerEvent::from(event) {
            ServerEvent::EmbeddingComplete { atom_id, .. } => assert_eq!(atom_id, "a2"),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_embedding_failed_conversion() {
        let event = atomic_core::EmbeddingEvent::EmbeddingFailed {
            atom_id: "a3".into(),
            error: "timeout".into(),
        };
        match ServerEvent::from(event) {
            ServerEvent::EmbeddingFailed { atom_id, error, .. } => {
                assert_eq!(atom_id, "a3");
                assert_eq!(error, "timeout");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_tagging_complete_conversion() {
        let event = atomic_core::EmbeddingEvent::TaggingComplete {
            atom_id: "a4".into(),
            tags_extracted: vec!["t1".into()],
            new_tags_created: vec!["t2".into()],
        };
        match ServerEvent::from(event) {
            ServerEvent::TaggingComplete {
                atom_id,
                tags_extracted,
                new_tags_created,
                ..
            } => {
                assert_eq!(atom_id, "a4");
                assert_eq!(tags_extracted, vec!["t1"]);
                assert_eq!(new_tags_created, vec!["t2"]);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_chat_stream_delta_conversion() {
        let event = atomic_core::ChatEvent::StreamDelta {
            conversation_id: "c1".into(),
            content: "hello".into(),
        };
        match ServerEvent::from_chat(event, "default".into(), None) {
            ServerEvent::ChatStreamDelta {
                conversation_id,
                content,
                database_id,
            } => {
                assert_eq!(conversation_id, "c1");
                assert_eq!(content, "hello");
                assert_eq!(database_id, "default");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_chat_tool_start_conversion() {
        let event = atomic_core::ChatEvent::ToolStart {
            conversation_id: "c2".into(),
            tool_call_id: "tc1".into(),
            tool_name: "search".into(),
            tool_input: serde_json::json!({"query": "test"}),
        };
        match ServerEvent::from_chat(event, "default".into(), None) {
            ServerEvent::ChatToolStart {
                conversation_id,
                tool_name,
                tool_input,
                ..
            } => {
                assert_eq!(conversation_id, "c2");
                assert_eq!(tool_name, "search");
                assert_eq!(tool_input["query"], "test");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_chat_error_conversion() {
        let event = atomic_core::ChatEvent::Error {
            conversation_id: "c3".into(),
            error: "api failed".into(),
        };
        match ServerEvent::from_chat(event, "default".into(), None) {
            ServerEvent::ChatError {
                conversation_id,
                error,
                ..
            } => {
                assert_eq!(conversation_id, "c3");
                assert_eq!(error, "api failed");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_chat_pipeline_event_keeps_database_scope() {
        let event = atomic_core::ChatEvent::AtomPipelineEvent {
            conversation_id: "c4".into(),
            event: atomic_core::EmbeddingEvent::EmbeddingComplete {
                atom_id: "a4".into(),
            },
        };
        assert_eq!(
            ServerEvent::from_chat(event, "workspace".into(), None).database_id(),
            Some("workspace")
        );
    }

    #[test]
    fn test_server_event_serializes_with_type_tag() {
        let event = ServerEvent::EmbeddingComplete {
            atom_id: "a1".into(),
            database_id: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "EmbeddingComplete");
        assert_eq!(json["atom_id"], "a1");
    }

    #[test]
    fn test_event_broadcast_delivery() {
        let (tx, mut rx) = broadcast::channel::<ServerEvent>(16);
        let event = ServerEvent::EmbeddingStarted {
            atom_id: "a1".into(),
            database_id: None,
        };
        tx.send(event).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            ServerEvent::EmbeddingStarted { atom_id, .. } => assert_eq!(atom_id, "a1"),
            _ => panic!("Wrong variant"),
        }
    }
}
