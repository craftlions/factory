-- Secrets a harness needs, entered in the dashboard's settings. One per
-- harness; `kind` says which of the harness's ways of signing in it is.
CREATE TABLE credentials (
    harness TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    secret TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
