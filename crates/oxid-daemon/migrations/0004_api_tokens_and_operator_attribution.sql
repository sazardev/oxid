-- Lightweight multi-user support: named, database-managed API tokens on top
-- of the existing single OXID_API_TOKEN "master" credential. Tokens are
-- stored hashed (SHA-256), never in plaintext, matching how a leaked DB dump
-- shouldn't hand out live credentials.
CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);

-- Attributes an audit event to the operator (named token) who triggered it.
-- NULL for the master token or system-initiated events (GC sweeps).
ALTER TABLE audit_events ADD COLUMN operator TEXT;
