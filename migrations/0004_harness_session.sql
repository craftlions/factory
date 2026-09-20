-- The harness's own identifiers for the session, reported by the harness after
-- launch. NULL on rows created before this migration and until it reports.
ALTER TABLE sessions ADD COLUMN harness_session_id TEXT;
ALTER TABLE sessions ADD COLUMN harness_session_file TEXT;
