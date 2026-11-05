//! Chat conversation CRUD operations
//!
//! Provides all conversation, message, tool call, and citation management
//! for the chat system. Used by both Tauri and standalone server.

use crate::error::AtomicCoreError;
use crate::models::{
    AtomicChatBackend, AtomicChatPhase, AtomicChatRecap, AtomicChatRecapOutcome, AtomicChatRequest,
    AtomicChatState, AtomicChatTurn, AtomicChatTurnStatus, AtomicChatNativeOutcome, ChatCitation, ChatMessage,
    ChatMessageWithContext, ChatToolCall, Conversation, ConversationWithMessages,
    ConversationWithTags, Tag,
};
use rusqlite::{Connection, OptionalExtension};

// ==================== Helper Functions ====================

fn read_chat_state(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<AtomicChatState>> {
    let raw: Option<String> = row.get(column)?;
    raw.map(|text| {
        serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })
    })
    .transpose()
}

/// Truncate a string to at most `max_chars` characters, appending `...` if truncated.
///
/// Operates on character boundaries (not bytes) so it is safe for any UTF-8 input,
/// including multi-byte scripts like Cyrillic, CJK, or emoji.
pub(crate) fn truncate_preview(s: &str, max_chars: usize) -> String {
    let mut iter = s.chars();
    let head: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{}...", head)
    } else {
        head
    }
}

/// Get tags for a conversation
pub fn get_conversation_tags(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<Tag>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM tags t
         JOIN conversation_tags ct ON ct.tag_id = t.id
         WHERE ct.conversation_id = ?1
         ORDER BY t.name",
    )?;

    let tags = stmt
        .query_map([conversation_id], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
                parent_id: row.get(2)?,
                created_at: row.get(3)?,
                is_autotag_target: row.get::<_, i32>(4)? != 0,
                autotag_description: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(tags)
}

/// Get message count and last message preview for a conversation
pub fn get_conversation_summary(
    conn: &Connection,
    conversation_id: &str,
) -> Result<(i32, Option<String>), AtomicCoreError> {
    let message_count: i32 = conn.query_row(
        "SELECT COUNT(*) FROM chat_messages WHERE conversation_id = ?1 AND role != 'system'",
        [conversation_id],
        |row| row.get(0),
    )?;

    let last_message_preview: Option<String> = conn
        .query_row(
            "SELECT content FROM chat_messages
             WHERE conversation_id = ?1 AND role != 'system'
             ORDER BY message_index DESC
             LIMIT 1",
            [conversation_id],
            |row| {
                let content: String = row.get(0)?;
                Ok(truncate_preview(&content, 100))
            },
        )
        .ok();

    Ok((message_count, last_message_preview))
}

