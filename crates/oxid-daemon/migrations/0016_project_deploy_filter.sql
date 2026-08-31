-- `[deploy]` rules: which branches a webhook push may deploy, and how many
-- environments the project may hold.
--
-- These live on the project row rather than being read from the commit like
-- `[build]` and `[routing]`, because the filter has to answer *before* the
-- checkout — the whole point of it is to avoid the fetch and the build for a
-- branch nobody wanted. Reading it from the commit would mean doing the work
-- the filter exists to skip.
--
-- Added as columns with defaults rather than a table rebuild: '[]' and NULL
-- are exactly "every branch, no cap", so every existing project keeps the
-- behaviour it has today.
ALTER TABLE projects ADD COLUMN deploy_branches_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE projects ADD COLUMN deploy_ignore_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE projects ADD COLUMN max_environments INTEGER;
