-- Which pull request a branch belongs to, and the comment Oxid keeps there.
--
-- A push webhook does not carry a PR number, and the push handler answers
-- before anything is built — so at the moment Oxid learns a branch moved it
-- knows neither the PR nor the URL the preview will get. The association is
-- therefore learned from the `pull_request`/`merge_request` deliveries that
-- already arrive at the same routes and are currently discarded.
--
-- `comment_id` is the one comment Oxid edits in place. A bot that appends on
-- every push is a bot people mute.
CREATE TABLE pull_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    head_branch TEXT NOT NULL,
    head_sha TEXT,
    -- open | closed. A closed PR still gets its comment updated once, so a
    -- merged branch's entry stops advertising a URL that is about to die.
    state TEXT NOT NULL DEFAULT 'open',
    comment_id TEXT,
    updated_at INTEGER NOT NULL,
    UNIQUE (project_id, number)
);

CREATE INDEX idx_pull_requests_branch ON pull_requests (project_id, head_branch, state);

-- Which host a project lives on, learned from the webhook route its first
-- delivery arrived on — nothing in a payload reliably says, and a
-- self-hosted Gitea or GitLab answers at an arbitrary domain.
ALTER TABLE projects ADD COLUMN forge TEXT;
-- Normally derived from `repo_url`'s host; set only for a deployment whose
-- API is not where its forge usually puts it.
ALTER TABLE projects ADD COLUMN forge_api_base TEXT;
-- Separate from `git_token_enc` on purpose. That one may legitimately be a
-- read-only clone token, and it is embedded in an HTTPS clone URL where it
-- can surface in git's own error text. Commenting needs write scope on
-- issues, and quietly requiring every project's clone credential to gain it
-- would be a security regression nobody asked for.
ALTER TABLE projects ADD COLUMN forge_token_enc TEXT;

-- The queue of things to tell the git host.
--
-- Deliberately shaped like `deploy_queue`: persisted so a restart does not
-- lose a notification, with an attempt count and a not-before for backoff.
--
-- The unique index is the rate-limit design. Five pushes in a minute collapse
-- to ONE pending row carrying the latest state, so the queue can never
-- outrun the forge however fast someone pushes.
CREATE TABLE forge_notifications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    state TEXT NOT NULL,
    url TEXT,
    detail TEXT,
    commit_sha TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    not_before INTEGER NOT NULL DEFAULT 0,
    requested_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_forge_notifications_pending
    ON forge_notifications (project_id, branch);