/// Get tool calls for a message
pub fn get_message_tool_calls(
    conn: &Connection,
    message_id: &str,
) -> Result<Vec<ChatToolCall>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id, tool_name, tool_input, tool_output, status, created_at, completed_at
         FROM chat_tool_calls
         WHERE message_id = ?1
         ORDER BY created_at",
    )?;

    let tool_calls = stmt
        .query_map([message_id], |row| {
            let tool_input_str: String = row.get(3)?;
            let tool_output_str: Option<String> = row.get(4)?;

            Ok(ChatToolCall {
                id: row.get(0)?,
                message_id: row.get(1)?,
                tool_name: row.get(2)?,
                tool_input: serde_json::from_str(&tool_input_str)
                    .unwrap_or(serde_json::Value::Null),
                tool_output: tool_output_str
                    .map(|s| serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)),
                status: row.get(5)?,
                created_at: row.get(6)?,
                completed_at: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(tool_calls)
}

/// Get citations for a message
pub fn get_message_citations(
    conn: &Connection,
    message_id: &str,
) -> Result<Vec<ChatCitation>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id, citation_index, atom_id, chunk_index, excerpt, relevance_score
         FROM chat_citations
         WHERE message_id = ?1
         ORDER BY citation_index",
    )?;

    let citations = stmt
        .query_map([message_id], |row| {
            Ok(ChatCitation {
                id: row.get(0)?,
                message_id: row.get(1)?,
                citation_index: row.get(2)?,
                atom_id: row.get(3)?,
                chunk_index: row.get(4)?,
                excerpt: row.get(5)?,
                relevance_score: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(citations)
}

/// Get messages with context for a conversation
pub fn get_messages_with_context(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<ChatMessageWithContext>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, role, content, created_at, message_index
         FROM chat_messages
         WHERE conversation_id = ?1
         ORDER BY message_index",
    )?;

    let messages: Vec<ChatMessage> = stmt
        .query_map([conversation_id], |row| {
            Ok(ChatMessage {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
                message_index: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    if messages.is_empty() {
        return Ok(Vec::new());
    }

    // Batch fetch tool_calls and citations for all messages (avoids N+1)
    let msg_ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
    let tool_calls_map = batch_fetch_tool_calls(conn, &msg_ids)?;
    let citations_map = batch_fetch_citations(conn, &msg_ids)?;

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

    Ok(messages_with_context)
}

/// Batch fetch tool calls for multiple messages in a single query
fn batch_fetch_tool_calls(
    conn: &Connection,
    message_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<ChatToolCall>>, AtomicCoreError> {
    if message_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = message_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT id, message_id, tool_name, tool_input, tool_output, status, created_at, completed_at
         FROM chat_tool_calls
         WHERE message_id IN ({})
         ORDER BY created_at",
        placeholders
    );
    let mut stmt = conn.prepare(&query)?;
    let mut map: std::collections::HashMap<String, Vec<ChatToolCall>> =
        std::collections::HashMap::new();
    let rows = stmt.query_map(rusqlite::params_from_iter(message_ids.iter()), |row| {
        let tool_input_str: String = row.get(3)?;
        let tool_output_str: Option<String> = row.get(4)?;
        Ok(ChatToolCall {
            id: row.get(0)?,
            message_id: row.get(1)?,
            tool_name: row.get(2)?,
            tool_input: serde_json::from_str(&tool_input_str).unwrap_or(serde_json::Value::Null),
            tool_output: tool_output_str
                .map(|s| serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)),
            status: row.get(5)?,
            created_at: row.get(6)?,
            completed_at: row.get(7)?,
        })
    })?;
    for row in rows {
        let tc = row?;
        map.entry(tc.message_id.clone()).or_default().push(tc);
    }
    Ok(map)
}

/// Batch fetch citations for multiple messages in a single query
fn batch_fetch_citations(
    conn: &Connection,
    message_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<ChatCitation>>, AtomicCoreError> {
    if message_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = message_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT id, message_id, citation_index, atom_id, chunk_index, excerpt, relevance_score
         FROM chat_citations
         WHERE message_id IN ({})
         ORDER BY citation_index",
        placeholders
    );
    let mut stmt = conn.prepare(&query)?;
    let mut map: std::collections::HashMap<String, Vec<ChatCitation>> =
        std::collections::HashMap::new();
    let rows = stmt.query_map(rusqlite::params_from_iter(message_ids.iter()), |row| {
        Ok(ChatCitation {
            id: row.get(0)?,
            message_id: row.get(1)?,
            citation_index: row.get(2)?,
            atom_id: row.get(3)?,
            chunk_index: row.get(4)?,
            excerpt: row.get(5)?,
            relevance_score: row.get(6)?,
        })
    })?;
    for row in rows {
        let c = row?;
        map.entry(c.message_id.clone()).or_default().push(c);
    }
    Ok(map)
}

/// Batch fetch tags for multiple conversations in a single query
fn batch_fetch_conversation_tags(
    conn: &Connection,
    conv_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<Tag>>, AtomicCoreError> {
    if conv_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = conv_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT ct.conversation_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM conversation_tags ct
         JOIN tags t ON ct.tag_id = t.id
         WHERE ct.conversation_id IN ({})
         ORDER BY t.name",
        placeholders
    );
    let mut stmt = conn.prepare(&query)?;
    let mut map: std::collections::HashMap<String, Vec<Tag>> = std::collections::HashMap::new();
    let rows = stmt.query_map(rusqlite::params_from_iter(conv_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            Tag {
                id: row.get(1)?,
                name: row.get(2)?,
                parent_id: row.get(3)?,
                created_at: row.get(4)?,
                is_autotag_target: row.get::<_, i32>(5)? != 0,
                autotag_description: row.get(6)?,
            },
        ))
    })?;
    for row in rows {
        let (conv_id, tag) = row?;
        map.entry(conv_id).or_default().push(tag);
    }
    Ok(map)
}

/// Batch fetch message counts and last message previews for multiple conversations
fn batch_fetch_conversation_summaries(
    conn: &Connection,
    conv_ids: &[String],
) -> Result<std::collections::HashMap<String, (i32, Option<String>)>, AtomicCoreError> {
    if conv_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = conv_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");

    // Get counts
    let count_query = format!(
        "SELECT conversation_id, COUNT(*) FROM chat_messages WHERE conversation_id IN ({}) AND role != 'system' GROUP BY conversation_id",
        placeholders
    );
    let mut count_stmt = conn.prepare(&count_query)?;
    let mut map: std::collections::HashMap<String, (i32, Option<String>)> =
        std::collections::HashMap::new();
    let count_rows = count_stmt.query_map(rusqlite::params_from_iter(conv_ids.iter()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
    })?;
    for row in count_rows {
        let (conv_id, count) = row?;
        map.insert(conv_id, (count, None));
    }

    // Get last message previews using window function
    let preview_query = format!(
            "SELECT conversation_id, content FROM (
                SELECT conversation_id, content,
                       ROW_NUMBER() OVER (PARTITION BY conversation_id ORDER BY message_index DESC) as rn
                FROM chat_messages
                WHERE conversation_id IN ({}) AND role != 'system'
            ) WHERE rn = 1",
        placeholders
    );
    let mut preview_stmt = conn.prepare(&preview_query)?;
    let preview_rows = preview_stmt
        .query_map(rusqlite::params_from_iter(conv_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
    for row in preview_rows {
        let (conv_id, content) = row?;
        let preview = truncate_preview(&content, 100);
        map.entry(conv_id).or_insert((0, None)).1 = Some(preview);
    }

    // Ensure all conv_ids have entries (even those with 0 messages)
    for id in conv_ids {
        map.entry(id.clone()).or_insert((0, None));
    }

    Ok(map)
}

pub(crate) fn bound_chat_state(
    current: Option<AtomicChatState>,
    backend: &AtomicChatBackend,
    last_role: Option<&str>,
) -> Result<AtomicChatState, AtomicCoreError> {
    // Before native hookup, existing unbound dialogue can only be API history.
    // Never use that legacy state to initialize a replacement Claude session.
    let state = match current {
        Some(state) => state,
        None if last_role.is_some() && *backend != AtomicChatBackend::Api => {
            return Err(AtomicCoreError::Conflict(
                "This sitting has API history. Restore claude_code=false to continue/end it, or start a new sitting.".to_string(),
            ));
        }
        None => AtomicChatState {
            backend: *backend,
            sitting_wide_citations: last_role.is_none(),
            phase: AtomicChatPhase::Open,
            active: None,
            recap: None,
            context_window_tokens: None,
            native_usage: None,
            native_context_tokens: None,
            maintenance: None,
        },
    };
    if state.backend != *backend {
        let required = match state.backend {
            AtomicChatBackend::Api => "false",
            AtomicChatBackend::ClaudeCode => "true",
        };
        return Err(AtomicCoreError::Conflict(format!(
            "This sitting requires claude_code={required}. Restore its original claude_code setting to continue/end it, or start a new sitting.",
        )));
    }
    Ok(state)
}

pub(crate) fn admitted_chat_state(
    current: Option<AtomicChatState>,
    backend: &AtomicChatBackend,
    last_role: Option<&str>,
) -> Result<AtomicChatState, AtomicCoreError> {
    let state = bound_chat_state(current, backend, last_role)?;
    let refusal = match (state.phase, state.active.as_ref().map(|turn| turn.status)) {
        (AtomicChatPhase::Closed, _) => {
            Some("This sitting is closed and read-only. Start a new sitting.")
        }
        (AtomicChatPhase::Closing, _) => {
            Some("This sitting is closing. Wait for close-out or retry the failed close-out.")
        }
        (_, Some(AtomicChatTurnStatus::Running)) => {
            Some("This sitting has an unfinished turn. Wait for its outcome; do not send it again.")
        }
        (_, Some(AtomicChatTurnStatus::Failed)) => {
            Some("The previous turn failed. Retry that turn or end the sitting.")
        }
        (_, Some(AtomicChatTurnStatus::Uncertain)) => Some(
            "The previous turn's outcome is unknown. Do not replay it; resolve it or close out from saved history.",
        ),
        (_, None) if last_role == Some("user") => {
            Some("This sitting has an unanswered user turn. Retry that turn or end the sitting.")
        }
        _ => None,
    };
    if let Some(message) = refusal {
        return Err(AtomicCoreError::Conflict(message.to_string()));
    }
    Ok(state)
}

pub(crate) fn matching_chat_turn(
    state: &AtomicChatState,
    expected_user_message_id: &str,
    native: Option<&AtomicChatNativeOutcome>,
) -> Result<(), AtomicCoreError> {
    // Delivery failure may precede native completion. Recovery resolves only
    // that persisted attempt; API/standalone still require a Running turn.
    let matching_native = |attempt: Option<&str>| native.is_some_and(|native| {
        state.backend == AtomicChatBackend::ClaudeCode
            && state.phase != AtomicChatPhase::Closed
            && attempt == Some(native.attempt_id.as_str())
    });
    let matching = if let Some(recap) = state
        .recap
        .as_ref()
        .filter(|recap| recap.request_message_id == expected_user_message_id)
    {
        match recap.outcome {
            AtomicChatRecapOutcome::Complete { .. } => false,
            _ if native.is_some() => matching_native(recap.native_attempt_id.as_deref()),
            _ => matches!(recap.outcome, AtomicChatRecapOutcome::Running),
        }
    } else {
        state.active.as_ref().is_some_and(|turn| {
            turn.user_message_id == expected_user_message_id
                && (native.is_none() || state.recap.is_none())
                && if native.is_some() { matching_native(turn.native_attempt_id.as_deref()) }
                    else { turn.status == AtomicChatTurnStatus::Running }
        })
    };
    matching.then_some(()).ok_or_else(|| {
        AtomicCoreError::Conflict("The active chat turn no longer matches this result".to_string())
    })
}

pub(crate) fn failed_chat_state(
    state: &mut AtomicChatState,
    message_id: &str,
    error: &str,
    uncertain: bool,
    native_outcome: Option<&AtomicChatNativeOutcome>,
) -> Result<(), AtomicCoreError> {
    matching_chat_turn(state, message_id, native_outcome)?;
    apply_native_usage(state, message_id, native_outcome)?;
    if let Some(recap) = state
        .recap
        .as_mut()
        .filter(|recap| recap.request_message_id == message_id)
    {
        recap.outcome = if uncertain {
            AtomicChatRecapOutcome::Uncertain {
                error: error.to_string(),
            }
        } else {
            AtomicChatRecapOutcome::Failed {
                error: error.to_string(),
            }
        };
    } else if let Some(turn) = &mut state.active {
        if state.backend == AtomicChatBackend::ClaudeCode && turn.native_input_recorded != Some(true)
        {
            turn.native_input_recorded = native_outcome
                .and_then(|outcome| outcome.input_recorded)
                .filter(|recorded| *recorded || !uncertain);
        }
        turn.status = if uncertain {
            AtomicChatTurnStatus::Uncertain
        } else {
            AtomicChatTurnStatus::Failed
        };
        turn.error = Some(error.to_string());
    }
    Ok(())
}

pub(crate) fn completed_chat_state(
    state: Option<AtomicChatState>,
    expected_user_message_id: Option<&str>,
    reply_message_id: &str,
    native_outcome: Option<&AtomicChatNativeOutcome>,
    tool_calls: &[ChatToolCall],
) -> Result<Option<AtomicChatState>, AtomicCoreError> {
    match (state, expected_user_message_id) {
        (None, None) if native_outcome.is_none() => Ok(None), // Standalone.
        (Some(mut state), Some(expected)) => {
            matching_chat_turn(&state, expected, native_outcome)?;
            apply_native_usage(&mut state, expected, native_outcome)?;
            if let Some(recap) = state
                .recap
                .as_mut()
                .filter(|recap| recap.request_message_id == expected)
            {
                recap.outcome = AtomicChatRecapOutcome::Complete {
                    reply_message_id: reply_message_id.to_string(),
                };
            } else {
                state.active = None;
                if tool_calls.iter().any(ChatToolCall::outcome_unknown) {
                    state.phase = AtomicChatPhase::Closing;
                }
            }
            Ok(Some(state)) // Preserve Closing: publication must not reopen the sitting.
        }
        _ => Err(AtomicCoreError::Conflict(
            "Reply has no matching admitted chat turn".to_string(),
        )),
    }
}

fn apply_native_usage(
    state: &mut AtomicChatState,
    turn_id: &str,
    outcome: Option<&AtomicChatNativeOutcome>,
) -> Result<(), AtomicCoreError> {
    if let Some(usage) = outcome.and_then(|outcome| outcome.usage.as_ref()) {
        if state.backend != AtomicChatBackend::ClaudeCode
            || usage.turn_id != turn_id || usage.message_id.is_empty()
        {
            return Err(AtomicCoreError::Conflict("Native usage does not match this Claude turn".into()));
        }
        if let (Some(maintenance), Some(outcome), Some(capacity)) = (
            state.maintenance.as_mut(), outcome, state.context_window_tokens,
        ) {
            if maintenance.awaiting_measurement && outcome.attempt_id != maintenance.native_attempt_id {
                if let Some(tokens) = outcome.context_tokens {
                    maintenance.ordinary_disabled |= tokens > capacity.saturating_mul(60) / 100;
                    maintenance.awaiting_measurement = false;
                }
            }
        }
        state.native_usage = Some(usage.clone());
    }
    if let Some(outcome) = outcome {
        // Unknown coverage invalidates the estimate, not the last measured usage.
        state.native_context_tokens = outcome.context_tokens;
    }
    Ok(())
}

pub(crate) fn changed_chat_phase(
    mut state: AtomicChatState,
    phase: &AtomicChatPhase,
) -> Result<AtomicChatState, AtomicCoreError> {
    if state.phase == *phase {
        return Ok(state);
    }
    if state.maintenance.as_ref().is_some_and(|maintenance| maintenance.unresolved()) {
        return Err(AtomicCoreError::Conflict("Reconcile native maintenance before changing the sitting phase".into()));
    }
    match (state.phase, *phase) {
        (AtomicChatPhase::Open, AtomicChatPhase::Closing) => {}
        (AtomicChatPhase::Closing, AtomicChatPhase::Closed)
            if !state
                .active
                .as_ref()
                .is_some_and(|turn| turn.status == AtomicChatTurnStatus::Running)
                && state.recap.as_ref().is_none_or(|recap| {
                    matches!(recap.outcome, AtomicChatRecapOutcome::Complete { .. })
                }) => {}
        _ => {
            return Err(AtomicCoreError::Conflict(
                "Chat cannot close with running work or an unfinished recap, or reopen after close-out"
                    .to_string(),
            ));
        }
    }
    state.phase = *phase;
    Ok(state)
}

/// Only legacy, unidentified controls use wording; managed controls use saved IDs.
pub fn is_atomic_recap_prompt(content: &str) -> bool {
    let content = content.trim();
    content
        .lines()
        .next()
        .unwrap_or("")
        .starts_with("This Atomic session is ending. Write a recap of your activity")
        && content.lines().any(|line| line == "Full Atomic transcript:")
}

/// Saved controls are not dialogue. Wording is only for old nested-transcript rows.
pub fn dialogue_messages<'a>(
    conversation: &'a ConversationWithMessages,
    extra_control_id: Option<&str>,
) -> Vec<&'a crate::ChatMessageWithContext> {
    let state = conversation.conversation.atomic_chat_state.as_ref();
    let recap = state.and_then(|state| state.recap.as_ref());
    let reply_id = recap.and_then(|recap| match &recap.outcome {
        AtomicChatRecapOutcome::Complete { reply_message_id } => Some(reply_message_id.as_str()),
        _ => None,
    });
    let ordinary_id = state
        .and_then(|state| state.active.as_ref())
        .map(|turn| turn.user_message_id.as_str());
    let mut skip_reply = false;
    conversation
        .messages
        .iter()
        .filter(|row| {
            let id = row.message.id.as_str();
            let control = extra_control_id == Some(id)
                || recap.is_some_and(|recap| recap.request_message_id == id)
                || (ordinary_id != Some(id)
                    && row.message.role == "user"
                    && is_atomic_recap_prompt(&row.message.content));
            let omitted =
                control || reply_id == Some(id) || (skip_reply && row.message.role == "assistant");
            skip_reply = control;
            !omitted && matches!(row.message.role.as_str(), "system" | "user" | "assistant")
        })
        .collect()
}

pub(crate) fn prepared_chat_state(
    current: Option<AtomicChatState>,
    backend: &AtomicChatBackend,
    last: Option<&ChatMessage>,
    request: &AtomicChatRequest,
    context_window_tokens: usize,
    outcome_unknown: bool,
) -> Result<AtomicChatState, AtomicCoreError> {
    if context_window_tokens <= 20_000 {
        return Err(AtomicCoreError::Validation(
            "Usable chat context must exceed the 20,000-token reserve".to_string(),
        ));
    }
    let role = last.map(|message| message.role.as_str());
    let mut state = bound_chat_state(current, backend, role)?;
    let conflict = |message: &str| AtomicCoreError::Conflict(message.to_string());
    if state.maintenance.as_ref().is_some_and(|maintenance| maintenance.unresolved()
        || (maintenance.status == crate::AtomicChatMaintenanceStatus::Blocked
            && !matches!(request, AtomicChatRequest::Recap { .. })))
    {
        return Err(conflict("Native maintenance is unresolved. Reload before continuing; blocked history can only close out."));
    }
    match request {
        AtomicChatRequest::New { .. } => {
            state = admitted_chat_state(Some(state), backend, role)?;
        }
        AtomicChatRequest::Retry { user_message_id } => {
            let last = last
                .filter(|last| last.role == "user" && last.id == *user_message_id)
                .ok_or_else(|| conflict("Retry must match the latest unanswered user turn"))?;
            if state.phase != AtomicChatPhase::Open {
                return Err(conflict(
                    "This sitting is closing or closed. Retry close-out instead.",
                ));
            }
            if let Some(active) = &state.active {
                if active.status == AtomicChatTurnStatus::Uncertain {
                    return Err(conflict(
                        "The previous outcome is unknown. Close out from saved history; do not replay it.",
                    ));
                }
                if active.user_message_id != *user_message_id
                    || active.status != AtomicChatTurnStatus::Failed
                {
                    return Err(conflict(
                        "Retry requires the matching failed turn after writer-idle reconciliation",
                    ));
                }
            } else if is_atomic_recap_prompt(&last.content) {
                return Err(conflict(
                    "This is a legacy recap. Retry tool-free close-out, not ordinary chat.",
                ));
            }
            if outcome_unknown {
                return Err(conflict(
                    "A tool outcome is unknown. Close out from saved history; do not replay it.",
                ));
            }
        }
        AtomicChatRequest::Recap { .. } => {
            if state.phase != AtomicChatPhase::Closing
                || state
                    .active
                    .as_ref()
                    .is_some_and(|turn| turn.status == AtomicChatTurnStatus::Running)
            {
                return Err(conflict(
                    "Recap requires Closing with no running ordinary turn",
                ));
            }
            if let Some(recap) = &state.recap {
                if matches!(recap.outcome, AtomicChatRecapOutcome::Complete { .. }) {
                    return Err(conflict(
                        "Recap already saved. Reuse its reply for publication.",
                    ));
                }
                // The lock proves an API worker idle, not a separate native writer.
                let retryable = matches!(recap.outcome, AtomicChatRecapOutcome::Failed { .. })
                    || (*backend == AtomicChatBackend::Api
                        && matches!(recap.outcome, AtomicChatRecapOutcome::Uncertain { .. }));
                if !retryable || last.is_none_or(|last| last.id != recap.request_message_id) {
                    return Err(conflict(
                        "Recap Retry requires its matching failed control after writer-idle reconciliation",
                    ));
                }
            }
        }
    }
    // A failed recap with a smaller profile must not erase the ordinary-turn
    // capacity basis and make cleanup eligible on the next End attempt.
    if *backend != AtomicChatBackend::ClaudeCode || !matches!(request, AtomicChatRequest::Recap { .. }) {
        state.context_window_tokens = Some(context_window_tokens);
    }
    Ok(state)
}

pub(crate) fn admitted_chat_message(
    state: &mut AtomicChatState,
    request: &AtomicChatRequest,
    message_id: &str,
) {
    let native_attempt_id = (state.backend == AtomicChatBackend::ClaudeCode)
        .then(|| uuid::Uuid::new_v4().to_string());
    match request {
        AtomicChatRequest::New {
            page_context,
            canvas_context,
            ..
        } => {
            state.active = Some(AtomicChatTurn {
                user_message_id: message_id.to_string(),
                native_attempt_id,
                native_input_recorded: None,
                status: AtomicChatTurnStatus::Running,
                error: None,
                page_context: page_context.clone(),
                canvas_context: canvas_context.clone(),
            });
        }
        AtomicChatRequest::Retry { .. } => {
            let turn = state.active.get_or_insert(AtomicChatTurn {
                user_message_id: message_id.to_string(),
                native_attempt_id: None,
                native_input_recorded: None,
                status: AtomicChatTurnStatus::Running,
                error: None,
                page_context: None,
                canvas_context: None,
            });
            turn.status = AtomicChatTurnStatus::Running;
            turn.error = None;
            turn.native_attempt_id = native_attempt_id;
        }
        AtomicChatRequest::Recap { .. } => {
            state.recap = Some(AtomicChatRecap {
                request_message_id: message_id.to_string(),
                native_attempt_id,
                outcome: AtomicChatRecapOutcome::Running,
            });
        }
    }
}

pub(crate) fn chat_admission_input<'a>(
    state: &AtomicChatState,
    last: Option<&ChatMessage>,
    request: &'a AtomicChatRequest,
) -> (Option<ChatMessage>, &'a str) {
    match request {
        AtomicChatRequest::New { content, .. } => (None, content),
        AtomicChatRequest::Retry { .. } => (last.cloned(), ""),
        AtomicChatRequest::Recap { instruction } => {
            let legacy = state.active.is_none()
                && last.is_some_and(|last| {
                    last.role == "user" && is_atomic_recap_prompt(&last.content)
                });
            (
                last.filter(|_| state.recap.is_some() || legacy).cloned(),
                instruction,
            )
        }
    }
}

fn stored_chat_state(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Option<AtomicChatState>, AtomicCoreError> {
    Ok(conn.query_row(
        "SELECT atomic_chat_state FROM conversations WHERE id=?1",
        [conversation_id],
        |row| read_chat_state(row, 0),
    )?)
}

fn write_chat_state(
    conn: &Connection,
    conversation_id: &str,
    state: Option<&AtomicChatState>,
) -> Result<(), AtomicCoreError> {
    let raw = state.map(serde_json::to_string).transpose()?;
    conn.execute(
        "UPDATE conversations SET atomic_chat_state=?1 WHERE id=?2",
        rusqlite::params![raw, conversation_id],
    )?;
    Ok(())
}

fn owned_chat_state(
    conn: &Connection,
    conversation_id: &str,
    user_id: &str,
    soul_id: &str,
) -> Result<Option<AtomicChatState>, AtomicCoreError> {
    conn.query_row(
        "SELECT atomic_chat_state FROM conversations WHERE id=?1 AND user_id=?2 AND soul_id=?3",
        rusqlite::params![conversation_id, user_id, soul_id],
        |row| read_chat_state(row, 0),
    )
    .optional()?
    .ok_or_else(|| AtomicCoreError::Validation("Chat owner/Soul does not match".to_string()))
}

fn last_chat_message(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Option<ChatMessage>, AtomicCoreError> {
    Ok(conn.query_row(
        "SELECT id, conversation_id, role, content, created_at, message_index FROM chat_messages WHERE conversation_id=?1 AND role IN ('user','assistant') ORDER BY message_index DESC LIMIT 1",
        [conversation_id], |row| Ok(ChatMessage {
            id: row.get(0)?, conversation_id: row.get(1)?, role: row.get(2)?,
            content: row.get(3)?, created_at: row.get(4)?, message_index: row.get(5)?,
        }),
    ).optional()?)
}

pub fn admit_chat_turn(
    conn: &Connection,
    conversation_id: &str,
    user_id: &str,
    soul_id: &str,
    backend: &AtomicChatBackend,
    request: &AtomicChatRequest,
    context_window_tokens: usize,
) -> Result<ChatMessage, AtomicCoreError> {
    let tx = conn.unchecked_transaction()?;
    let current = owned_chat_state(&tx, conversation_id, user_id, soul_id)?;
    let last = last_chat_message(&tx, conversation_id)?;
    let outcome_unknown = if let AtomicChatRequest::Retry { user_message_id } = request {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM chat_tool_calls WHERE message_id=?1 AND (status='running' OR json_extract(tool_output,'$.outcome_unknown')=1))",
            [user_message_id], |row| row.get(0),
        )?
    } else {
        false
    };
    let mut state = prepared_chat_state(
        current,
        backend,
        last.as_ref(),
        request,
        context_window_tokens,
        outcome_unknown,
    )?;
    let (saved, content) = chat_admission_input(&state, last.as_ref(), request);
    let message = match saved {
        Some(message) => message,
        None => save_message(&tx, conversation_id, "user", content)?,
    };
    admitted_chat_message(&mut state, request, &message.id);
    write_chat_state(&tx, conversation_id, Some(&state))?;
    tx.commit()?;
    Ok(message)
}

pub fn fail_chat_turn(
    conn: &Connection,
    conversation_id: &str,
    user_message_id: &str,
    error: &str,
    uncertain: bool,
    native_outcome: Option<&AtomicChatNativeOutcome>,
) -> Result<(), AtomicCoreError> {
    let tx = conn.unchecked_transaction()?;
    let mut state = stored_chat_state(&tx, conversation_id)?
        .ok_or_else(|| AtomicCoreError::Conflict("No admitted chat turn".to_string()))?;
    failed_chat_state(&mut state, user_message_id, error, uncertain, native_outcome)?;
    write_chat_state(&tx, conversation_id, Some(&state))?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn changed_chat_maintenance(
    current: Option<AtomicChatState>,
    expected: &AtomicChatState,
    maintenance: &crate::AtomicChatMaintenance,
    context_tokens: Option<usize>,
) -> Result<AtomicChatState, AtomicCoreError> {
    let mut state = current.filter(|state| state == expected
        && state.backend == AtomicChatBackend::ClaudeCode)
        .ok_or_else(|| AtomicCoreError::Conflict("Native sitting changed before maintenance checkpoint".into()))?;
    state.maintenance = Some(maintenance.clone());
    state.native_context_tokens = context_tokens;
    Ok(state)
}

pub fn set_chat_maintenance(
    conn: &Connection,
    conversation_id: &str,
    user_id: &str,
    soul_id: &str,
    expected: &AtomicChatState,
    maintenance: &crate::AtomicChatMaintenance,
    context_tokens: Option<usize>,
) -> Result<(), AtomicCoreError> {
    let tx = conn.unchecked_transaction()?;
    let current = owned_chat_state(&tx, conversation_id, user_id, soul_id)?;
    let state = changed_chat_maintenance(current, expected, maintenance, context_tokens)?;
    write_chat_state(&tx, conversation_id, Some(&state))?;
    tx.commit()?;
    Ok(())
}

pub fn set_chat_phase(
    conn: &Connection,
    conversation_id: &str,
    user_id: &str,
    soul_id: &str,
    backend: &AtomicChatBackend,
    phase: &AtomicChatPhase,
) -> Result<(), AtomicCoreError> {
    let tx = conn.unchecked_transaction()?;
    let current = owned_chat_state(&tx, conversation_id, user_id, soul_id)?;
    let last = last_chat_message(&tx, conversation_id)?;
    let state = bound_chat_state(
        current,
        backend,
        last.as_ref().map(|last| last.role.as_str()),
    )?;
    let state = changed_chat_phase(state, phase)?;
    write_chat_state(&tx, conversation_id, Some(&state))?;
    tx.commit()?;
    Ok(())
}

// ==================== CRUD Operations ====================

/// Create a new conversation
pub fn create_conversation(
    conn: &Connection,
    tag_ids: &[String],
    title: Option<&str>,
    owner: Option<(&str, &str)>,
) -> Result<ConversationWithTags, AtomicCoreError> {
    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    conn.execute(
        "INSERT INTO conversations (id, title, user_id, soul_id, created_at, updated_at, is_archived)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
        rusqlite::params![
            &id,
            &title,
            owner.map(|(user_id, _)| user_id),
            owner.map(|(_, soul_id)| soul_id),
            &now,
            &now
        ],
    )?;

    for tag_id in tag_ids {
        conn.execute(
            "INSERT INTO conversation_tags (conversation_id, tag_id) VALUES (?1, ?2)",
            rusqlite::params![&id, tag_id],
        )?;
    }

    let tags = get_conversation_tags(conn, &id)?;

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

/// Get all conversations, optionally filtered by tag
pub fn get_conversations(
    conn: &Connection,
    filter_tag_id: Option<&str>,
    limit: i32,
    offset: i32,
    owner: Option<(&str, &str)>,
) -> Result<Vec<ConversationWithTags>, AtomicCoreError> {
    let user_id = owner.map(|(user_id, _)| user_id);
    let soul_id = owner.map(|(_, soul_id)| soul_id);
    let conversations: Vec<Conversation> = if let Some(tag_id) = filter_tag_id {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT c.id, c.title, c.user_id, c.soul_id, c.created_at, c.updated_at, c.is_archived, c.atomic_chat_state
             FROM conversations c
             JOIN conversation_tags ct ON ct.conversation_id = c.id
             WHERE ct.tag_id = ?1
               AND (?2 IS NULL OR (c.user_id = ?2 AND c.soul_id = ?3))
               AND c.is_archived = 0
             ORDER BY c.updated_at DESC
             LIMIT ?4 OFFSET ?5",
        )?;

        let results = stmt
            .query_map(
                rusqlite::params![tag_id, user_id, soul_id, limit, offset],
                |row| {
                    Ok(Conversation {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        user_id: row.get(2)?,
                        soul_id: row.get(3)?,
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                        is_archived: row.get::<_, i32>(6)? != 0,
                        atomic_chat_state: read_chat_state(row, 7)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        results
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
             FROM conversations
             WHERE (?1 IS NULL OR (user_id = ?1 AND soul_id = ?2)) AND is_archived = 0
             ORDER BY updated_at DESC
             LIMIT ?3 OFFSET ?4",
        )?;

        let results = stmt
            .query_map(rusqlite::params![user_id, soul_id, limit, offset], |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    user_id: row.get(2)?,
                    soul_id: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    is_archived: row.get::<_, i32>(6)? != 0,
                    atomic_chat_state: read_chat_state(row, 7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        results
    };

    if conversations.is_empty() {
        return Ok(Vec::new());
    }

    let conv_ids: Vec<String> = conversations.iter().map(|c| c.id.clone()).collect();

    // Batch fetch tags for all conversations
    let tag_map = batch_fetch_conversation_tags(conn, &conv_ids)?;

    // Batch fetch summaries (message count + last preview) for all conversations
    let summary_map = batch_fetch_conversation_summaries(conn, &conv_ids)?;

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

/// Get a single conversation with all messages
pub fn get_conversation(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Option<ConversationWithMessages>, AtomicCoreError> {
    let conversation: Option<Conversation> = conn
        .query_row(
            "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
             FROM conversations
             WHERE id = ?1",
            [conversation_id],
            |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    user_id: row.get(2)?,
                    soul_id: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    is_archived: row.get::<_, i32>(6)? != 0,
                        atomic_chat_state: read_chat_state(row, 7)?,
                })
            },
        )
        .optional()?;

    match conversation {
        Some(conv) => {
            let tags = get_conversation_tags(conn, &conv.id)?;
            let messages = get_messages_with_context(conn, &conv.id)?;

            Ok(Some(ConversationWithMessages {
                conversation: conv,
                tags,
                messages,
            }))
        }
        None => Ok(None),
    }
}

/// Update a conversation (title, archive status)
pub fn update_conversation(
    conn: &Connection,
    id: &str,
    title: Option<&str>,
    is_archived: Option<bool>,
) -> Result<Conversation, AtomicCoreError> {
    let now = chrono::Utc::now().to_rfc3339();

    if let Some(t) = title {
        conn.execute(
            "UPDATE conversations SET title = ?1 WHERE id = ?2",
            rusqlite::params![t, id],
        )?;
    }

    if let Some(archived) = is_archived {
        conn.execute(
            "UPDATE conversations SET is_archived = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![if archived { 1 } else { 0 }, &now, id],
        )?;
    }

    conn.query_row(
        "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
         FROM conversations
         WHERE id = ?1",
        [id],
        |row| {
            Ok(Conversation {
                id: row.get(0)?,
                title: row.get(1)?,
                user_id: row.get(2)?,
                soul_id: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                is_archived: row.get::<_, i32>(6)? != 0,
                atomic_chat_state: read_chat_state(row, 7)?,
            })
        },
    )
    .map_err(|_| AtomicCoreError::NotFound(format!("Conversation not found: {}", id)))
}

/// Delete a conversation
pub fn delete_conversation(conn: &Connection, id: &str) -> Result<(), AtomicCoreError> {
    conn.execute("DELETE FROM conversations WHERE id = ?1", [id])?;
    Ok(())
}

// ==================== Scope Management ====================

/// Set the full scope (replace all tags)
pub fn set_conversation_scope(
    conn: &Connection,
    conversation_id: &str,
    tag_ids: &[String],
) -> Result<ConversationWithTags, AtomicCoreError> {
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "DELETE FROM conversation_tags WHERE conversation_id = ?1",
        [conversation_id],
    )?;

    for tag_id in tag_ids {
        conn.execute(
            "INSERT INTO conversation_tags (conversation_id, tag_id) VALUES (?1, ?2)",
            rusqlite::params![conversation_id, tag_id],
        )?;
    }

    conn.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![&now, conversation_id],
    )?;

    let conversation = conn
        .query_row(
            "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
             FROM conversations WHERE id = ?1",
            [conversation_id],
            |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    user_id: row.get(2)?,
                    soul_id: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    is_archived: row.get::<_, i32>(6)? != 0,
                        atomic_chat_state: read_chat_state(row, 7)?,
                })
            },
        )
        .map_err(|_| {
            AtomicCoreError::NotFound(format!("Conversation not found: {}", conversation_id))
        })?;

    let tags = get_conversation_tags(conn, conversation_id)?;
    let (message_count, last_message_preview) = get_conversation_summary(conn, conversation_id)?;

    Ok(ConversationWithTags {
        conversation,
        tags,
        message_count,
        last_message_preview,
    })
}

/// Add a single tag to scope
pub fn add_tag_to_scope(
    conn: &Connection,
    conversation_id: &str,
    tag_id: &str,
) -> Result<ConversationWithTags, AtomicCoreError> {
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT OR IGNORE INTO conversation_tags (conversation_id, tag_id) VALUES (?1, ?2)",
        rusqlite::params![conversation_id, tag_id],
    )?;

    conn.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![&now, conversation_id],
    )?;

    let conversation = conn
        .query_row(
            "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
             FROM conversations WHERE id = ?1",
            [conversation_id],
            |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    user_id: row.get(2)?,
                    soul_id: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    is_archived: row.get::<_, i32>(6)? != 0,
                        atomic_chat_state: read_chat_state(row, 7)?,
                })
            },
        )
        .map_err(|_| {
            AtomicCoreError::NotFound(format!("Conversation not found: {}", conversation_id))
        })?;

    let tags = get_conversation_tags(conn, conversation_id)?;
    let (message_count, last_message_preview) = get_conversation_summary(conn, conversation_id)?;

    Ok(ConversationWithTags {
        conversation,
        tags,
        message_count,
        last_message_preview,
    })
}

