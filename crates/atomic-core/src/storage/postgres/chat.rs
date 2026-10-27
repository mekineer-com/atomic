use super::PostgresStorage;
use crate::chat::truncate_preview;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;
use std::collections::HashMap;

async fn locked_chat_state(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    conversation_id: &str,
) -> StorageResult<Option<AtomicChatState>> {
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT atomic_chat_state FROM conversations WHERE id=$1 AND db_id=$2 FOR UPDATE",
    )
    .bind(conversation_id)
    .bind(db_id)
    .fetch_one(conn)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    Ok(raw.map(|raw| serde_json::from_str(&raw)).transpose()?)
}

async fn owned_chat_state(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    conversation_id: &str,
    user_id: &str,
    soul_id: &str,
) -> StorageResult<Option<AtomicChatState>> {
    let raw: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT atomic_chat_state FROM conversations WHERE id=$1 AND db_id=$2 AND user_id=$3 AND soul_id=$4 FOR UPDATE"
    ).bind(conversation_id).bind(db_id).bind(user_id).bind(soul_id).fetch_optional(conn).await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        .ok_or_else(|| AtomicCoreError::Validation("Chat owner/Soul does not match".to_string()))?;
    Ok(raw.map(|raw| serde_json::from_str(&raw)).transpose()?)
}

async fn last_chat_message(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    conversation_id: &str,
) -> StorageResult<Option<ChatMessage>> {
    let row = sqlx::query_as::<_, (String, String, String, String, String, i32)>("SELECT id, conversation_id, role, content, created_at, message_index FROM chat_messages WHERE conversation_id=$1 AND db_id=$2 AND role IN ('user','assistant') ORDER BY message_index DESC LIMIT 1")
        .bind(conversation_id).bind(db_id).fetch_optional(conn).await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    Ok(row.map(
        |(id, conversation_id, role, content, created_at, message_index)| ChatMessage {
            id,
            conversation_id,
            role,
            content,
            created_at,
            message_index,
        },
    ))
}

async fn write_chat_state(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    conversation_id: &str,
    state: Option<&AtomicChatState>,
) -> StorageResult<()> {
    let raw = state.map(serde_json::to_string).transpose()?;
    sqlx::query("UPDATE conversations SET atomic_chat_state=$1 WHERE id=$2 AND db_id=$3")
        .bind(raw)
        .bind(conversation_id)
        .bind(db_id)
        .execute(conn)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    Ok(())
}

async fn write_message(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    conversation_id: &str,
    role: &str,
    content: &str,
) -> StorageResult<ChatMessage> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let message_index: Option<i32> = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT COALESCE(MAX(message_index), -1) + 1 FROM chat_messages WHERE conversation_id = $1 AND db_id = $2",
        )
        .bind(conversation_id)
        .bind(db_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let message_index = message_index.unwrap_or(0);

    sqlx::query(
            "INSERT INTO chat_messages (id, conversation_id, role, content, created_at, message_index, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&message_id)
        .bind(conversation_id)
        .bind(role)
        .bind(content)
        .bind(&now)
        .bind(message_index as i32)
        .bind(db_id)
        .execute(&mut *conn)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    sqlx::query("UPDATE conversations SET updated_at = $1 WHERE id = $2 AND db_id = $3")
        .bind(&now)
        .bind(conversation_id)
        .bind(db_id)
        .execute(&mut *conn)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    Ok(ChatMessage {
        id: message_id,
        conversation_id: conversation_id.to_string(),
        role: role.to_string(),
        content: content.to_string(),
        created_at: now,
        message_index: message_index as i32,
    })
}

async fn write_chat_tool_state(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    conversation_id: &str,
    message_id: &str,
    tool_calls: &[ChatToolCall],
    citations: &[ChatCitation],
    citation_index: i32,
) -> StorageResult<()> {
    let belongs: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM chat_messages WHERE id=$1 AND conversation_id=$2 AND db_id=$3)")
            .bind(message_id).bind(conversation_id).bind(db_id).fetch_one(&mut *conn).await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    if !belongs {
        return Err(AtomicCoreError::Validation(
            "Tool state target is outside this conversation".to_string(),
        ));
    }
    write_tool_calls(conn, db_id, message_id, tool_calls).await?;
    write_citations(conn, db_id, message_id, citations).await?;
    sqlx::query("UPDATE conversations SET atomic_citation_index=GREATEST(atomic_citation_index,$1) WHERE id=$2 AND db_id=$3")
            .bind(citation_index).bind(conversation_id).bind(db_id).execute(&mut *conn).await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    Ok(())
}

