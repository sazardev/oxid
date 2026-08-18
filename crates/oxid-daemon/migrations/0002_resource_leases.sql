-- Resource pooling (SPEC.md §3.1): one row per (branch, dependency), tracking
-- which logical Postgres database or Redis index index a branch leased from
-- a shared instance. Reused across redeploys of the same branch (idempotent);
-- explicitly deleted when the branch is destroyed, since Oxid never
-- SQL-deletes an `environments` row, so `ON DELETE CASCADE` from
-- `environments` would never fire.
CREATE TABLE resource_leases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    shared_instance TEXT NOT NULL,
    resource_name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_resource_leases_unique
    ON resource_leases (project_id, branch, kind, shared_instance);