/// Remove a single tag from scope
pub fn remove_tag_from_scope(
    conn: &Connection,
    conversation_id: &str,
    tag_id: &str,
) -> Result<ConversationWithTags, AtomicCoreError> {
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "DELETE FROM conversation_tags WHERE conversation_id = ?1 AND tag_id = ?2",
        rusqlite::params![conversation_id, tag_id],
    )?;

    conn.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![&now, conversation_id],
    )?;

    let conversation = conn
        .query_row(
            "SELECT id, title, user_id, soul_id, created_at, updated_at, is_archived, atomic_chat_state
             FROM conversations WHERE id = ?1",
            [conversation_id],
            |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    user_id: row.get(2)?,
                    soul_id: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    is_archived: row.get::<_, i32>(6)? != 0,
                        atomic_chat_state: read_chat_state(row, 7)?,
                })
            },
        )
        .map_err(|_| {
            AtomicCoreError::NotFound(format!("Conversation not found: {}", conversation_id))
        })?;

    let tags = get_conversation_tags(conn, conversation_id)?;
    let (message_count, last_message_preview) = get_conversation_summary(conn, conversation_id)?;

    Ok(ConversationWithTags {
        conversation,
        tags,
        message_count,
        last_message_preview,
    })
}

// ==================== Message DB Operations ====================