async fn write_tool_calls(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    message_id: &str,
    tool_calls: &[ChatToolCall],
) -> StorageResult<()> {
    for tool_call in tool_calls {
        let tool_input_str = serde_json::to_string(&tool_call.tool_input).unwrap_or_default();
        let tool_result_str = tool_call
            .tool_output
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default();

        let result = sqlx::query(
                "INSERT INTO chat_tool_calls (id, message_id, tool_name, tool_input, tool_result, created_at, db_id, status, completed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT(id) DO UPDATE SET message_id=EXCLUDED.message_id, tool_result=EXCLUDED.tool_result,
                     status=EXCLUDED.status, completed_at=EXCLUDED.completed_at
                 WHERE chat_tool_calls.db_id=EXCLUDED.db_id
                     AND (SELECT conversation_id FROM chat_messages WHERE id=chat_tool_calls.message_id AND db_id=EXCLUDED.db_id)
                         = (SELECT conversation_id FROM chat_messages WHERE id=EXCLUDED.message_id AND db_id=EXCLUDED.db_id)
                 AND (chat_tool_calls.message_id=EXCLUDED.message_id
                     OR ((SELECT role FROM chat_messages WHERE id=chat_tool_calls.message_id)='user'
                         AND (SELECT role FROM chat_messages WHERE id=EXCLUDED.message_id)='assistant'))",
            )
            .bind(&tool_call.id)
            .bind(message_id)
            .bind(&tool_call.tool_name)
            .bind(&tool_input_str)
            .bind(&tool_result_str)
            .bind(&tool_call.created_at)
            .bind(db_id)
            .bind(&tool_call.status)
            .bind(&tool_call.completed_at)
            .execute(&mut *conn)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        if result.rows_affected() != 1 {
            return Err(AtomicCoreError::Validation(
                "Tool record identity or publication target does not match".to_string(),
            ));
        }
    }
    Ok(())
}

async fn write_citations(
    conn: &mut sqlx::PgConnection,
    db_id: &str,
    message_id: &str,
    citations: &[ChatCitation],
) -> StorageResult<()> {
    for citation in citations {
        let result = sqlx::query(
                "INSERT INTO chat_citations (id, message_id, atom_id, chunk_index, excerpt, relevance_score, db_id, citation_index)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT(id) DO UPDATE SET message_id=EXCLUDED.message_id
                 WHERE chat_citations.db_id=EXCLUDED.db_id
                     AND (SELECT conversation_id FROM chat_messages WHERE id=chat_citations.message_id AND db_id=EXCLUDED.db_id)
                         = (SELECT conversation_id FROM chat_messages WHERE id=EXCLUDED.message_id AND db_id=EXCLUDED.db_id)
                 AND (chat_citations.message_id=EXCLUDED.message_id
                     OR ((SELECT role FROM chat_messages WHERE id=chat_citations.message_id)='user'
                         AND (SELECT role FROM chat_messages WHERE id=EXCLUDED.message_id)='assistant'))",
            )
            .bind(&citation.id)
            .bind(message_id)
            .bind(&citation.atom_id)
            .bind(citation.chunk_index)
            .bind(&citation.excerpt)
            .bind(citation.relevance_score)
            .bind(db_id)
            .bind(citation.citation_index)
            .execute(&mut *conn)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        if result.rows_affected() != 1 {
            return Err(AtomicCoreError::Validation(
                "Citation identity or publication target does not match".to_string(),
            ));
        }
    }
    Ok(())
}

