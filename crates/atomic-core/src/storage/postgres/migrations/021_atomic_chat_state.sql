ALTER TABLE conversations ADD COLUMN IF NOT EXISTS atomic_chat_state TEXT;
INSERT INTO schema_version (version) VALUES (21);