/// Save a message and return its stored identity, timestamp and index.
pub fn save_message(
    conn: &Connection,
    conversation_id: &str,
    role: &str,
    content: &str,
) -> Result<ChatMessage, AtomicCoreError> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let message_index: i32 = conn.query_row(
        "SELECT COALESCE(MAX(message_index), -1) + 1 FROM chat_messages WHERE conversation_id = ?1",
        [conversation_id],
        |row| row.get(0),
    )?;

    conn.execute(
        "INSERT INTO chat_messages (id, conversation_id, role, content, created_at, message_index)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            &message_id,
            conversation_id,
            role,
            content,
            &now,
            message_index
        ],
    )?;

    conn.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![&now, conversation_id],
    )?;

    if role != "system" {
        conn.execute(
            "INSERT INTO chat_messages_fts(id, conversation_id, content) VALUES (?1, ?2, ?3)",
            rusqlite::params![message_id, conversation_id, content],
        )?;
    }

    Ok(ChatMessage {
        id: message_id,
        conversation_id: conversation_id.to_string(),
        role: role.to_string(),
        content: content.to_string(),
        created_at: now,
        message_index,
    })
}

/// Save tool calls for a message
fn save_tool_calls(
    conn: &Connection,
    message_id: &str,
    tool_calls: &[ChatToolCall],
) -> Result<(), AtomicCoreError> {
    for tool_call in tool_calls {
        let changed = conn.execute(
            "INSERT INTO chat_tool_calls (id, message_id, tool_name, tool_input, tool_output, status, created_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET message_id=excluded.message_id,
                 tool_output=excluded.tool_output, status=excluded.status, completed_at=excluded.completed_at
             WHERE (SELECT conversation_id FROM chat_messages WHERE id=chat_tool_calls.message_id)
                 = (SELECT conversation_id FROM chat_messages WHERE id=excluded.message_id)
                 AND (chat_tool_calls.message_id=excluded.message_id
                     OR ((SELECT role FROM chat_messages WHERE id=chat_tool_calls.message_id)='user'
                         AND (SELECT role FROM chat_messages WHERE id=excluded.message_id)='assistant'))",
            rusqlite::params![
                &tool_call.id,
                message_id,
                &tool_call.tool_name,
                serde_json::to_string(&tool_call.tool_input).unwrap_or_default(),
                tool_call.tool_output.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default()),
                &tool_call.status,
                &tool_call.created_at,
                &tool_call.completed_at,
            ],
        )?;
        if changed != 1 {
            return Err(AtomicCoreError::Validation(
                "Tool record identity or publication target does not match".to_string(),
            ));
        }
    }
    Ok(())
}