/// Helper: fetch conversation tags for a single conversation.
async fn fetch_conversation_tags(
    pool: &sqlx::PgPool,
    conversation_id: &str,
    db_id: &str,
) -> StorageResult<Vec<Tag>> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>, String, bool, String)>(
        "SELECT t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM tags t
         JOIN conversation_tags ct ON ct.tag_id = t.id
         WHERE ct.conversation_id = $1 AND ct.db_id = $2 AND t.db_id = $2
         ORDER BY t.name",
    )
    .bind(conversation_id)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(
            |(id, name, parent_id, created_at, is_autotag_target, autotag_description)| Tag {
                id,
                name,
                parent_id,
                created_at,
                is_autotag_target,
                autotag_description,
            },
        )
        .collect())
}

/// Helper: fetch conversation summary (message_count, last_message_preview).
async fn fetch_conversation_summary(
    pool: &sqlx::PgPool,
    conversation_id: &str,
    db_id: &str,
) -> StorageResult<(i32, Option<String>)> {
    let message_count: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM chat_messages WHERE conversation_id = $1 AND db_id = $2 AND role != 'system'",
    )
    .bind(conversation_id)
    .bind(db_id)
    .fetch_one(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let message_count = message_count.unwrap_or(0);

    let last_message_preview: Option<String> = sqlx::query_scalar(
        "SELECT content FROM chat_messages
         WHERE conversation_id = $1 AND db_id = $2 AND role != 'system'
         ORDER BY message_index DESC
         LIMIT 1",
    )
    .bind(conversation_id)
    .bind(db_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
    .flatten();

    let preview = last_message_preview.map(|content| truncate_preview(&content, 100));

    Ok((message_count as i32, preview))
}

/// Helper: load a conversation row and build ConversationWithTags.
async fn load_conversation_with_tags(
    pool: &sqlx::PgPool,
    conversation_id: &str,
    db_id: &str,
) -> StorageResult<ConversationWithTags> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            i32,
            Option<String>,
        ),
    >(
        "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
         FROM conversations WHERE id = $1 AND db_id = $2",
    )
    .bind(conversation_id)
    .bind(db_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
    .ok_or_else(|| {
        AtomicCoreError::NotFound(format!("Conversation not found: {}", conversation_id))
    })?;

    let conversation = Conversation {
        id: row.0,
        title: row.1,
        user_id: row.2,
        soul_id: row.3,
        created_at: row.4,
        updated_at: row.5,
        is_archived: row.6 != 0,
        atomic_chat_state: row.7.map(|raw| serde_json::from_str(&raw)).transpose()?,
    };

    let tags = fetch_conversation_tags(pool, conversation_id, db_id).await?;
    let (message_count, last_message_preview) =
        fetch_conversation_summary(pool, conversation_id, db_id).await?;

    Ok(ConversationWithTags {
        conversation,
        tags,
        message_count,
        last_message_preview,
    })
}

