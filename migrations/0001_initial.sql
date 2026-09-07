CREATE TABLE samples (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    cpu_percent REAL NOT NULL,
    load1 REAL NOT NULL,
    mem_used INTEGER NOT NULL,
    mem_total INTEGER NOT NULL,
    disk_used INTEGER NOT NULL,
    disk_total INTEGER NOT NULL,
    process_cpu_percent REAL NOT NULL,
    process_rss INTEGER NOT NULL,
    data_files INTEGER NOT NULL,
    data_bytes INTEGER NOT NULL
);
CREATE INDEX samples_ts ON samples (ts);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    note TEXT
);
CREATE INDEX sessions_started_at ON sessions (started_at);
