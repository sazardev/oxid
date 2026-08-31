-- Make a queued deploy claimable, atomically.
--
-- The drain is single-flighted by `deploy_drain_lock`, an in-process mutex
-- whose own doc comment admits what happens without it: two drains read the
-- same pending row before either removes it, and the same push deploys
-- twice. An in-process lock can only promise that within one process — and
-- two processes on one data directory is not exotic. An operator restarting
-- a container while the old one is still shutting down has two daemons on
-- the same SQLite file for a few seconds, which is exactly long enough.
--
-- A claim in the database is the only thing that can be right here, because
-- the database is the only thing both processes share.
--
-- All three columns are NULL on existing rows, which reads as "unclaimed" —
-- correct, and no backfill needed.
ALTER TABLE deploy_queue ADD COLUMN claimed_by TEXT;
ALTER TABLE deploy_queue ADD COLUMN claimed_at INTEGER;
-- A lease, not a flag. A worker that dies mid-build must not hold the entry
-- for ever: expiry is what turns a crash into a retry instead of a push
-- that silently never happens.
ALTER TABLE deploy_queue ADD COLUMN lease_expires_at INTEGER;

CREATE INDEX IF NOT EXISTS idx_deploy_queue_claimable
    ON deploy_queue (lease_expires_at, id);
