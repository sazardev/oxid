-- One repository, several services.
--
-- `repo_url` was UNIQUE, which made "one project per repository" a rule the
-- schema enforced. That was right while a repository held one deployable
-- thing, and wrong the moment monorepos were understood: a turborepo with
-- an API, a web app and a worker is three services that deploy, scale and
-- fail independently, and modelling them as one project means two of them
-- cannot exist.
--
-- What is actually unique is a repository *plus the part of it being
-- built*. Registering the same repo twice with the same context is still a
-- duplicate and still rejected; registering `apps/api` and `apps/web` is
-- two projects, which is what they are.
--
-- SQLite cannot drop a constraint, so the table is rebuilt. The column list
-- is spelled out rather than `SELECT *` so a future column added between
-- this migration being written and run cannot silently shift positions —
-- and it must name *every* column, including the ones earlier migrations
-- added (`git_token_enc` is easy to miss, and dropping it would lose every
-- private repository's credential).
PRAGMA foreign_keys = OFF;

CREATE TABLE projects_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    repo_url TEXT NOT NULL,
    base_domain TEXT NOT NULL,
    pause_after_seconds INTEGER NOT NULL,
    destroy_after_seconds INTEGER NOT NULL,
    port INTEGER NOT NULL,
    dockerfile TEXT,
    build_context TEXT NOT NULL DEFAULT '.',
    on_start_json TEXT NOT NULL DEFAULT '[]',
    dependencies_json TEXT NOT NULL DEFAULT '[]',
    memory_limit_mb INTEGER,
    cpu_limit_millicores INTEGER,
    git_token_enc TEXT,
    detected_stack TEXT,
    workspace TEXT,
    UNIQUE (repo_url, build_context)
);

INSERT INTO projects_new (
    id, name, repo_url, base_domain, pause_after_seconds, destroy_after_seconds,
    port, dockerfile, build_context, on_start_json, dependencies_json,
    memory_limit_mb, cpu_limit_millicores, git_token_enc, detected_stack, workspace
)
SELECT
    id, name, repo_url, base_domain, pause_after_seconds, destroy_after_seconds,
    port, dockerfile, build_context, on_start_json, dependencies_json,
    memory_limit_mb, cpu_limit_millicores, git_token_enc, detected_stack, workspace
FROM projects;

DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

PRAGMA foreign_keys = ON;