/// Save citations for a message
fn save_citations(
    conn: &Connection,
    message_id: &str,
    citations: &[ChatCitation],
) -> Result<(), AtomicCoreError> {
    for citation in citations {
        let changed = conn.execute(
            "INSERT INTO chat_citations (id, message_id, citation_index, atom_id, chunk_index, excerpt, relevance_score)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET message_id=excluded.message_id
             WHERE (SELECT conversation_id FROM chat_messages WHERE id=chat_citations.message_id)
                 = (SELECT conversation_id FROM chat_messages WHERE id=excluded.message_id)
                 AND (chat_citations.message_id=excluded.message_id
                     OR ((SELECT role FROM chat_messages WHERE id=chat_citations.message_id)='user'
                         AND (SELECT role FROM chat_messages WHERE id=excluded.message_id)='assistant'))",
            rusqlite::params![
                &citation.id,
                message_id,
                citation.citation_index,
                &citation.atom_id,
                citation.chunk_index,
                &citation.excerpt,
                citation.relevance_score,
            ],
        )?;
        if changed != 1 {
            return Err(AtomicCoreError::Validation(
                "Citation identity or publication target does not match".to_string(),
            ));
        }
    }
    Ok(())
}

pub fn save_chat_tool_state(
    conn: &Connection,
    conversation_id: &str,
    message_id: &str,
    tool_calls: &[ChatToolCall],
    citations: &[ChatCitation],
    citation_index: i32,
) -> Result<(), AtomicCoreError> {
    let tx = conn.unchecked_transaction()?;
    write_chat_tool_state(
        &tx,
        conversation_id,
        message_id,
        tool_calls,
        citations,
        citation_index,
    )?;
    tx.commit()?;
    Ok(())
}

