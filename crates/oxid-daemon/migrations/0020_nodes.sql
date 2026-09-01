-- A fleet, instead of a machine.
--
-- Oxid has been one daemon on one server. This does not change that: after
-- this migration there is still exactly one node, it is still this one, and
-- nothing about how an environment is built, routed or reaped is different.
-- What changes is that every environment row now *says* where it lives,
-- which is the prerequisite for the answer ever being "somewhere else"
-- (MULTINODE.md §9, etapa 1).
--
-- The architecture this serves is one control plane over N Docker
-- endpoints, not an agent per node: `endpoint` is what bollard connects to,
-- and the control plane keeps the git cache, the secrets and the audit
-- trail. That is why there is no key material or state here beyond
-- addressing.
CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    -- 'local' is the socket this daemon already uses. Anything else is a
    -- bollard endpoint (`tcp://host:2376`).
    endpoint TEXT NOT NULL,
    -- Where the control plane's proxy dials this node's published ports.
    -- Deliberately distinct from `endpoint`: the Docker API and container
    -- traffic can legitimately live on different interfaces. NULL means
    -- loopback, which is both correct for `local` and exactly what
    -- `service/proxy.rs` dialled before nodes existed.
    address TEXT,
    -- Paths, not bytes. `service/backup.rs` snapshots the SQLite file, so a
    -- restore brings back these rows and *not* the files they point at —
    -- the certificates stay the operator's responsibility.
    tls_ca_path TEXT,
    tls_cert_path TEXT,
    tls_key_path TEXT,
    -- active | draining | down. `draining` refuses new placements but keeps
    -- serving; `down` is set by the health probe and is NEVER propagated to
    -- the environments on it — a partition is indistinguishable from a dead
    -- machine, and evicting on one is how two copies of a branch end up
    -- fighting over a URL.
    state TEXT NOT NULL DEFAULT 'active',
    reserved_memory_mb INTEGER,
    total_memory_bytes INTEGER NOT NULL DEFAULT 0,
    cpu_count INTEGER NOT NULL DEFAULT 0,
    last_seen_at INTEGER,
    created_at INTEGER NOT NULL
);

-- Every existing install is a node, and it is this one. Seeding it here
-- rather than at startup is what lets `environments.node_id` be backfilled
-- in the same migration, so no row is ever nodeless.
INSERT OR IGNORE INTO nodes (id, name, endpoint, state, created_at)
VALUES (1, 'local', 'local', 'active', CAST(strftime('%s', 'now') AS INTEGER));

-- SQLite forbids ADD COLUMN with both REFERENCES and a non-null DEFAULT, so
-- it arrives nullable and is filled immediately. A NULL read afterwards
-- means "written by a binary older than this migration" and resolves to
-- node 1 — the same answer, and one that survives rolling back.
ALTER TABLE environments ADD COLUMN node_id INTEGER REFERENCES nodes(id);
UPDATE environments SET node_id = 1 WHERE node_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_environments_node ON environments(node_id);
