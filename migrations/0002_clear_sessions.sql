-- Sessions are no longer recorded automatically per process run. The rows
-- written so far are all of that automatic kind, so they are removed.
DELETE FROM sessions;
