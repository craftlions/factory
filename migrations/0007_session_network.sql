-- What a session may reach besides the model API, as JSON. NULL on rows from
-- before this migration, which are restricted.
ALTER TABLE sessions ADD COLUMN network TEXT;
