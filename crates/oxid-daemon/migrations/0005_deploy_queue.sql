-- Resource admission control (SPEC.md "Eficiencia Absoluta"): a deploy that
-- doesn't fit in the host's currently-free memory/CPU is queued here instead
-- of either failing outright or overcommitting the host. Persisted (not
-- in-memory) so a queued request survives a daemon restart while it waits.
CREATE TABLE deploy_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    operator TEXT,
    requested_at INTEGER NOT NULL
);
