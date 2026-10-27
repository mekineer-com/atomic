ALTER TABLE conversations ADD COLUMN IF NOT EXISTS atomic_citation_index INTEGER NOT NULL DEFAULT 0;
ALTER TABLE chat_tool_calls ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'complete';
ALTER TABLE chat_tool_calls ADD COLUMN IF NOT EXISTS completed_at TEXT;
ALTER TABLE chat_citations ADD COLUMN IF NOT EXISTS citation_index INTEGER NOT NULL DEFAULT 0;

-- Legacy numbers were not stored. Physical order approximates the old heap scan;
-- exact historical numbering cannot be recovered after row movement.
WITH numbered AS (
    SELECT id, ROW_NUMBER() OVER (PARTITION BY message_id ORDER BY ctid)::INTEGER AS position
    FROM chat_citations
)
UPDATE chat_citations SET citation_index=numbered.position
FROM numbered WHERE chat_citations.id=numbered.id AND chat_citations.citation_index=0;

INSERT INTO schema_version (version) VALUES (20);