fn write_chat_tool_state(
    conn: &Connection,
    conversation_id: &str,
    message_id: &str,
    tool_calls: &[ChatToolCall],
    citations: &[ChatCitation],
    citation_index: i32,
) -> Result<(), AtomicCoreError> {
    let belongs: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM chat_messages WHERE id=?1 AND conversation_id=?2)",
        rusqlite::params![message_id, conversation_id],
        |row| row.get(0),
    )?;
    if !belongs {
        return Err(AtomicCoreError::Validation(
            "Tool state target is outside this conversation".to_string(),
        ));
    }
    save_tool_calls(conn, message_id, tool_calls)?;
    save_citations(conn, message_id, citations)?;
    conn.execute(
        "UPDATE conversations SET atomic_citation_index=MAX(atomic_citation_index, ?1) WHERE id=?2",
        rusqlite::params![citation_index, conversation_id],
    )?;
    Ok(())
}

/// Publish text and its tool/citation records together, or leave the turn unanswered.
pub fn save_chat_reply(
    conn: &Connection,
    conversation_id: &str,
    expected_user_message_id: Option<&str>,
    content: &str,
    tool_calls: &[ChatToolCall],
    citations: &[ChatCitation],
    citation_index: i32,
    native_outcome: Option<&AtomicChatNativeOutcome>,
) -> Result<ChatMessage, AtomicCoreError> {
    let tx = conn.unchecked_transaction()?;
    let message = save_message(&tx, conversation_id, "assistant", content)?;
    let state = completed_chat_state(
        stored_chat_state(&tx, conversation_id)?,
        expected_user_message_id,
        &message.id,
        native_outcome,
        tool_calls,
    )?;
    write_chat_tool_state(
        &tx,
        conversation_id,
        &message.id,
        tool_calls,
        citations,
        citation_index,
    )?;
    if state.is_some() {
        write_chat_state(&tx, conversation_id, state.as_ref())?;
    }
    tx.commit()?;
    Ok(message)
}

