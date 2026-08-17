CREATE TABLE projects (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    repo_url TEXT NOT NULL,
    base_domain TEXT NOT NULL,
    pause_after_seconds INTEGER NOT NULL,
    destroy_after_seconds INTEGER NOT NULL,
    port INTEGER NOT NULL,
    dockerfile TEXT,
    build_context TEXT NOT NULL DEFAULT '.',
    on_start_json TEXT NOT NULL DEFAULT '[]',
    dependencies_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE environments (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch_name TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    state TEXT NOT NULL,
    url TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL
);

CREATE INDEX idx_environments_project_id ON environments(project_id);
CREATE INDEX idx_environments_state ON environments(state);

CREATE TABLE audit_events (
    id INTEGER PRIMARY KEY,
    environment_id INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    detail TEXT,
    occurred_at INTEGER NOT NULL
);

CREATE INDEX idx_audit_events_environment ON audit_events(environment_id);
CREATE INDEX idx_audit_events_occurred ON audit_events(occurred_at);