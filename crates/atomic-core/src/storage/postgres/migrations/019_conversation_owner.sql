ALTER TABLE conversations ADD COLUMN IF NOT EXISTS user_id TEXT;
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS soul_id TEXT;

CREATE INDEX IF NOT EXISTS idx_conversations_owner
    ON conversations(db_id, user_id, soul_id, updated_at DESC);
