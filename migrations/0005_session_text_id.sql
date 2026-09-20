-- Session ids become short random strings instead of a counter. SQLite cannot
-- change a column's type, so the table is rebuilt. Existing rows keep their
-- number as text, which still matches their directory under sessions/.
CREATE TABLE sessions_new (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    note TEXT,
    isolation TEXT,
    workdir TEXT,
    provider TEXT,
    model TEXT,
    reasoning TEXT,
    harness_session_id TEXT,
    harness_session_file TEXT
);
INSERT INTO sessions_new
SELECT CAST(id AS TEXT), kind, status, started_at, ended_at, note, isolation, workdir,
       provider, model, reasoning, harness_session_id, harness_session_file
FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;
CREATE INDEX sessions_started_at ON sessions (started_at);
