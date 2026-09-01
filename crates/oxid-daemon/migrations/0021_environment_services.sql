-- An environment is several containers.
--
-- `IDEA.md` promises that a repository with a `docker-compose.yml` just
-- works. It did not: the parser took the first service with a `build:` key
-- and dropped the rest in silence, so an `api` + `worker` + `db` stack
-- deployed the api alone and the app failed at runtime on a connection
-- nobody had said would be missing.
--
-- The scalar columns on `environments` stay exactly as they are and now
-- mean **the primary service** — the one that takes the branch URL. That is
-- what keeps every existing reader (the API type, the dashboard, the CLI,
-- the nine call sites of `resolved_container_name`) compiling *and*
-- meaning the right thing. This table is the rest of the story, consulted
-- only by the paths that must know: deploy, teardown, GC, reconciliation.
CREATE TABLE IF NOT EXISTS environment_services (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    environment_id INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    -- The compose key: `api`, `worker`, `db`. Load-bearing, and the field
    -- the old parser threw away — it is the hostname siblings resolve each
    -- other by inside the environment's network.
    service_name TEXT NOT NULL,
    container_name TEXT NOT NULL,
    image TEXT NOT NULL,
    -- NULL for a service that listens on nothing, which is what a worker is.
    container_port INTEGER,
    host_port INTEGER,
    -- Exactly one per environment. The scalar columns on `environments`
    -- describe this one.
    is_primary INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_env_services_unique
    ON environment_services (environment_id, service_name);
CREATE INDEX IF NOT EXISTS idx_env_services_env
    ON environment_services (environment_id);

-- No backfill, and that is deliberate. Oxid never SQL-deletes an
-- `environments` row (see `0002_resource_leases.sql`), so the cascade above
-- is belt-and-braces rather than the real cleanup — `destroy` deletes these
-- rows explicitly, exactly as it does leases.
--
-- An environment with no rows here is one deployed before this migration,
-- and that means "one service": every reader falls back to
-- `resolved_container_name`, which is the answer that was already true.