/// Helper: batch fetch tags for multiple conversations.
async fn batch_fetch_conversation_tags(
    pool: &sqlx::PgPool,
    conv_ids: &[String],
    db_id: &str,
) -> StorageResult<HashMap<String, Vec<Tag>>> {
    if conv_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = sqlx::query_as::<_, (String, String, String, Option<String>, String, bool, String)>(
        "SELECT ct.conversation_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM conversation_tags ct
         JOIN tags t ON ct.tag_id = t.id
         WHERE ct.conversation_id = ANY($1) AND ct.db_id = $2 AND t.db_id = $2
         ORDER BY t.name",
    )
    .bind(conv_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut map: HashMap<String, Vec<Tag>> = HashMap::new();
    for (conv_id, id, name, parent_id, created_at, is_autotag_target, autotag_description) in rows {
        map.entry(conv_id).or_default().push(Tag {
            id,
            name,
            parent_id,
            created_at,
            is_autotag_target,
            autotag_description,
        });
    }

    Ok(map)
}

/// Helper: batch fetch summaries for multiple conversations.
async fn batch_fetch_conversation_summaries(
    pool: &sqlx::PgPool,
    conv_ids: &[String],
    db_id: &str,
) -> StorageResult<HashMap<String, (i32, Option<String>)>> {
    if conv_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut map: HashMap<String, (i32, Option<String>)> = HashMap::new();

    // Get counts
    let count_rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT conversation_id, COUNT(*)
         FROM chat_messages
         WHERE conversation_id = ANY($1) AND db_id = $2
         GROUP BY conversation_id",
    )
    .bind(conv_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    for (conv_id, count) in count_rows {
        map.insert(conv_id, (count as i32, None));
    }

    // Get last message previews
    let preview_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT conversation_id, content FROM (
            SELECT conversation_id, content,
                   ROW_NUMBER() OVER (PARTITION BY conversation_id ORDER BY message_index DESC) as rn
            FROM chat_messages
            WHERE conversation_id = ANY($1) AND db_id = $2
        ) sub WHERE rn = 1",
    )
    .bind(conv_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    for (conv_id, content) in preview_rows {
        let preview = truncate_preview(&content, 100);
        map.entry(conv_id).or_insert((0, None)).1 = Some(preview);
    }

    // Ensure all conv_ids have entries
    for id in conv_ids {
        map.entry(id.clone()).or_insert((0, None));
    }

    Ok(map)
}

#[async_trait]
impl ChatStore for PostgresStorage {
    async fn admit_chat_turn(
        &self,
        conversation_id: &str,
        user_id: &str,
        soul_id: &str,
        backend: &AtomicChatBackend,
        request: &AtomicChatRequest,
        context_window_tokens: usize,
    ) -> StorageResult<ChatMessage> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let current =
            owned_chat_state(&mut tx, &self.db_id, conversation_id, user_id, soul_id).await?;
        let last = last_chat_message(&mut tx, &self.db_id, conversation_id).await?;
        let outcome_unknown = if let AtomicChatRequest::Retry { user_message_id } = request {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM chat_tool_calls WHERE message_id=$1 AND db_id=$2 AND (status='running' OR NULLIF(tool_result,'')::jsonb->>'outcome_unknown'='true'))"
            ).bind(user_message_id).bind(&self.db_id).fetch_one(&mut *tx).await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        } else {
            false
        };
        let mut state = crate::chat::prepared_chat_state(
            current,
            backend,
            last.as_ref(),
            request,
            context_window_tokens,
            outcome_unknown,
        )?;
        let (saved, content) = crate::chat::chat_admission_input(&state, last.as_ref(), request);
        let message = match saved {
            Some(message) => message,
            None => write_message(&mut tx, &self.db_id, conversation_id, "user", content).await?,
        };
        crate::chat::admitted_chat_message(&mut state, request, &message.id);
        write_chat_state(&mut tx, &self.db_id, conversation_id, Some(&state)).await?;
        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(message)
    }

    async fn fail_chat_turn(
        &self,
        conversation_id: &str,
        user_message_id: &str,
        error: &str,
        uncertain: bool,
    ) -> StorageResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let mut state = locked_chat_state(&mut tx, &self.db_id, conversation_id)
            .await?
            .ok_or_else(|| AtomicCoreError::Conflict("No admitted chat turn".to_string()))?;
        crate::chat::failed_chat_state(&mut state, user_message_id, error, uncertain)?;
        write_chat_state(&mut tx, &self.db_id, conversation_id, Some(&state)).await?;
        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))
    }

    async fn set_chat_phase(
        &self,
        conversation_id: &str,
        user_id: &str,
        soul_id: &str,
        backend: &AtomicChatBackend,
        phase: &AtomicChatPhase,
    ) -> StorageResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let current =
            owned_chat_state(&mut tx, &self.db_id, conversation_id, user_id, soul_id).await?;
        let last = last_chat_message(&mut tx, &self.db_id, conversation_id).await?;
        let state = crate::chat::bound_chat_state(
            current,
            backend,
            last.as_ref().map(|last| last.role.as_str()),
        )?;
        let state = crate::chat::changed_chat_phase(state, phase)?;
        write_chat_state(&mut tx, &self.db_id, conversation_id, Some(&state)).await?;
        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))
    }

    async fn create_conversation(
        &self,
        tag_ids: &[String],
        title: Option<&str>,
        owner: Option<(&str, &str)>,
    ) -> StorageResult<ConversationWithTags> {
        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO conversations (id, title, user_id, soul_id, created_at, updated_at, is_archived, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, 0, $7)",
        )
        .bind(&id)
        .bind(title)
        .bind(owner.map(|(user_id, _)| user_id))
        .bind(owner.map(|(_, soul_id)| soul_id))
        .bind(&now)
        .bind(&now)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for tag_id in tag_ids {
            sqlx::query(
                "INSERT INTO conversation_tags (conversation_id, tag_id, db_id) VALUES ($1, $2, $3)",
            )
            .bind(&id)
            .bind(tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        let tags = fetch_conversation_tags(&self.pool, &id, &self.db_id).await?;

        Ok(ConversationWithTags {
            conversation: Conversation {
                id,
                title: title.map(String::from),
                user_id: owner.map(|(user_id, _)| user_id.to_string()),
                soul_id: owner.map(|(_, soul_id)| soul_id.to_string()),
                created_at: now.clone(),
                updated_at: now,
                is_archived: false,
                atomic_chat_state: None,
            },
            tags,
            message_count: 0,
            last_message_preview: None,
        })
    }

    async fn get_conversations(
        &self,
        filter_tag_id: Option<&str>,
        limit: i32,
        offset: i32,
        owner: Option<(&str, &str)>,
    ) -> StorageResult<Vec<ConversationWithTags>> {
        let user_id = owner.map(|(user_id, _)| user_id);
        let soul_id = owner.map(|(_, soul_id)| soul_id);
        let conversations: Vec<Conversation> = if let Some(tag_id) = filter_tag_id {
            let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>, String, String, i32, Option<String>)>(
                "SELECT DISTINCT c.id, c.title, c.user_id, c.soul_id, c.created_at, c.updated_at, c.is_archived, c.atomic_chat_state
                 FROM conversations c
                 JOIN conversation_tags ct ON ct.conversation_id = c.id
                 WHERE ct.tag_id = $1
                   AND ($2 IS NULL OR (c.user_id = $2 AND c.soul_id = $3))
                   AND c.is_archived = 0 AND c.db_id = $6 AND ct.db_id = $6
                 ORDER BY c.updated_at DESC
                 LIMIT $4 OFFSET $5",
            )
            .bind(tag_id)
            .bind(user_id)
            .bind(soul_id)
            .bind(limit)
            .bind(offset)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            rows.into_iter()
                .map(
                    |(
                        id,
                        title,
                        user_id,
                        soul_id,
                        created_at,
                        updated_at,
                        is_archived,
                        atomic_chat_state,
                    )| {
                        Ok(Conversation {
                            id,
                            title,
                            user_id,
                            soul_id,
                            created_at,
                            updated_at,
                            is_archived: is_archived != 0,
                            atomic_chat_state: atomic_chat_state
                                .map(|raw| serde_json::from_str(&raw))
                                .transpose()?,
                        })
                    },
                )
                .collect::<StorageResult<Vec<_>>>()?
        } else {
            let rows = sqlx::query_as::<
                _,
                (
                    String,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    String,
                    String,
                    i32,
                    Option<String>,
                ),
            >(
                "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
                 FROM conversations
                 WHERE ($1 IS NULL OR (user_id = $1 AND soul_id = $2))
                   AND is_archived = 0 AND db_id = $5
                 ORDER BY updated_at DESC
                 LIMIT $3 OFFSET $4",
            )
            .bind(user_id)
            .bind(soul_id)
            .bind(limit)
            .bind(offset)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            rows.into_iter()
                .map(
                    |(
                        id,
                        title,
                        user_id,
                        soul_id,
                        created_at,
                        updated_at,
                        is_archived,
                        atomic_chat_state,
                    )| {
                        Ok(Conversation {
                            id,
                            title,
                            user_id,
                            soul_id,
                            created_at,
                            updated_at,
                            is_archived: is_archived != 0,
                            atomic_chat_state: atomic_chat_state
                                .map(|raw| serde_json::from_str(&raw))
                                .transpose()?,
                        })
                    },
                )
                .collect::<StorageResult<Vec<_>>>()?
        };

        if conversations.is_empty() {
            return Ok(Vec::new());
        }

        let conv_ids: Vec<String> = conversations.iter().map(|c| c.id.clone()).collect();
        let tag_map = batch_fetch_conversation_tags(&self.pool, &conv_ids, &self.db_id).await?;
        let summary_map =
            batch_fetch_conversation_summaries(&self.pool, &conv_ids, &self.db_id).await?;

        let result = conversations
            .into_iter()
            .map(|conversation| {
                let tags = tag_map.get(&conversation.id).cloned().unwrap_or_default();
                let (message_count, last_message_preview) = summary_map
                    .get(&conversation.id)
                    .cloned()
                    .unwrap_or((0, None));
                ConversationWithTags {
                    conversation,
                    tags,
                    message_count,
                    last_message_preview,
                }
            })
            .collect();

        Ok(result)
    }

    async fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> StorageResult<Option<ConversationWithMessages>> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                String,
                i32,
                Option<String>,
            ),
        >(
            "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
             FROM conversations WHERE id = $1 AND db_id = $2",
        )
        .bind(conversation_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let conv = match row {
            Some((
                id,
                title,
                user_id,
                soul_id,
                created_at,
                updated_at,
                is_archived,
                atomic_chat_state,
            )) => Conversation {
                id,
                title,
                user_id,
                soul_id,
                created_at,
                updated_at,
                is_archived: is_archived != 0,
                atomic_chat_state: atomic_chat_state
                    .map(|raw| serde_json::from_str(&raw))
                    .transpose()?,
            },
            None => return Ok(None),
        };

        let tags = fetch_conversation_tags(&self.pool, &conv.id, &self.db_id).await?;

        // Fetch messages
        let msg_rows = sqlx::query_as::<_, (String, String, String, String, String, i32)>(
            "SELECT id, conversation_id, role, content, created_at, message_index
             FROM chat_messages
             WHERE conversation_id = $1 AND db_id = $2
             ORDER BY message_index",
        )
        .bind(&conv.id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let messages: Vec<ChatMessage> = msg_rows
            .into_iter()
            .map(
                |(id, conversation_id, role, content, created_at, message_index)| ChatMessage {
                    id,
                    conversation_id,
                    role,
                    content,
                    created_at,
                    message_index,
                },
            )
            .collect();

        if messages.is_empty() {
            return Ok(Some(ConversationWithMessages {
                conversation: conv,
                tags,
                messages: Vec::new(),
            }));
        }

        let msg_ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();

        // Batch fetch tool calls
        let tc_rows = sqlx::query_as::<_, (String, String, String, String, Option<String>, String, String, Option<String>)>(
            "SELECT id, message_id, tool_name, tool_input, tool_result, status, created_at, completed_at
             FROM chat_tool_calls
             WHERE message_id = ANY($1) AND db_id = $2
             ORDER BY created_at",
        )
        .bind(&msg_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut tool_calls_map: HashMap<String, Vec<ChatToolCall>> = HashMap::new();
        for (
            id,
            message_id,
            tool_name,
            tool_input,
            tool_output,
            status,
            created_at,
            completed_at,
        ) in tc_rows
        {
            let tc = ChatToolCall {
                id,
                message_id: message_id.clone(),
                tool_name,
                tool_input: serde_json::from_str(&tool_input).unwrap_or(serde_json::Value::Null),
                tool_output: tool_output
                    .filter(|value| !value.is_empty())
                    .map(|s| serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)),
                status,
                created_at,
                completed_at,
            };
            tool_calls_map.entry(message_id).or_default().push(tc);
        }

        // Batch fetch citations
        let cit_rows = sqlx::query_as::<
            _,
            (
                String,
                String,
                i32,
                String,
                Option<i32>,
                String,
                Option<f32>,
            ),
        >(
            "SELECT id, message_id, citation_index, atom_id, chunk_index, excerpt, relevance_score
             FROM chat_citations
             WHERE message_id = ANY($1) AND db_id = $2 ORDER BY citation_index",
        )
        .bind(&msg_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut citations_map: HashMap<String, Vec<ChatCitation>> = HashMap::new();
        for (id, message_id, citation_index, atom_id, chunk_index, excerpt, relevance_score) in
            cit_rows
        {
            let cit = ChatCitation {
                id,
                message_id: message_id.clone(),
                citation_index,
                atom_id,
                chunk_index,
                excerpt,
                relevance_score,
            };
            citations_map.entry(message_id).or_default().push(cit);
        }

        let messages_with_context = messages
            .into_iter()
            .map(|message| {
                let tool_calls = tool_calls_map.get(&message.id).cloned().unwrap_or_default();
                let citations = citations_map.get(&message.id).cloned().unwrap_or_default();
                ChatMessageWithContext {
                    message,
                    tool_calls,
                    citations,
                }
            })
            .collect();

        Ok(Some(ConversationWithMessages {
            conversation: conv,
            tags,
            messages: messages_with_context,
        }))
    }

    async fn update_conversation(
        &self,
        id: &str,
        title: Option<&str>,
        is_archived: Option<bool>,
    ) -> StorageResult<Conversation> {
        let now = chrono::Utc::now().to_rfc3339();

        if let Some(t) = title {
            sqlx::query(
                "UPDATE conversations SET title = $1, updated_at = $2 WHERE id = $3 AND db_id = $4",
            )
            .bind(t)
            .bind(&now)
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        if let Some(archived) = is_archived {
            sqlx::query(
                "UPDATE conversations SET is_archived = $1, updated_at = $2 WHERE id = $3 AND db_id = $4",
            )
            .bind(if archived { 1i32 } else { 0i32 })
            .bind(&now)
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        let row = sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                String,
                i32,
                Option<String>,
            ),
        >(
            "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
             FROM conversations WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        .ok_or_else(|| AtomicCoreError::NotFound(format!("Conversation not found: {}", id)))?;

        Ok(Conversation {
            id: row.0,
            title: row.1,
            user_id: row.2,
            soul_id: row.3,
            created_at: row.4,
            updated_at: row.5,
            is_archived: row.6 != 0,
            atomic_chat_state: row.7.map(|raw| serde_json::from_str(&raw)).transpose()?,
        })
    }

    async fn delete_conversation(&self, id: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM conversations WHERE id = $1 AND db_id = $2")
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    async fn set_conversation_scope(
        &self,
        conversation_id: &str,
        tag_ids: &[String],
    ) -> StorageResult<ConversationWithTags> {
        let now = chrono::Utc::now().to_rfc3339();

        sqlx::query("DELETE FROM conversation_tags WHERE conversation_id = $1 AND db_id = $2")
            .bind(conversation_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for tag_id in tag_ids {
            sqlx::query(
                "INSERT INTO conversation_tags (conversation_id, tag_id, db_id) VALUES ($1, $2, $3)",
            )
            .bind(conversation_id)
            .bind(tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        sqlx::query("UPDATE conversations SET updated_at = $1 WHERE id = $2 AND db_id = $3")
            .bind(&now)
            .bind(conversation_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        load_conversation_with_tags(&self.pool, conversation_id, &self.db_id).await
    }

    async fn add_tag_to_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> StorageResult<ConversationWithTags> {
        let now = chrono::Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO conversation_tags (conversation_id, tag_id, db_id) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(conversation_id)
        .bind(tag_id)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query("UPDATE conversations SET updated_at = $1 WHERE id = $2 AND db_id = $3")
            .bind(&now)
            .bind(conversation_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        load_conversation_with_tags(&self.pool, conversation_id, &self.db_id).await
    }

    async fn remove_tag_from_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> StorageResult<ConversationWithTags> {
        let now = chrono::Utc::now().to_rfc3339();

        sqlx::query(
            "DELETE FROM conversation_tags WHERE conversation_id = $1 AND tag_id = $2 AND db_id = $3",
        )
        .bind(conversation_id)
        .bind(tag_id)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query("UPDATE conversations SET updated_at = $1 WHERE id = $2 AND db_id = $3")
            .bind(&now)
            .bind(conversation_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        load_conversation_with_tags(&self.pool, conversation_id, &self.db_id).await
    }

    async fn save_message(
        &self,
        conversation_id: &str,
        role: &str,
        content: &str,
    ) -> StorageResult<ChatMessage> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        write_message(&mut conn, &self.db_id, conversation_id, role, content).await
    }

    async fn save_chat_reply(
        &self,
        conversation_id: &str,
        expected_user_message_id: Option<&str>,
        content: &str,
        tool_calls: &[ChatToolCall],
        citations: &[ChatCitation],
        citation_index: i32,
    ) -> StorageResult<ChatMessage> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let current = locked_chat_state(&mut tx, &self.db_id, conversation_id).await?;
        let message =
            write_message(&mut tx, &self.db_id, conversation_id, "assistant", content).await?;
        let state =
            crate::chat::completed_chat_state(current, expected_user_message_id, &message.id)?;
        write_chat_tool_state(
            &mut tx,
            &self.db_id,
            conversation_id,
            &message.id,
            tool_calls,
            citations,
            citation_index,
        )
        .await?;
        if state.is_some() {
            write_chat_state(&mut tx, &self.db_id, conversation_id, state.as_ref()).await?;
        }
        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(message)
    }

    async fn get_chat_citation_index(&self, conversation_id: &str) -> StorageResult<i32> {
        sqlx::query_scalar(
            "SELECT atomic_citation_index FROM conversations WHERE id=$1 AND db_id=$2",
        )
        .bind(conversation_id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))
    }

    async fn save_chat_tool_state(
        &self,
        conversation_id: &str,
        message_id: &str,
        tool_calls: &[ChatToolCall],
        citations: &[ChatCitation],
        citation_index: i32,
    ) -> StorageResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        write_chat_tool_state(
            &mut tx,
            &self.db_id,
            conversation_id,
            message_id,
            tool_calls,
            citations,
            citation_index,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))
    }

    async fn get_scope_tag_ids(&self, conversation_id: &str) -> StorageResult<Vec<String>> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT tag_id FROM conversation_tags WHERE conversation_id = $1 AND db_id = $2",
        )
        .bind(conversation_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows)
    }

    async fn get_scope_description(&self, tag_ids: &[String]) -> StorageResult<String> {
        if tag_ids.is_empty() {
            return Ok("You have access to ALL atoms in the knowledge base.".to_string());
        }

        let names = sqlx::query_scalar::<_, String>(
            "SELECT name FROM tags WHERE id = ANY($1) AND db_id = $2",
        )
        .bind(tag_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if names.is_empty() {
            Ok("You have access to a scoped set of atoms.".to_string())
        } else {
            Ok(format!(
                "You have access to atoms tagged with: {}. Focus your search on these topics.",
                names.join(", ")
            ))
        }
    }
}