/// Get scope tag IDs for a conversation
pub fn get_scope_tag_ids(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<String>, AtomicCoreError> {
    let mut stmt =
        conn.prepare("SELECT tag_id FROM conversation_tags WHERE conversation_id = ?1")?;

    let tag_ids = stmt
        .query_map([conversation_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(tag_ids)
}

/// Get scope description for system prompt
pub fn get_scope_description(conn: &Connection, tag_ids: &[String]) -> String {
    if tag_ids.is_empty() {
        return "You have access to ALL atoms in the knowledge base.".to_string();
    }

    let placeholders: Vec<String> = (0..tag_ids.len()).map(|i| format!("?{}", i + 1)).collect();
    let query = format!(
        "SELECT name FROM tags WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut stmt = match conn.prepare(&query) {
        Ok(s) => s,
        Err(_) => return "You have access to a scoped set of atoms.".to_string(),
    };

    let params: Vec<&dyn rusqlite::ToSql> =
        tag_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let names: Vec<String> = stmt
        .query_map(params.as_slice(), |row| row.get(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    if names.is_empty() {
        "You have access to a scoped set of atoms.".to_string()
    } else {
        format!(
            "You have access to atoms tagged with: {}. Focus your search on these topics.",
            names.join(", ")
        )
    }
}

/// Get conversation messages in provider Message format
pub fn get_conversation_messages(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<crate::providers::types::Message>, AtomicCoreError> {
    use crate::providers::types::{Message, MessageRole};

    let mut stmt = conn.prepare(
        "SELECT role, content FROM chat_messages WHERE conversation_id = ?1 ORDER BY message_index",
    )?;

    let messages = stmt
        .query_map([conversation_id], |row| {
            let role_str: String = row.get(0)?;
            let content: Option<String> = row.get(1)?;
            let role = match role_str.as_str() {
                "system" => MessageRole::System,
                "assistant" => MessageRole::Assistant,
                "tool" => MessageRole::Tool,
                _ => MessageRole::User,
            };
            Ok(Message {
                role,
                content,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use tempfile::NamedTempFile;

    fn setup_db() -> (Database, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();
        (db, temp_file)
    }

    #[test]
    fn native_usage_requires_complete_normalized_counts_but_old_state_still_reads() {
        let value = serde_json::json!({"turn_id":"fictional-turn", "message_id":"primary",
            "input_tokens":10, "cache_creation_input_tokens":0, "cache_read_input_tokens":20, "output_tokens":5});
        assert!(serde_json::from_value::<crate::AtomicChatUsage>(value.clone()).is_ok());
        for key in ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<crate::AtomicChatUsage>(missing).is_err());
            let mut null = value.clone();
            null[key] = serde_json::Value::Null;
            assert!(serde_json::from_value::<crate::AtomicChatUsage>(null).is_err());
        }
        let old: AtomicChatState = serde_json::from_value(serde_json::json!({
            "backend":"api", "phase":"open", "active":null
        })).unwrap();
        assert!(old.native_usage.is_none());
        assert!(old.maintenance.is_none());
        assert!(!old.sitting_wide_citations);
        assert!(bound_chat_state(None, &AtomicChatBackend::Api, None).unwrap().sitting_wide_citations);
        assert!(!bound_chat_state(None, &AtomicChatBackend::Api, Some("assistant")).unwrap().sitting_wide_citations);
    }

    #[test]
    fn maintenance_checkpoint_is_scoped_cas_and_preserves_dialogue_and_phase() {
        use crate::{AtomicChatMaintenance as Maintenance, AtomicChatMaintenanceStatus as Status};
        let (db, _file) = setup_db();
        let conn = db.conn.lock().unwrap();
        let conv = create_conversation(&conn, &[], None, Some(("TestOwner", "TestSoul"))).unwrap();
        let id = &conv.conversation.id;
        set_chat_phase(&conn, id, "TestOwner", "TestSoul", &AtomicChatBackend::ClaudeCode, &AtomicChatPhase::Open).unwrap();
        save_message(&conn, id, "system", "fictional Presence").unwrap();
        let saved = stored_chat_state(&conn, id).unwrap().unwrap();
        let pending: Maintenance = serde_json::from_value(serde_json::json!({
            "attempt_id":"cleanup", "native_attempt_id":"native", "turn_id":"turn", "previous_attempt_id":null,
            "kind":"ordinary", "status":"pending", "error":null,
            "ordinary_disabled":false, "awaiting_measurement":false, "final_used":false
        })).unwrap();
        assert!(set_chat_maintenance(&conn, id, "OtherOwner", "TestSoul", &saved, &pending, Some(100)).is_err());
        set_chat_maintenance(&conn, id, "TestOwner", "TestSoul", &saved, &pending, Some(100)).unwrap();
        assert!(set_chat_maintenance(&conn, id, "TestOwner", "TestSoul", &saved, &pending, None).is_err());
        let mut current = stored_chat_state(&conn, id).unwrap().unwrap();
        assert_eq!(current.phase, AtomicChatPhase::Open);
        assert_eq!(current.native_context_tokens, Some(100));
        assert_eq!(get_messages_with_context(&conn, id).unwrap()[0].message.content, "fictional Presence");
        assert!(changed_chat_phase(current.clone(), &AtomicChatPhase::Closing).is_err());
        let request = AtomicChatRequest::New { content:"new input".into(), page_context:None, canvas_context:None };
        assert!(prepared_chat_state(Some(current.clone()), &AtomicChatBackend::ClaudeCode, None,
                                    &request, 128000, false).is_err());
        current.maintenance.as_mut().unwrap().status = Status::Blocked;
        assert!(prepared_chat_state(Some(current.clone()), &AtomicChatBackend::ClaudeCode, None,
                                    &request, 128000, false).is_err());
        current.phase = AtomicChatPhase::Closing;
        assert!(prepared_chat_state(Some(current.clone()), &AtomicChatBackend::ClaudeCode, None,
            &AtomicChatRequest::Recap { instruction:"fictional recap".into() }, 128000, false).is_ok());
        let mut failed = pending;
        failed.status = Status::Failed;
        let stored = stored_chat_state(&conn, id).unwrap().unwrap();
        set_chat_maintenance(&conn, id, "TestOwner", "TestSoul", &stored, &failed, None).unwrap();
        assert_eq!(stored_chat_state(&conn, id).unwrap().unwrap().native_context_tokens, None);
    }

    #[test]
    fn cleanup_latch_waits_for_later_finalized_coverage() {
        for tokens in [50_000, 60_000, 60_001] {
            let mut state: AtomicChatState = serde_json::from_value(serde_json::json!({
                "backend":"claude_code", "phase":"open", "active":null, "context_window_tokens":100000,
                "maintenance":{"attempt_id":"cleanup", "native_attempt_id":"before", "turn_id":"turn",
                    "kind":"ordinary", "status":"complete", "error":null, "previous_attempt_id":null,
                    "ordinary_disabled":false,"awaiting_measurement":true,"final_used":false}
            })).unwrap();
            let usage = crate::AtomicChatUsage { turn_id:"turn".into(), message_id:"message".into(),
                input_tokens:tokens, cache_creation_input_tokens:0, cache_read_input_tokens:0, output_tokens:0 };
            let mut outcome = AtomicChatNativeOutcome { attempt_id:"before".into(), usage:Some(usage.clone()),
                input_recorded:Some(true), context_tokens:Some(tokens) };
            apply_native_usage(&mut state, "turn", Some(&outcome)).unwrap();
            assert!(state.maintenance.as_ref().unwrap().awaiting_measurement, "same pre-clean turn is not new usage");
            outcome.attempt_id = "after".into();
            outcome.usage = None;
            apply_native_usage(&mut state, "turn", Some(&outcome)).unwrap();
            assert!(state.maintenance.as_ref().unwrap().awaiting_measurement);
            outcome.usage = Some(usage);
            outcome.context_tokens = None;
            apply_native_usage(&mut state, "turn", Some(&outcome)).unwrap();
            assert!(state.maintenance.as_ref().unwrap().awaiting_measurement);
            outcome.context_tokens = Some(tokens);
            apply_native_usage(&mut state, "turn", Some(&outcome)).unwrap();
            assert!(!state.maintenance.as_ref().unwrap().awaiting_measurement);
            assert_eq!(state.maintenance.as_ref().unwrap().ordinary_disabled, tokens > 60_000);
        }
    }

    #[test]
    fn failed_native_recap_retains_the_ordinary_capacity_basis() {
        let state: AtomicChatState = serde_json::from_value(serde_json::json!({
            "backend":"claude_code","phase":"closing","active":null,"context_window_tokens":200000})).unwrap();
        let request = AtomicChatRequest::Recap { instruction:"fictional recap".into() };
        let mut admitted = prepared_chat_state(Some(state), &AtomicChatBackend::ClaudeCode, None, &request, 128000, false).unwrap();
        assert_eq!(admitted.context_window_tokens, Some(200000));
        admitted_chat_message(&mut admitted, &request, "recap");
        failed_chat_state(&mut admitted, "recap", "provider context overflow", false, None).unwrap();
        assert_eq!(admitted.context_window_tokens, Some(200000));
    }

    #[test]
    fn native_recovery_cannot_publish_after_close_or_after_recap_starts() {
        let mut state: AtomicChatState = serde_json::from_value(serde_json::json!({
            "backend":"claude_code", "phase":"open", "active":{
                "user_message_id":"turn", "native_attempt_id":"attempt", "status":"uncertain", "error":"lost delivery"
            }
        })).unwrap();
        let native = AtomicChatNativeOutcome { attempt_id:"attempt".into(), usage:None, input_recorded:Some(true), context_tokens:Some(123) };
        failed_chat_state(&mut state, "turn", "recorded failure", false, Some(&native)).unwrap();
        assert_eq!(state.native_context_tokens, Some(123));
        state.active.as_mut().unwrap().status = AtomicChatTurnStatus::Running;
        failed_chat_state(&mut state, "turn", "no native work admitted", false, None).unwrap();
        assert_eq!(state.native_context_tokens, Some(123));
        assert_eq!(state.active.as_ref().unwrap().native_input_recorded, Some(true));
        let refused = AtomicChatNativeOutcome { input_recorded:Some(false), ..native.clone() };
        failed_chat_state(&mut state, "turn", "retry refused", false, Some(&refused)).unwrap();
        assert_eq!(state.active.as_ref().unwrap().native_input_recorded, Some(true));
        state.active.as_mut().unwrap().native_input_recorded = None;
        failed_chat_state(&mut state, "turn", "input not delivered", false, Some(&refused)).unwrap();
        assert_eq!(state.active.as_ref().unwrap().native_input_recorded, Some(false));
        failed_chat_state(&mut state, "turn", "writer may still submit", true, Some(&refused)).unwrap();
        assert_eq!(state.active.as_ref().unwrap().native_input_recorded, None);
        let unknown = AtomicChatNativeOutcome { input_recorded:None, context_tokens:None, ..native.clone() };
        failed_chat_state(&mut state, "turn", "delivery unknown", true, Some(&unknown)).unwrap();
        assert_eq!(state.native_context_tokens, None);
        assert_eq!(state.active.as_ref().unwrap().native_input_recorded, None);
        assert!(completed_chat_state(Some(state.clone()), Some("turn"), "reply", Some(&native), &[]).unwrap().unwrap().active.is_none());
        let unresolved = ChatToolCall { id:"unknown-tool".into(), message_id:"turn".into(),
            tool_name:"edit_atom".into(), tool_input:serde_json::json!({}),
            tool_output:Some(serde_json::json!({"outcome_unknown":true})), status:"failed".into(),
            created_at:String::new(), completed_at:None };
        let closed_input = completed_chat_state(Some(state.clone()), Some("turn"), "reply", Some(&native), &[unresolved]).unwrap().unwrap();
        assert!(closed_input.active.is_none());
        assert_eq!(closed_input.phase, AtomicChatPhase::Closing);
        state.phase = AtomicChatPhase::Closed;
        assert!(completed_chat_state(Some(state.clone()), Some("turn"), "reply", Some(&native), &[]).is_err());
        state.phase = AtomicChatPhase::Closing;
        state.recap = Some(AtomicChatRecap { request_message_id:"recap".into(), native_attempt_id:Some("recap-attempt".into()),
            outcome:AtomicChatRecapOutcome::Running });
        assert!(completed_chat_state(Some(state.clone()), Some("turn"), "reply", Some(&native), &[]).is_err());
        state.recap.as_mut().unwrap().outcome = AtomicChatRecapOutcome::Complete { reply_message_id:"saved-recap".into() };
        let recap = AtomicChatNativeOutcome { attempt_id:"recap-attempt".into(), usage:None, input_recorded:Some(true), context_tokens:None };
        assert!(completed_chat_state(Some(state.clone()), Some("recap"), "duplicate", Some(&recap), &[]).is_err());
        assert!(completed_chat_state(Some(state), Some("turn"), "late ordinary", Some(&native), &[]).is_err());
    }

    #[test]
    fn test_create_conversation() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        let result = create_conversation(
            &conn,
            &[],
            Some("Test Chat"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        assert_eq!(result.conversation.title, Some("Test Chat".to_string()));
        assert_eq!(result.message_count, 0);
        assert!(result.tags.is_empty());
    }

    #[test]
    fn test_create_conversation_with_tags() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        // Create a tag first
        let tag_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO tags (id, name, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![&tag_id, "TestTag", &now],
        )
        .unwrap();

        let result = create_conversation(
            &conn,
            &[tag_id.clone()],
            Some("Tagged Chat"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        assert_eq!(result.tags.len(), 1);
        assert_eq!(result.tags[0].name, "TestTag");
    }

    #[test]
    fn test_get_conversations() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        create_conversation(
            &conn,
            &[],
            Some("Chat 1"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        create_conversation(
            &conn,
            &[],
            Some("Chat 2"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();

        let conversations = get_conversations(
            &conn,
            None,
            10,
            0,
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        assert_eq!(conversations.len(), 2);

        create_conversation(&conn, &[], Some("Standalone"), None).unwrap();
        assert_eq!(
            get_conversations(&conn, None, 10, 0, None).unwrap().len(),
            3
        );
        assert_eq!(
            get_conversations(
                &conn,
                None,
                10,
                0,
                Some(("Fictional User", "Fictional Soul"))
            )
            .unwrap()
            .len(),
            2
        );
    }

    #[test]
    fn test_get_conversation_with_messages() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        let conv = create_conversation(
            &conn,
            &[],
            Some("Chat"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        save_message(&conn, &conv.conversation.id, "user", "Hello").unwrap();
        save_message(&conn, &conv.conversation.id, "assistant", "Hi there!").unwrap();

        let result = get_conversation(&conn, &conv.conversation.id).unwrap();
        assert!(result.is_some());
        let conv_with_msgs = result.unwrap();
        assert_eq!(conv_with_msgs.messages.len(), 2);
        assert_eq!(conv_with_msgs.messages[0].message.role, "user");
        assert_eq!(conv_with_msgs.messages[1].message.role, "assistant");
    }

    #[test]
    fn test_system_messages_hidden_from_summaries() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        let conv = create_conversation(
            &conn,
            &[],
            Some("Chat"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        save_message(&conn, &conv.conversation.id, "system", "Hidden snapshot").unwrap();
        save_message(&conn, &conv.conversation.id, "user", "Visible user message").unwrap();

        let (message_count, preview) =
            get_conversation_summary(&conn, &conv.conversation.id).unwrap();
        assert_eq!(message_count, 1);
        assert_eq!(preview.as_deref(), Some("Visible user message"));

        let conversations = get_conversations(
            &conn,
            None,
            10,
            0,
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        assert_eq!(conversations[0].message_count, 1);
        assert_eq!(
            conversations[0].last_message_preview.as_deref(),
            Some("Visible user message")
        );

        let raw = get_conversation(&conn, &conv.conversation.id)
            .unwrap()
            .unwrap();
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].message.role, "system");
    }

    #[test]
    fn test_update_conversation() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        let conv = create_conversation(
            &conn,
            &[],
            Some("Original"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        conn.execute(
            "UPDATE conversations SET updated_at = '2026-01-02T03:04:05Z' WHERE id = ?1",
            [&conv.conversation.id],
        )
        .unwrap();
        let updated =
            update_conversation(&conn, &conv.conversation.id, Some("Updated"), None).unwrap();
        assert_eq!(updated.title, Some("Updated".to_string()));
        assert_eq!(updated.updated_at, "2026-01-02T03:04:05Z");
    }

    #[test]
    fn test_delete_conversation() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        let conv = create_conversation(
            &conn,
            &[],
            Some("ToDelete"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        delete_conversation(&conn, &conv.conversation.id).unwrap();

        let result = get_conversation(&conn, &conv.conversation.id).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_scope_management() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        // Create tags
        let tag1_id = uuid::Uuid::new_v4().to_string();
        let tag2_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO tags (id, name, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![&tag1_id, "Tag1", &now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, name, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![&tag2_id, "Tag2", &now],
        )
        .unwrap();

        let conv =
            create_conversation(&conn, &[], None, Some(("Fictional User", "Fictional Soul")))
                .unwrap();

        // Add tag
        let result = add_tag_to_scope(&conn, &conv.conversation.id, &tag1_id).unwrap();
        assert_eq!(result.tags.len(), 1);

        // Add another tag
        let result = add_tag_to_scope(&conn, &conv.conversation.id, &tag2_id).unwrap();
        assert_eq!(result.tags.len(), 2);

        // Remove tag
        let result = remove_tag_from_scope(&conn, &conv.conversation.id, &tag1_id).unwrap();
        assert_eq!(result.tags.len(), 1);

        // Set scope (replace all)
        let result = set_conversation_scope(
            &conn,
            &conv.conversation.id,
            &[tag1_id.clone(), tag2_id.clone()],
        )
        .unwrap();
        assert_eq!(result.tags.len(), 2);
    }

    #[test]
    fn test_truncate_preview_utf8_safe() {
        // ASCII shorter than limit — returned as-is
        assert_eq!(truncate_preview("hello", 100), "hello");

        // ASCII longer than limit — truncated with ellipsis
        let long_ascii = "a".repeat(150);
        let truncated = truncate_preview(&long_ascii, 100);
        assert_eq!(truncated.len(), 103); // 100 'a' + "..."
        assert!(truncated.ends_with("..."));

        // Cyrillic that would panic with byte slicing at 100 — must not panic
        // and must return a valid UTF-8 string.
        let cyrillic = "Это пример длинного текста на русском языке, который содержит только обычные кириллические символы и несколько предложений подряд. Он нужен только для воспроизведения ошибки.";
        let preview = truncate_preview(cyrillic, 100);
        assert!(preview.ends_with("..."));
        assert_eq!(preview.chars().count(), 103); // 100 chars + "..."

        // Emoji (4-byte UTF-8) — must not panic
        let emoji = "😀".repeat(150);
        let preview = truncate_preview(&emoji, 100);
        assert!(preview.ends_with("..."));
        assert_eq!(preview.chars().count(), 103);
    }

    #[test]
    fn test_get_conversations_with_cyrillic_message() {
        // Regression test for crash on /api/conversations when an assistant message
        // contains long Cyrillic text. The batch summary path used to byte-slice the
        // message content at index 100, which falls inside a multi-byte UTF-8
        // character and panicked.
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        let conv = create_conversation(
            &conn,
            &[],
            Some("Russian chat"),
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        save_message(&conn, &conv.conversation.id, "user", "Привет").unwrap();
        save_message(
            &conn,
            &conv.conversation.id,
            "assistant",
            "Это пример длинного текста на русском языке, который содержит только обычные кириллические символы и несколько предложений подряд. Он нужен только для воспроизведения ошибки в обработке preview строки. Если система пытается обрезать такой текст по байтам, а не по корректной UTF-8 границе символа, сервер падает при загрузке списка разговоров.",
        )
        .unwrap();

        // Must not panic.
        let conversations = get_conversations(
            &conn,
            None,
            10,
            0,
            Some(("Fictional User", "Fictional Soul")),
        )
        .unwrap();
        assert_eq!(conversations.len(), 1);
        let preview = conversations[0]
            .last_message_preview
            .as_ref()
            .expect("preview should be set");
        assert!(preview.ends_with("..."));
        // Preview must be valid UTF-8 (it is by Rust type guarantee, but assert
        // it parses cleanly as Cyrillic content).
        assert!(preview.starts_with("Это пример"));
    }

    #[test]
    fn test_save_message() {
        let (db, _temp) = setup_db();
        let conn = db.conn.lock().unwrap();

        let conv =
            create_conversation(&conn, &[], None, Some(("Fictional User", "Fictional Soul")))
                .unwrap();
        let first = save_message(&conn, &conv.conversation.id, "user", "Hello").unwrap();

        assert!(!first.id.is_empty());
        assert_eq!(first.message_index, 0);
        let saved_time: String = conn
            .query_row(
                "SELECT created_at FROM chat_messages WHERE id=?1",
                [&first.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first.created_at, saved_time);

        let second = save_message(&conn, &conv.conversation.id, "assistant", "Hi!").unwrap();
        assert_eq!(second.message_index, 1);
        save_message(&conn, &conv.conversation.id, "system", "Hidden Presence").unwrap();
        let indexed: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM chat_messages_fts WHERE conversation_id=?1",
                [&conv.conversation.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            indexed, 2,
            "the message writer indexes dialogue, not system snapshots"
        );
    }
}
