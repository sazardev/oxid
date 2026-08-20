-- Correlates an audit event with the HTTP request that caused it (the
-- `X-Request-Id` header/response, see `oxid_daemon::api`'s request-id
-- middleware), so an operator can grep structured logs for
-- `request_id=<id>` and cross-reference `SELECT * FROM audit_events WHERE
-- request_id = '<id>'` to see the same operation end-to-end. Nullable: rows
-- written before this migration (and system-initiated events like the GC
-- sweep, which have no originating request) simply have no request id.
ALTER TABLE audit_events ADD COLUMN request_id TEXT;

CREATE INDEX idx_audit_events_request_id ON audit_events(request_id);
