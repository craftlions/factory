-- What a session was created with. `kind` holds the harness id. Rows from
-- before this migration keep NULL here.
ALTER TABLE sessions ADD COLUMN isolation TEXT;
ALTER TABLE sessions ADD COLUMN workdir TEXT;
ALTER TABLE sessions ADD COLUMN provider TEXT;
ALTER TABLE sessions ADD COLUMN model TEXT;
ALTER TABLE sessions ADD COLUMN reasoning TEXT;
