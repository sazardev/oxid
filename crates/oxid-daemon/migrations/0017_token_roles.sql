-- Access grows from "a name and maybe some project ids" into a role, a
-- scope and an expiry, so a devops can say what a person may do, where, and
-- for how long — instead of the single level of power every named token had.
--
-- Backfill preserves behaviour exactly, which is the whole point of doing it
-- with explicit values rather than a column default:
--
--   * A token with no project scopes could already do everything on the
--     node, including issuing other tokens. That is `admin`.
--   * A scoped token could do everything *within* its projects — deploy,
--     secrets, settings, deletion. That is `maintainer`, not `developer`:
--     demoting it would silently take away secrets access somebody is
--     relying on today, and an upgrade must never quietly remove a
--     permission.
--
-- Nobody is expired or suspended by an upgrade, so both stay NULL.
ALTER TABLE api_tokens ADD COLUMN role TEXT;
ALTER TABLE api_tokens ADD COLUMN expires_at INTEGER;
ALTER TABLE api_tokens ADD COLUMN suspended_at INTEGER;

UPDATE api_tokens
   SET role = CASE
                WHEN scoped_projects IS NULL OR scoped_projects = '' THEN 'admin'
                ELSE 'maintainer'
              END
 WHERE role IS NULL;
