-- Project-scoped named tokens (RBAC-lite): `NULL` keeps the token
-- unrestricted (same reach as the master credential), while a JSON array of
-- project ids — e.g. `[1,3]` — limits the token to those projects only.
ALTER TABLE api_tokens ADD COLUMN scoped_projects TEXT;
