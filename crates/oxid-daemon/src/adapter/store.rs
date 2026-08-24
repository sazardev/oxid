//! `SQLite` persistence adapter implementing the domain ports
//! (SPEC.md §2.2 "Persistencia").
//!
//! Storage encoding:
//! - timestamps as `INTEGER` unix-seconds,
//! - TTLs as `INTEGER` whole seconds,
//! - enum states as `TEXT` (`Display`/`FromStr`),
//! - variable-length collections (`on_start`, `dependencies`) as JSON.

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use oxid_core::{
    AuditEvent, AuditFilter, AuditStore, Branch, BranchName, BuildConfig, Dependency, DomainError,
    EnvVarScope, Environment, EnvironmentId, EnvironmentState, EnvironmentStore, OffsetDateTime,
    PoolKind, Project, ProjectConfig, ProjectId, ProjectStore, RepoUrl, RepositoryError,
    SecretContext, SecretStore, SecretValue, StateTransition, Ttl,
};

use crate::adapter::crypto::{Cipher, CryptoError};

/// Errors surfaced while opening the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Connection or query failure.
    #[error("database failure: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Embedded migrations could not run.
    #[error("migration failure: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// Secret encryption or decryption failed.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// A connected, migrated `SQLite` database.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
    /// `ArcSwap`, not a plain field, so [`Self::rotate_master_key`] can
    /// atomically swap in a new key with zero downtime — every other clone
    /// of this `SqliteStore` (one per in-flight request) sees the new key
    /// on its very next encrypt/decrypt call, no restart needed.
    cipher: Arc<ArcSwap<Cipher>>,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path` with `WAL` journaling
    /// and foreign keys enabled, then runs the embedded migrations.
    ///
    /// `synchronous = NORMAL` is paired with WAL deliberately, not left at
    /// `SQLite`'s `FULL` default: with WAL, `NORMAL` is the combination
    /// `SQLite`'s own documentation calls safe from corruption (a
    /// power-loss can lose the last commit, never corrupt the file) while
    /// skipping an `fsync` per transaction — the right trade for an audit
    /// trail of ephemeral dev environments, not a system of record.
    ///
    /// # Errors
    /// Returns [`StoreError`] on connection or migration failure.
    pub async fn open(path: impl AsRef<Path>, cipher: Cipher) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);
        Self::open_with(opts, cipher).await
    }

    /// Opens an ephemeral in-memory database (tests) with a fixed test key.
    ///
    /// # Errors
    /// Returns [`StoreError`] on connection or migration failure.
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        Self::open_with(opts, Cipher::from_key([1u8; 32])).await
    }

    /// Writes a consistent point-in-time snapshot of the database to
    /// `dest` via `SQLite`'s `VACUUM INTO` — safe to run against a live,
    /// already-open pool (unlike copying the `.sqlite`/`-wal`/`-shm` files
    /// directly, which can capture an inconsistent mid-write state). Backs
    /// the `GET /api/v1/backup` endpoint.
    ///
    /// # Errors
    /// Returns [`StoreError`] on query failure (e.g. `dest` already exists).
    pub async fn backup_to(&self, dest: &Path) -> Result<(), StoreError> {
        // `VACUUM INTO` doesn't accept a bound parameter for the filename
        // (silently no-ops instead of erroring) — it needs a string literal,
        // hence the manual escaping instead of `.bind(...)`.
        let escaped = dest.to_string_lossy().replace('\'', "''");
        sqlx::query(&format!("VACUUM INTO '{escaped}'"))
            .persistent(false)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Re-encrypts every stored secret under `new_cipher` and atomically
    /// swaps it in — zero downtime, no restart. The re-encryption pass runs
    /// inside one transaction (all rows or none — with `max_connections(1)`
    /// this also blocks out any concurrent secret write until it commits,
    /// so nothing can be written mid-rotation under the wrong key), and
    /// only *after* it commits successfully does the in-memory cipher
    /// change — a failed rotation leaves every secret exactly as it was.
    ///
    /// The caller (the daemon, which alone knows the data directory) still
    /// has to persist `new_cipher`'s key to `secret.key` itself — see
    /// `api.rs`'s `rotate_key` handler.
    ///
    /// # Errors
    /// Returns [`StoreError`] on query or encryption/decryption failure.
    pub async fn rotate_master_key(&self, new_cipher: Cipher) -> Result<(), StoreError> {
        let old_cipher = self.cipher.load_full();
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query("SELECT id, value_enc FROM secrets")
            .fetch_all(&mut *tx)
            .await?;
        for row in rows {
            let id: i64 = row.try_get("id")?;
            let value_enc: String = row.try_get("value_enc")?;
            let plaintext = old_cipher.decrypt(&value_enc)?;
            let re_encrypted = new_cipher.encrypt(&plaintext)?;
            sqlx::query("UPDATE secrets SET value_enc = ? WHERE id = ?")
                .bind(re_encrypted)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        // Also re-encrypts `projects.git_token_enc` — easy to forget since
        // it's a completely separate table from `secrets`, but it's
        // encrypted with the very same cipher and would otherwise become
        // silently undecryptable the moment `self.cipher` below is swapped.
        let project_rows =
            sqlx::query("SELECT id, git_token_enc FROM projects WHERE git_token_enc IS NOT NULL")
                .fetch_all(&mut *tx)
                .await?;
        for row in project_rows {
            let id: i64 = row.try_get("id")?;
            let git_token_enc: String = row.try_get("git_token_enc")?;
            let plaintext = old_cipher.decrypt(&git_token_enc)?;
            let re_encrypted = new_cipher.encrypt(&plaintext)?;
            sqlx::query("UPDATE projects SET git_token_enc = ? WHERE id = ?")
                .bind(re_encrypted)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.cipher.store(Arc::new(new_cipher));
        Ok(())
    }

    /// Sets (or, with `token: None`, clears) a project's git access token —
    /// needed to clone/fetch a private repository, since the daemon's own
    /// git-cache clone is independent of any credential helper the operator's
    /// shell has configured. Encrypted at rest with the same cipher used for
    /// `secrets.value_enc`; deliberately not exposed on the `Project` domain
    /// struct (which is returned wholesale from `GET /api/v1/projects`), so
    /// it's only ever decrypted right before the git operation that needs it.
    ///
    /// # Errors
    /// Returns [`RepositoryError::NotFound`] if the project does not exist.
    pub async fn set_git_token(
        &self,
        project_id: ProjectId,
        token: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let encrypted = token
            .filter(|t| !t.is_empty())
            .map(|t| self.cipher.load().encrypt(t))
            .transpose()
            .map_err(storage)?;
        let res = sqlx::query("UPDATE projects SET git_token_enc = ? WHERE id = ?")
            .bind(encrypted)
            .bind(id_as_i64(project_id.0))
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!(
                "project `{project_id}` does not exist"
            )));
        }
        Ok(())
    }

    /// Decrypts and returns a project's git access token, if one is set.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query or decryption failure.
    pub async fn get_git_token(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<String>, RepositoryError> {
        let row = sqlx::query("SELECT git_token_enc FROM projects WHERE id = ?")
            .bind(id_as_i64(project_id.0))
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let encrypted: Option<String> = row.try_get("git_token_enc").map_err(storage)?;
        encrypted
            .map(|enc| self.cipher.load().decrypt(&enc))
            .transpose()
            .map_err(storage)
    }

    async fn open_with(opts: SqliteConnectOptions, cipher: Cipher) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self {
            pool,
            cipher: Arc::new(ArcSwap::from_pointee(cipher)),
        })
    }

    /// Lists every environment across all projects, regardless of state.
    ///
    /// Used by the garbage-collection sweep, which needs to evaluate idle
    /// environments independently of which project they belong to.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn list_all_environments(&self) -> Result<Vec<Environment>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {ENV_COLUMNS} FROM environments ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(env_from_row).collect()
    }

    /// Finds the most recent environment routed at `url`, if any.
    ///
    /// Backs the wake-on-request and heartbeat endpoints, which only know
    /// the `Host` header Traefik forwards, not an environment id. A branch
    /// redeployed after `oxid down` produces a new row reusing the same
    /// `url`, so this must pick the highest id, not an arbitrary row, or a
    /// stale `Destroyed` deployment could shadow the live one.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn find_by_url(&self, url: &str) -> Result<Option<Environment>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {ENV_COLUMNS} FROM environments WHERE url = ? ORDER BY id DESC LIMIT 1"
        ))
        .bind(url)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        row.as_ref().map(env_from_row).transpose()
    }

    /// Looks up an existing resource lease for (project, branch, kind,
    /// `shared_instance`), if any — leases are reused across redeploys of
    /// the same branch rather than re-provisioned every time.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn find_resource_lease(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
        kind: PoolKind,
        shared_instance: &str,
    ) -> Result<Option<String>, RepositoryError> {
        let row = sqlx::query(
            "SELECT resource_name FROM resource_leases \
             WHERE project_id = ? AND branch = ? AND kind = ? AND shared_instance = ?",
        )
        .bind(id_as_i64(project_id.0))
        .bind(branch.as_str())
        .bind(kind.to_string())
        .bind(shared_instance)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        row.map(|r| r.try_get::<String, _>("resource_name").map_err(storage))
            .transpose()
    }

    /// Every `resource_name` already leased under `shared_instance`+`kind`,
    /// across all branches — used to pick a free Redis index (there's no
    /// `CREATE DATABASE`-equivalent pre-check for Redis, so this is the only
    /// source of truth for "is this slot free").
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn used_resource_names(
        &self,
        kind: PoolKind,
        shared_instance: &str,
    ) -> Result<Vec<String>, RepositoryError> {
        let rows = sqlx::query(
            "SELECT resource_name FROM resource_leases WHERE kind = ? AND shared_instance = ?",
        )
        .bind(kind.to_string())
        .bind(shared_instance)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| r.try_get::<String, _>("resource_name").map_err(storage))
            .collect()
    }

    /// Records a new resource lease.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn create_resource_lease(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
        kind: PoolKind,
        shared_instance: &str,
        resource_name: &str,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO resource_leases \
             (project_id, branch, kind, shared_instance, resource_name, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id_as_i64(project_id.0))
        .bind(branch.as_str())
        .bind(kind.to_string())
        .bind(shared_instance)
        .bind(resource_name)
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    /// Deletes and returns every resource lease held by (project, branch) —
    /// used when the branch is destroyed, so its Postgres database can be
    /// dropped and its Redis index freed. Returns `(kind, resource_name)`
    /// pairs.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn take_resource_leases(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
    ) -> Result<Vec<(PoolKind, String)>, RepositoryError> {
        let rows = sqlx::query(
            "DELETE FROM resource_leases WHERE project_id = ? AND branch = ? \
             RETURNING kind, resource_name",
        )
        .bind(id_as_i64(project_id.0))
        .bind(branch.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| {
                let kind: String = r.try_get("kind").map_err(storage)?;
                let kind = kind.parse::<PoolKind>().map_err(storage)?;
                let resource_name: String = r.try_get("resource_name").map_err(storage)?;
                Ok((kind, resource_name))
            })
            .collect()
    }

    /// Creates a new named API token, storing only its `SHA-256` hash —
    /// the raw token is returned once here and never persisted or
    /// retrievable again, same convention as a password. `scoped_projects`
    /// (`Some` list, never empty) limits the token to those projects;
    /// `None` leaves it unrestricted.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn create_api_token(
        &self,
        name: &str,
        token_hash: &str,
        scoped_projects: Option<&[u64]>,
    ) -> Result<u64, RepositoryError> {
        let scopes = scoped_projects
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| storage(format!("cannot serialize token scopes: {e}")))?;
        let row = sqlx::query(
            "INSERT INTO api_tokens (name, token_hash, created_at, scoped_projects) \
             VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(name)
        .bind(token_hash)
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .bind(scopes)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let id: i64 = row.try_get("id").map_err(storage)?;
        u64::try_from(id).map_err(|_| storage("token id overflowed u64"))
    }

    /// Looks up the operator a token belongs to (name + project scopes), if
    /// it exists and hasn't been revoked. Backs bearer-token authentication
    /// for named tokens.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn find_operator_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<OperatorIdentity>, RepositoryError> {
        let row = sqlx::query(
            "SELECT name, scoped_projects FROM api_tokens \
             WHERE token_hash = ? AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        row.map(|r| {
            let name: String = r.try_get("name").map_err(storage)?;
            let scopes: Option<String> = r.try_get("scoped_projects").map_err(storage)?;
            let scoped_projects = scopes
                .map(|s| serde_json::from_str::<Vec<u64>>(&s))
                .transpose()
                .map_err(|e| storage(format!("corrupt scope list on api token `{name}`: {e}")))?;
            Ok(OperatorIdentity {
                name,
                scoped_projects,
            })
        })
        .transpose()
    }

    /// Lists every token (including revoked ones), newest first — never
    /// includes the raw token or its hash.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn list_api_tokens(&self) -> Result<Vec<ApiTokenSummary>, RepositoryError> {
        let rows = sqlx::query(
            "SELECT id, name, created_at, revoked_at, scoped_projects \
             FROM api_tokens ORDER BY id DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| {
                let id: i64 = r.try_get("id").map_err(storage)?;
                let name: String = r.try_get("name").map_err(storage)?;
                let created_at = ts_from_row(r, "created_at")?;
                let revoked_at: Option<i64> = r.try_get("revoked_at").map_err(storage)?;
                let scopes: Option<String> = r.try_get("scoped_projects").map_err(storage)?;
                let scoped_projects = scopes
                    .map(|s| serde_json::from_str::<Vec<u64>>(&s))
                    .transpose()
                    .map_err(|e| storage(format!("corrupt scope list on token {id}: {e}")))?;
                Ok(ApiTokenSummary {
                    id: u64::try_from(id).map_err(|_| storage("token id overflowed u64"))?,
                    name,
                    created_at,
                    revoked: revoked_at.is_some(),
                    scoped_projects,
                })
            })
            .collect()
    }

    /// Marks a token revoked (idempotent — revoking twice is a no-op, not
    /// an error).
    ///
    /// # Errors
    /// Returns [`RepositoryError::NotFound`] if no token with that id exists.
    pub async fn revoke_api_token(&self, id: u64) -> Result<(), RepositoryError> {
        let res =
            sqlx::query("UPDATE api_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
                .bind(OffsetDateTime::now_utc().unix_timestamp())
                .bind(id_as_i64(id))
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            // Already revoked or never existed — distinguish so callers get
            // a real 404 for a bogus id instead of a silent success.
            let exists = sqlx::query("SELECT 1 FROM api_tokens WHERE id = ?")
                .bind(id_as_i64(id))
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx)?
                .is_some();
            if !exists {
                return Err(RepositoryError::NotFound(format!("api token `{id}`")));
            }
        }
        Ok(())
    }

    /// Queues a deploy that didn't currently fit in host capacity.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn enqueue_deploy(
        &self,
        project_id: ProjectId,
        branch: &BranchName,
        operator: Option<&str>,
    ) -> Result<u64, RepositoryError> {
        let row = sqlx::query(
            "INSERT INTO deploy_queue (project_id, branch, operator, requested_at) \
             VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(id_as_i64(project_id.0))
        .bind(branch.as_str())
        .bind(operator)
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let id: i64 = row.try_get("id").map_err(storage)?;
        u64::try_from(id).map_err(|_| storage("queue id overflowed u64"))
    }

    /// Lists queued deploys oldest-first — the order they're retried in.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn list_deploy_queue(&self) -> Result<Vec<QueuedDeploy>, RepositoryError> {
        let rows = sqlx::query(
            "SELECT id, project_id, branch, operator, requested_at \
             FROM deploy_queue ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| {
                let id: i64 = r.try_get("id").map_err(storage)?;
                let project_id: i64 = r.try_get("project_id").map_err(storage)?;
                let branch: String = r.try_get("branch").map_err(storage)?;
                let operator: Option<String> = r.try_get("operator").map_err(storage)?;
                let requested_at = ts_from_row(r, "requested_at")?;
                Ok(QueuedDeploy {
                    id: u64::try_from(id).map_err(|_| storage("queue id overflowed u64"))?,
                    project_id: ProjectId(
                        u64::try_from(project_id)
                            .map_err(|_| storage("project id overflowed u64"))?,
                    ),
                    branch,
                    operator,
                    requested_at,
                })
            })
            .collect()
    }

    /// Removes a queued deploy — called once it's either been dequeued for
    /// a retry attempt or its project was deleted out from under it.
    ///
    /// # Errors
    /// Returns [`RepositoryError`] on query failure.
    pub async fn remove_from_deploy_queue(&self, id: u64) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM deploy_queue WHERE id = ?")
            .bind(id_as_i64(id))
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }
}

/// A deploy request queued because it didn't fit host capacity at request
/// time.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueuedDeploy {
    /// Queue entry id (used to remove it once retried).
    pub id: u64,
    /// Project to deploy.
    pub project_id: ProjectId,
    /// Branch to deploy.
    pub branch: String,
    /// Operator who requested it, if authenticated with a named token.
    pub operator: Option<String>,
    /// When it was queued.
    pub requested_at: OffsetDateTime,
}

/// Non-secret view of an `api_tokens` row (no hash, no raw token).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiTokenSummary {
    /// Token id (used to revoke it).
    pub id: u64,
    /// Human-readable name chosen at creation (e.g. an operator's username).
    pub name: String,
    /// When the token was created.
    pub created_at: OffsetDateTime,
    /// Whether the token has been revoked.
    pub revoked: bool,
    /// Project ids this token is scoped to, or `None` when it has the same
    /// reach as the master credential. An empty list means "no projects",
    /// which creation rejects — it exists here only as a safe-direction
    /// interpretation of a corrupt row.
    pub scoped_projects: Option<Vec<u64>>,
}

/// What a named API token resolves to at authentication time: who it
/// belongs to and which projects it may touch.
#[derive(Debug, Clone)]
pub struct OperatorIdentity {
    /// Human-readable name audit events are attributed to.
    pub name: String,
    /// `None` = unrestricted (full access, like the master credential);
    /// `Some(ids)` = limited to those projects — every other project is
    /// answered with `404` so its existence isn't revealed.
    pub scoped_projects: Option<Vec<u64>>,
}

// ---------------------------------------------------------------------------
// error mapping helpers
// ---------------------------------------------------------------------------

fn storage(err: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Storage(err.to_string())
}

fn validation(err: &DomainError) -> RepositoryError {
    RepositoryError::Storage(err.to_string())
}

fn map_sqlx(err: sqlx::Error) -> RepositoryError {
    match err {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            RepositoryError::Conflict(db.to_string())
        }
        e => RepositoryError::Storage(e.to_string()),
    }
}

fn id_as_i64(id: u64) -> i64 {
    i64::try_from(id).expect("oxid record ids are positive and fit in i64")
}

fn id_from_row(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<u64, RepositoryError> {
    let v: i64 = row.try_get(column).map_err(storage)?;
    u64::try_from(v).map_err(|_| storage(format!("column `{column}` overflowed u64")))
}

fn ts(dt: &OffsetDateTime) -> i64 {
    dt.unix_timestamp()
}

fn ts_from_row(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<OffsetDateTime, RepositoryError> {
    let v: i64 = row.try_get(column).map_err(storage)?;
    OffsetDateTime::from_unix_timestamp(v)
        .map_err(|_| storage(format!("column `{column}` is not a valid unix timestamp")))
}

// ---------------------------------------------------------------------------
// project mapping
// ---------------------------------------------------------------------------

fn project_to_binds(project: &Project) -> ProjectBinds<'_> {
    ProjectBinds {
        name: &project.name,
        repo_url: project.repo_url.as_str(),
        base_domain: &project.config.base_domain,
        pause_after: project.config.pause_after.whole_seconds(),
        destroy_after: project.config.destroy_after.whole_seconds(),
        port: i64::from(project.config.port),
        dockerfile: project.config.build.dockerfile.as_deref(),
        build_context: &project.config.build.context,
        on_start_json: serde_json::to_string(&project.config.build.on_start)
            .expect("serializing Vec<String> cannot fail"),
        dependencies_json: serde_json::to_string(&project.config.dependencies)
            .expect("serializing Vec<Dependency> cannot fail"),
        memory_limit_mb: project.config.build.memory_limit_mb.map(u64::cast_signed),
        cpu_limit_millicores: project.config.build.cpu_limit_millicores.map(i64::from),
    }
}

struct ProjectBinds<'a> {
    name: &'a str,
    repo_url: &'a str,
    base_domain: &'a str,
    pause_after: i64,
    destroy_after: i64,
    port: i64,
    dockerfile: Option<&'a str>,
    build_context: &'a str,
    on_start_json: String,
    dependencies_json: String,
    memory_limit_mb: Option<i64>,
    cpu_limit_millicores: Option<i64>,
}

const PROJECT_COLUMNS: &str = "id, name, repo_url, base_domain, pause_after_seconds, \
     destroy_after_seconds, port, dockerfile, build_context, on_start_json, dependencies_json, \
     memory_limit_mb, cpu_limit_millicores";

const PROJECT_COLUMNS_NO_ID: &str = "name, repo_url, base_domain, pause_after_seconds, \
     destroy_after_seconds, port, dockerfile, build_context, on_start_json, dependencies_json, \
     memory_limit_mb, cpu_limit_millicores";

fn project_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Project, RepositoryError> {
    let id = id_from_row(row, "id")?;
    let name: String = row.try_get("name").map_err(storage)?;
    let repo_url: String = row.try_get("repo_url").map_err(storage)?;
    let base_domain: String = row.try_get("base_domain").map_err(storage)?;
    let pause_after: i64 = row.try_get("pause_after_seconds").map_err(storage)?;
    let destroy_after: i64 = row.try_get("destroy_after_seconds").map_err(storage)?;
    let port: i64 = row.try_get("port").map_err(storage)?;
    let dockerfile: Option<String> = row.try_get("dockerfile").map_err(storage)?;
    let build_context: String = row.try_get("build_context").map_err(storage)?;
    let on_start_json: String = row.try_get("on_start_json").map_err(storage)?;
    let dependencies_json: String = row.try_get("dependencies_json").map_err(storage)?;
    let memory_limit_mb: Option<i64> = row.try_get("memory_limit_mb").map_err(storage)?;
    let cpu_limit_millicores: Option<i64> = row.try_get("cpu_limit_millicores").map_err(storage)?;

    let on_start: Vec<String> =
        serde_json::from_str(&on_start_json).map_err(|e| storage(e.to_string()))?;
    let dependencies: Vec<Dependency> =
        serde_json::from_str(&dependencies_json).map_err(|e| storage(e.to_string()))?;

    let build = BuildConfig {
        dockerfile,
        context: build_context,
        on_start,
        memory_limit_mb: memory_limit_mb.map(i64::cast_unsigned),
        cpu_limit_millicores: cpu_limit_millicores
            .map(|v| u32::try_from(v).map_err(|_| storage("cpu_limit_millicores overflowed u32")))
            .transpose()?,
    };
    let config = ProjectConfig::new(
        base_domain,
        Ttl::from_seconds(pause_after).map_err(|e| validation(&e))?,
        Ttl::from_seconds(destroy_after).map_err(|e| validation(&e))?,
        u16::try_from(port).map_err(|_| storage("port overflowed u16"))?,
        build,
        dependencies,
    )
    .map_err(|e| validation(&e))?;
    let repo_url = RepoUrl::parse(repo_url).map_err(|e| validation(&e))?;

    Project::new(ProjectId(id), name, repo_url, config).map_err(|e| validation(&e))
}

impl ProjectStore for SqliteStore {
    async fn create(&self, project: &Project) -> Result<ProjectId, RepositoryError> {
        let binds = project_to_binds(project);
        let row = sqlx::query(&format!(
            "INSERT INTO projects ({PROJECT_COLUMNS_NO_ID}) VALUES \
             (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"
        ))
        .bind(binds.name)
        .bind(binds.repo_url)
        .bind(binds.base_domain)
        .bind(binds.pause_after)
        .bind(binds.destroy_after)
        .bind(binds.port)
        .bind(binds.dockerfile)
        .bind(binds.build_context)
        .bind(binds.on_start_json)
        .bind(binds.dependencies_json)
        .bind(binds.memory_limit_mb)
        .bind(binds.cpu_limit_millicores)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let id: i64 = row.try_get("id").map_err(storage)?;
        Ok(ProjectId(
            u64::try_from(id).map_err(|_| storage("project id overflowed u64"))?,
        ))
    }

    async fn get(&self, id: ProjectId) -> Result<Option<Project>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {PROJECT_COLUMNS} FROM projects WHERE id = ?"
        ))
        .bind(id_as_i64(id.0))
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        row.as_ref().map(project_from_row).transpose()
    }

    async fn list(&self) -> Result<Vec<Project>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {PROJECT_COLUMNS} FROM projects ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(project_from_row).collect()
    }

    async fn delete(&self, id: ProjectId) -> Result<(), RepositoryError> {
        let res = sqlx::query("DELETE FROM projects WHERE id = ?")
            .bind(id_as_i64(id.0))
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!(
                "project `{id}` does not exist"
            )));
        }
        Ok(())
    }

    async fn update(&self, project: &Project) -> Result<(), RepositoryError> {
        let binds = project_to_binds(project);
        let res = sqlx::query(
            "UPDATE projects SET name = ?, base_domain = ?, pause_after_seconds = ?, \
             destroy_after_seconds = ?, port = ?, dockerfile = ?, build_context = ?, \
             on_start_json = ?, dependencies_json = ?, memory_limit_mb = ?, \
             cpu_limit_millicores = ? WHERE id = ?",
        )
        .bind(binds.name)
        .bind(binds.base_domain)
        .bind(binds.pause_after)
        .bind(binds.destroy_after)
        .bind(binds.port)
        .bind(binds.dockerfile)
        .bind(binds.build_context)
        .bind(binds.on_start_json)
        .bind(binds.dependencies_json)
        .bind(binds.memory_limit_mb)
        .bind(binds.cpu_limit_millicores)
        .bind(id_as_i64(project.id.0))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!(
                "project `{}` does not exist",
                project.id
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// environment mapping
// ---------------------------------------------------------------------------

const ENV_COLUMNS: &str = "id, project_id, branch_name, commit_sha, state, url, \
     created_at, updated_at, last_accessed_at, host_port, public_port, container_name";

const ENV_COLUMNS_NO_ID: &str = "project_id, branch_name, commit_sha, state, url, \
     created_at, updated_at, last_accessed_at, host_port, public_port, container_name";

fn env_to_binds(env: &Environment) -> EnvBinds<'_> {
    EnvBinds {
        id: id_as_i64(env.id.0),
        project_id: id_as_i64(env.project_id.0),
        branch_name: env.branch.name.as_str(),
        commit_sha: &env.branch.commit_sha,
        state: env.state.to_string(),
        url: &env.url,
        created_at: ts(&env.created_at),
        updated_at: ts(&env.updated_at),
        last_accessed_at: ts(&env.last_accessed_at),
        host_port: env.host_port.map(i64::from),
        public_port: env.public_port.map(i64::from),
        container_name: env.container_name.as_deref(),
    }
}

struct EnvBinds<'a> {
    id: i64,
    project_id: i64,
    branch_name: &'a str,
    commit_sha: &'a str,
    state: String,
    url: &'a str,
    created_at: i64,
    updated_at: i64,
    last_accessed_at: i64,
    host_port: Option<i64>,
    public_port: Option<i64>,
    container_name: Option<&'a str>,
}

fn env_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Environment, RepositoryError> {
    let id = id_from_row(row, "id")?;
    let project_id = id_from_row(row, "project_id")?;
    let branch_name: String = row.try_get("branch_name").map_err(storage)?;
    let commit_sha: String = row.try_get("commit_sha").map_err(storage)?;
    let state: String = row.try_get("state").map_err(storage)?;
    let url: String = row.try_get("url").map_err(storage)?;
    let created_at = ts_from_row(row, "created_at")?;
    let updated_at = ts_from_row(row, "updated_at")?;
    let last_accessed_at = ts_from_row(row, "last_accessed_at")?;
    let host_port: Option<i64> = row.try_get("host_port").map_err(storage)?;
    let public_port: Option<i64> = row.try_get("public_port").map_err(storage)?;
    let container_name: Option<String> = row.try_get("container_name").map_err(storage)?;

    let branch = Branch::new(
        BranchName::parse(branch_name).map_err(|e| validation(&e))?,
        commit_sha,
    )
    .map_err(|e| validation(&e))?;
    let state = state
        .parse::<EnvironmentState>()
        .map_err(|e| validation(&e))?;

    let mut env = Environment::new(
        EnvironmentId(id),
        ProjectId(project_id),
        branch,
        state,
        url,
        OffsetDateTime::UNIX_EPOCH,
    )
    .map_err(|e| validation(&e))?;
    env.created_at = created_at;
    env.updated_at = updated_at;
    env.last_accessed_at = last_accessed_at;
    env.host_port = host_port.and_then(|p| u16::try_from(p).ok());
    env.public_port = public_port.and_then(|p| u16::try_from(p).ok());
    env.container_name = container_name;
    Ok(env)
}

impl EnvironmentStore for SqliteStore {
    async fn create(&self, env: &Environment) -> Result<EnvironmentId, RepositoryError> {
        let binds = env_to_binds(env);
        let row = sqlx::query(&format!(
            "INSERT INTO environments ({ENV_COLUMNS_NO_ID}) VALUES \
             (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"
        ))
        .bind(binds.project_id)
        .bind(binds.branch_name)
        .bind(binds.commit_sha)
        .bind(binds.state)
        .bind(binds.url)
        .bind(binds.created_at)
        .bind(binds.updated_at)
        .bind(binds.last_accessed_at)
        .bind(binds.host_port)
        .bind(binds.public_port)
        .bind(binds.container_name)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let id: i64 = row.try_get("id").map_err(storage)?;
        Ok(EnvironmentId(
            u64::try_from(id).map_err(|_| storage("environment id overflowed u64"))?,
        ))
    }

    async fn get(&self, id: EnvironmentId) -> Result<Option<Environment>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {ENV_COLUMNS} FROM environments WHERE id = ?"
        ))
        .bind(id_as_i64(id.0))
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        row.as_ref().map(env_from_row).transpose()
    }

    async fn list_by_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Environment>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {ENV_COLUMNS} FROM environments WHERE project_id = ? ORDER BY id"
        ))
        .bind(id_as_i64(project_id.0))
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(env_from_row).collect()
    }

    async fn list_by_state(
        &self,
        state: EnvironmentState,
    ) -> Result<Vec<Environment>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {ENV_COLUMNS} FROM environments WHERE state = ? ORDER BY id"
        ))
        .bind(state.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(env_from_row).collect()
    }

    async fn update(&self, env: &Environment) -> Result<(), RepositoryError> {
        let binds = env_to_binds(env);
        let res = sqlx::query(
            "UPDATE environments SET project_id = ?, branch_name = ?, commit_sha = ?, \
             state = ?, url = ?, created_at = ?, updated_at = ?, last_accessed_at = ?, \
             host_port = ?, public_port = ?, container_name = ? WHERE id = ?",
        )
        .bind(binds.project_id)
        .bind(binds.branch_name)
        .bind(binds.commit_sha)
        .bind(binds.state)
        .bind(binds.url)
        .bind(binds.created_at)
        .bind(binds.updated_at)
        .bind(binds.last_accessed_at)
        .bind(binds.host_port)
        .bind(binds.public_port)
        .bind(binds.container_name)
        .bind(binds.id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!(
                "environment `{}` does not exist",
                binds.id
            )));
        }
        Ok(())
    }

    async fn delete(&self, id: EnvironmentId) -> Result<(), RepositoryError> {
        let res = sqlx::query("DELETE FROM environments WHERE id = ?")
            .bind(id_as_i64(id.0))
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!(
                "environment `{id}` does not exist"
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// audit mapping
// ---------------------------------------------------------------------------

const AUDIT_COLUMNS: &str = "audit_events.id, audit_events.environment_id, audit_events.kind, \
    audit_events.detail, audit_events.occurred_at, audit_events.operator, \
    audit_events.request_id";

/// Fallback for [`AuditFilter::limit`] when unset — matches the API layer's
/// own `DEFAULT_AUDIT_LIMIT` (kept in sync manually; see `api.rs`).
const DEFAULT_LIST_RECENT_LIMIT: u64 = 50;

fn audit_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<AuditEvent, RepositoryError> {
    let id = id_from_row(row, "id")?;
    let environment_id = id_from_row(row, "environment_id")?;
    let kind: String = row.try_get("kind").map_err(storage)?;
    let detail: Option<String> = row.try_get("detail").map_err(storage)?;
    let occurred_at = ts_from_row(row, "occurred_at")?;
    let operator: Option<String> = row.try_get("operator").map_err(storage)?;
    let request_id: Option<String> = row.try_get("request_id").map_err(storage)?;

    Ok(AuditEvent::with_operator(
        id,
        EnvironmentId(environment_id),
        kind.parse::<StateTransition>()
            .map_err(|e| validation(&e))?,
        detail,
        occurred_at,
        operator,
    )
    .with_request_id(request_id))
}

/// Appends `AND audit_events.occurred_at >= ?`/`<= ?`/`AND
/// audit_events.kind = ?` to `qb` for whichever of `filter.since`/
/// `filter.until`/`filter.kind` are set — shared by both `AuditStore`
/// methods below, since both accept the same three fields.
fn push_common_audit_filters<'a>(
    qb: &mut sqlx::QueryBuilder<'a, sqlx::Sqlite>,
    filter: &'a AuditFilter,
) {
    if let Some(since) = filter.since {
        qb.push(" AND audit_events.occurred_at >= ");
        qb.push_bind(ts(&since));
    }
    if let Some(until) = filter.until {
        qb.push(" AND audit_events.occurred_at <= ");
        qb.push_bind(ts(&until));
    }
    if let Some(kind) = filter.kind {
        qb.push(" AND audit_events.kind = ");
        qb.push_bind(kind.to_string());
    }
}

impl AuditStore for SqliteStore {
    async fn record(&self, event: &AuditEvent) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO audit_events (environment_id, kind, detail, occurred_at, operator, request_id) \
                     VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id_as_i64(event.environment_id.0))
        .bind(event.kind.to_string())
        .bind(event.detail.as_deref())
        .bind(ts(&event.occurred_at))
        .bind(event.operator.as_deref())
        .bind(event.request_id.as_deref())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn list_by_environment(
        &self,
        environment_id: EnvironmentId,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEvent>, RepositoryError> {
        // `project_id`/`branch`/`limit` are deliberately ignored here — an
        // environment is already scoped to exactly one project/branch (see
        // `AuditFilter`'s doc comment), and this endpoint has always
        // returned an environment's *full* history, not a page of it.
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_events WHERE audit_events.environment_id = "
        ));
        qb.push_bind(id_as_i64(environment_id.0));
        push_common_audit_filters(&mut qb, filter);
        qb.push(" ORDER BY audit_events.occurred_at ASC, audit_events.id ASC");
        let rows = qb.build().fetch_all(&self.pool).await.map_err(map_sqlx)?;
        rows.iter().map(audit_from_row).collect()
    }

    async fn list_recent(&self, filter: &AuditFilter) -> Result<Vec<AuditEvent>, RepositoryError> {
        // `project_id`/`branch` need a join against `environments` — an
        // audit event only carries `environment_id`.
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_events \
             JOIN environments ON environments.id = audit_events.environment_id WHERE 1 = 1"
        ));
        if let Some(project_id) = filter.project_id {
            qb.push(" AND environments.project_id = ");
            qb.push_bind(id_as_i64(project_id.0));
        }
        if let Some(branch) = &filter.branch {
            qb.push(" AND environments.branch_name = ");
            qb.push_bind(branch.clone());
        }
        push_common_audit_filters(&mut qb, filter);
        qb.push(" ORDER BY audit_events.occurred_at DESC, audit_events.id DESC LIMIT ");
        let limit = filter.limit.unwrap_or(DEFAULT_LIST_RECENT_LIMIT);
        qb.push_bind(i64::try_from(limit).unwrap_or(i64::MAX));
        let rows = qb.build().fetch_all(&self.pool).await.map_err(map_sqlx)?;
        rows.iter().map(audit_from_row).collect()
    }
}

// ---------------------------------------------------------------------------
// secret mapping
// ---------------------------------------------------------------------------

impl SecretStore for SqliteStore {
    async fn set_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
        scope: EnvVarScope,
        value: &SecretValue,
    ) -> Result<(), RepositoryError> {
        let value_enc = self
            .cipher
            .load()
            .encrypt(value.as_str())
            .map_err(storage)?;
        let project_bind = project_id.map(|id| id_as_i64(id.0));
        let branch_bind = branch.map(oxid_core::BranchName::as_str);
        let now = OffsetDateTime::now_utc().unix_timestamp();

        sqlx::query(
            "INSERT INTO secrets (project_id, branch, name, scope, value_enc, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT DO UPDATE SET scope = excluded.scope, \
             value_enc = excluded.value_enc, updated_at = excluded.updated_at",
        )
        .bind(project_bind)
        .bind(branch_bind)
        .bind(name)
        .bind(scope.to_string())
        .bind(value_enc)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn secrets_for(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
    ) -> Result<SecretContext, RepositoryError> {
        let project_bind = project_id.map(|id| id_as_i64(id.0));
        let branch_bind = branch.map(oxid_core::BranchName::as_str);
        let rows = sqlx::query(&format!(
            "SELECT {SECRET_COLUMNS} FROM secrets WHERE {SECRET_CONTEXT_FILTER} ORDER BY name"
        ))
        .bind(project_bind)
        .bind(project_bind)
        .bind(branch_bind)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        // `SecretContext::set` is a raw, precedence-blind upsert (by design,
        // callers like tests build single-scope contexts with it). Rows here
        // come back in an incidental SQL order, not scope-priority order, so
        // folding them in directly with `set` would let whichever row SQLite
        // happens to return last for a name win — not necessarily the most
        // specific scope. `merge` applies the actual
        // Global -> Project -> Branch precedence rule per key.
        let mut ctx = SecretContext::new();
        for row in rows {
            let name: String = row.try_get("name").map_err(storage)?;
            let scope: String = row.try_get("scope").map_err(storage)?;
            let scope = scope.parse::<EnvVarScope>().map_err(storage)?;
            let value_enc: String = row.try_get("value_enc").map_err(storage)?;
            let value = self.cipher.load().decrypt(&value_enc).map_err(storage)?;
            let mut single = SecretContext::new();
            single.set(name, scope, SecretValue::new(value));
            ctx = ctx.merge([single]);
        }
        Ok(ctx)
    }

    async fn list_secrets(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
    ) -> Result<Vec<(String, EnvVarScope)>, RepositoryError> {
        let project_bind = project_id.map(|id| id_as_i64(id.0));
        let branch_bind = branch.map(oxid_core::BranchName::as_str);
        let rows = sqlx::query(&format!(
            "SELECT name, scope FROM secrets WHERE {SECRET_CONTEXT_FILTER} ORDER BY name"
        ))
        .bind(project_bind)
        .bind(project_bind)
        .bind(branch_bind)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.iter()
            .map(|row| {
                let name: String = row.try_get("name").map_err(storage)?;
                let scope: String = row.try_get("scope").map_err(storage)?;
                let scope = scope.parse::<EnvVarScope>().map_err(storage)?;
                Ok((name, scope))
            })
            .collect()
    }

    async fn delete_secret(
        &self,
        project_id: Option<ProjectId>,
        branch: Option<&BranchName>,
        name: &str,
    ) -> Result<(), RepositoryError> {
        let project_bind = project_id.map(|id| id_as_i64(id.0));
        let branch_bind = branch.map(oxid_core::BranchName::as_str);
        let res =
            sqlx::query("DELETE FROM secrets WHERE project_id IS ? AND branch IS ? AND name = ?")
                .bind(project_bind)
                .bind(branch_bind)
                .bind(name)
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!(
                "secret `{name}` does not exist in this scope"
            )));
        }
        Ok(())
    }
}

/// Filter matching global (`project_id IS NULL`), project-scope (`branch IS
/// NULL`) and this-branch-only secrets for a context. Binds: project id
/// (twice), then branch name.
///
/// This used to be `project_id IS NULL OR project_id = ? OR (project_id = ?
/// AND (branch IS NULL OR branch = ?))`: the middle `project_id = ?` matched
/// *every* row for the project on its own, branch-scoped or not, making the
/// trailing `branch = ?` check dead code. In practice this leaked another
/// branch's branch-scoped secret into every other branch's deploy of the
/// same project whenever both defined a secret with the same name — found by
/// deploying two branches with same-named branch-scoped secrets and seeing
/// one branch's container receive the other's value.
const SECRET_CONTEXT_FILTER: &str = "project_id IS NULL OR \
     (project_id = ? AND branch IS NULL) OR (project_id = ? AND branch = ?)";

const SECRET_COLUMNS: &str = "name, scope, value_enc";

#[cfg(test)]
mod tests {
    use super::*;
    use oxid_core::{
        AuditFilter, AuditStore, EnvironmentStore, PoolKind, ProjectStore, StateTransition,
    };

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[tokio::test]
    async fn backup_to_writes_a_real_sqlite_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.sqlite");
        let store = SqliteStore::open(&src, Cipher::from_key([1u8; 32]))
            .await
            .unwrap();
        let dest = dir.path().join("snapshot.sqlite");
        store.backup_to(&dest).await.unwrap();
        assert!(dest.exists(), "expected {} to exist", dest.display());
    }

    #[tokio::test]
    async fn rotate_master_key_re_encrypts_and_swaps_with_no_downtime() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        SecretStore::set_secret(
            &store,
            None,
            None,
            "DB_PASSWORD",
            EnvVarScope::Global,
            &SecretValue::new("hunter2".to_owned()),
        )
        .await
        .unwrap();

        let old_value_enc: String = sqlx::query("SELECT value_enc FROM secrets")
            .fetch_one(&store.pool)
            .await
            .unwrap()
            .try_get("value_enc")
            .unwrap();

        let new_cipher = Cipher::from_key([42u8; 32]);
        store.rotate_master_key(new_cipher).await.unwrap();

        // Still readable through the store (now under the new key).
        let ctx = SecretStore::secrets_for(&store, None, None).await.unwrap();
        assert_eq!(
            ctx.resolved_map().get("DB_PASSWORD").unwrap().as_str(),
            "hunter2"
        );

        // The raw ciphertext on disk actually changed — this isn't just an
        // in-memory swap with stale data left behind.
        let new_value_enc: String = sqlx::query("SELECT value_enc FROM secrets")
            .fetch_one(&store.pool)
            .await
            .unwrap()
            .try_get("value_enc")
            .unwrap();
        assert_ne!(old_value_enc, new_value_enc);

        // The old key genuinely can't decrypt it anymore.
        let old_cipher = Cipher::from_key([1u8; 32]);
        assert!(old_cipher.decrypt(&new_value_enc).is_err());
    }

    #[tokio::test]
    async fn git_token_round_trips_encrypted_and_clears() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let project = project(1);
        let id = ProjectStore::create(&store, &project).await.unwrap();

        assert_eq!(store.get_git_token(id).await.unwrap(), None);

        store
            .set_git_token(id, Some("ghp_secret123"))
            .await
            .unwrap();
        assert_eq!(
            store.get_git_token(id).await.unwrap(),
            Some("ghp_secret123".to_owned())
        );

        // Stored encrypted, not in plaintext.
        let raw: Option<String> = sqlx::query("SELECT git_token_enc FROM projects WHERE id = ?")
            .bind(id_as_i64(id.0))
            .fetch_one(&store.pool)
            .await
            .unwrap()
            .try_get("git_token_enc")
            .unwrap();
        assert_ne!(raw.as_deref(), Some("ghp_secret123"));

        // Clearing it (empty string, matching the API's "empty clears"
        // convention) removes it entirely.
        store.set_git_token(id, Some("")).await.unwrap();
        assert_eq!(store.get_git_token(id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rotate_master_key_also_re_encrypts_project_git_tokens() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let project = project(1);
        let id = ProjectStore::create(&store, &project).await.unwrap();
        store
            .set_git_token(id, Some("ghp_secret123"))
            .await
            .unwrap();

        store
            .rotate_master_key(Cipher::from_key([42u8; 32]))
            .await
            .unwrap();

        // Still readable through the store under the new key.
        assert_eq!(
            store.get_git_token(id).await.unwrap(),
            Some("ghp_secret123".to_owned())
        );

        // The old key genuinely can't decrypt it anymore.
        let raw: String = sqlx::query("SELECT git_token_enc FROM projects WHERE id = ?")
            .bind(id_as_i64(id.0))
            .fetch_one(&store.pool)
            .await
            .unwrap()
            .try_get("git_token_enc")
            .unwrap();
        let old_cipher = Cipher::from_key([1u8; 32]);
        assert!(old_cipher.decrypt(&raw).is_err());
    }

    fn project(id: u64) -> Project {
        let config = ProjectConfig::new(
            "app.local.dev",
            Ttl::parse("30m").unwrap(),
            Ttl::parse("7d").unwrap(),
            8080,
            BuildConfig {
                dockerfile: Some("Dockerfile.dev".to_owned()),
                context: "deploy".to_owned(),
                on_start: vec!["db:migrate".to_owned()],
                memory_limit_mb: Some(256),
                cpu_limit_millicores: Some(500),
            },
            vec![Dependency {
                kind: PoolKind::Postgres,
                shared_instance: "local-pg-cluster".to_owned(),
                inject_url_as: "DATABASE_URL".to_owned(),
            }],
        )
        .unwrap();
        Project::new(
            ProjectId(id),
            format!("app-{id}"),
            RepoUrl::parse("https://github.com/org/app.git").unwrap(),
            config,
        )
        .unwrap()
    }

    fn env(id: u64, project_id: u64, state: EnvironmentState, now: i64) -> Environment {
        let branch = Branch::new(BranchName::parse("feature-a").unwrap(), SHA).unwrap();
        Environment::new(
            EnvironmentId(id),
            ProjectId(project_id),
            branch,
            state,
            "feature-a.app.local.dev",
            OffsetDateTime::from_unix_timestamp(now).unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn project_crud() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let p = project(1);

        ProjectStore::create(&store, &p).await.unwrap();
        let loaded = ProjectStore::get(&store, ProjectId(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.name, p.name);
        assert_eq!(loaded.config.dependencies[0].inject_url_as, "DATABASE_URL");
        assert_eq!(loaded.config.build.on_start[0], "db:migrate");

        assert_eq!(ProjectStore::list(&store).await.unwrap().len(), 1);

        ProjectStore::delete(&store, ProjectId(1)).await.unwrap();
        assert!(
            ProjectStore::get(&store, ProjectId(1))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn project_conflict_and_not_found() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        ProjectStore::create(&store, &project(1)).await.unwrap();
        assert!(matches!(
            ProjectStore::create(&store, &project(1)).await,
            Err(RepositoryError::Conflict(_))
        ));
        assert!(matches!(
            ProjectStore::delete(&store, ProjectId(99)).await,
            Err(RepositoryError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn environment_transition_persists() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        ProjectStore::create(&store, &project(1)).await.unwrap();

        let mut e = env(1, 1, EnvironmentState::Building, 1_000);
        EnvironmentStore::create(&store, &e).await.unwrap();

        let now = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
        e.transition(StateTransition::BuildSucceeded, now).unwrap();
        EnvironmentStore::update(&store, &e).await.unwrap();

        let loaded = EnvironmentStore::get(&store, EnvironmentId(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.state, EnvironmentState::Running);
        assert_eq!(loaded.updated_at, now);

        let running = EnvironmentStore::list_by_state(&store, EnvironmentState::Running)
            .await
            .unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(
            EnvironmentStore::list_by_project(&store, ProjectId(1))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn environment_update_unknown_fails() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        assert!(matches!(
            EnvironmentStore::update(&store, &env(1, 1, EnvironmentState::Running, 1_000)).await,
            Err(RepositoryError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn audit_trail() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        ProjectStore::create(&store, &project(1)).await.unwrap();
        EnvironmentStore::create(&store, &env(1, 1, EnvironmentState::Running, 1_000))
            .await
            .unwrap();

        AuditStore::record(
            &store,
            &AuditEvent::new(
                1,
                EnvironmentId(1),
                StateTransition::Woken,
                None,
                ts_from(1_000),
            ),
        )
        .await
        .unwrap();
        AuditStore::record(
            &store,
            &AuditEvent::new(
                2,
                EnvironmentId(1),
                StateTransition::IdleTimeout,
                None,
                ts_from(2_000),
            ),
        )
        .await
        .unwrap();

        let events =
            AuditStore::list_by_environment(&store, EnvironmentId(1), &AuditFilter::default())
                .await
                .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, StateTransition::Woken);

        let recent = AuditStore::list_recent(
            &store,
            &AuditFilter {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].kind, StateTransition::IdleTimeout);

        // `kind` filter narrows `list_recent` to just that transition.
        let woken_only = AuditStore::list_recent(
            &store,
            &AuditFilter {
                kind: Some(StateTransition::Woken),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(woken_only.len(), 1);
        assert_eq!(woken_only[0].kind, StateTransition::Woken);

        // `project_id` filter requires the `environments` join to work.
        let by_project = AuditStore::list_recent(
            &store,
            &AuditFilter {
                project_id: Some(ProjectId(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_project.len(), 2);
        let by_other_project = AuditStore::list_recent(
            &store,
            &AuditFilter {
                project_id: Some(ProjectId(2)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(by_other_project.is_empty());
    }

    #[tokio::test]
    async fn delete_cascades_to_children() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        ProjectStore::create(&store, &project(1)).await.unwrap();
        EnvironmentStore::create(&store, &env(1, 1, EnvironmentState::Running, 1_000))
            .await
            .unwrap();

        ProjectStore::delete(&store, ProjectId(1)).await.unwrap();
        assert!(
            EnvironmentStore::get(&store, EnvironmentId(1))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        // environment referencing a missing project must be rejected by the FK.
        let result = sqlx::query(
            "INSERT INTO environments (id, project_id, branch_name, commit_sha, state, url, \
             created_at, updated_at, last_accessed_at) VALUES (1, 999, 'x', ?, 'running', \
             'u', 0, 0, 0)",
        )
        .bind(SHA)
        .execute(&store.pool)
        .await;
        assert!(result.is_err());
    }

    fn ts_from(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).unwrap()
    }

    /// Regression test for a real secret-leakage bug: the old
    /// `SECRET_CONTEXT_FILTER` matched every row for a project regardless of
    /// branch, so branch A's `branch`-scoped secret was visible from branch
    /// B's deploy whenever both defined a secret with the same name. Found
    /// by deploying two real branches with same-named branch secrets and
    /// observing one branch's container receive the other's value.
    #[tokio::test]
    async fn branch_scoped_secrets_do_not_leak_across_branches() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        ProjectStore::create(&store, &project(1)).await.unwrap();

        let branch_a = BranchName::parse("feature-a").unwrap();
        let branch_b = BranchName::parse("feature-b").unwrap();

        SecretStore::set_secret(
            &store,
            Some(ProjectId(1)),
            None,
            "DB_PASS",
            EnvVarScope::Project,
            &SecretValue::new("project-level"),
        )
        .await
        .unwrap();
        SecretStore::set_secret(
            &store,
            Some(ProjectId(1)),
            Some(&branch_a),
            "DB_PASS",
            EnvVarScope::Branch,
            &SecretValue::new("only-for-branch-a"),
        )
        .await
        .unwrap();

        // Branch A sees its own override.
        let ctx_a = SecretStore::secrets_for(&store, Some(ProjectId(1)), Some(&branch_a))
            .await
            .unwrap();
        assert_eq!(
            ctx_a.resolve("DB_PASS").unwrap().as_str(),
            "only-for-branch-a"
        );

        // Branch B must fall back to the project-level value, never see A's.
        let ctx_b = SecretStore::secrets_for(&store, Some(ProjectId(1)), Some(&branch_b))
            .await
            .unwrap();
        assert_eq!(ctx_b.resolve("DB_PASS").unwrap().as_str(), "project-level");

        // Listing branch B's visible secrets must not include a `branch`
        // scope entry at all (that row belongs to branch A).
        let listed_b = SecretStore::list_secrets(&store, Some(ProjectId(1)), Some(&branch_b))
            .await
            .unwrap();
        assert!(
            listed_b
                .iter()
                .all(|(_, scope)| *scope != EnvVarScope::Branch),
            "{listed_b:?}"
        );
    }

    #[tokio::test]
    async fn resource_lease_lifecycle() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        ProjectStore::create(&store, &project(1)).await.unwrap();
        let branch = BranchName::parse("feature-a").unwrap();

        assert!(
            store
                .find_resource_lease(ProjectId(1), &branch, PoolKind::Redis, "local-redis")
                .await
                .unwrap()
                .is_none()
        );

        store
            .create_resource_lease(ProjectId(1), &branch, PoolKind::Redis, "local-redis", "3")
            .await
            .unwrap();
        assert_eq!(
            store
                .find_resource_lease(ProjectId(1), &branch, PoolKind::Redis, "local-redis")
                .await
                .unwrap(),
            Some("3".to_owned())
        );
        assert_eq!(
            store
                .used_resource_names(PoolKind::Redis, "local-redis")
                .await
                .unwrap(),
            vec!["3".to_owned()]
        );

        let taken = store
            .take_resource_leases(ProjectId(1), &branch)
            .await
            .unwrap();
        assert_eq!(taken, vec![(PoolKind::Redis, "3".to_owned())]);
        assert!(
            store
                .find_resource_lease(ProjectId(1), &branch, PoolKind::Redis, "local-redis")
                .await
                .unwrap()
                .is_none(),
            "take_resource_leases must remove the row"
        );
    }

    #[tokio::test]
    async fn resource_leases_are_isolated_per_project() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let mut second = project(2);
        second.repo_url = RepoUrl::parse("https://github.com/org/other.git").unwrap();
        let id1 = ProjectStore::create(&store, &project(1)).await.unwrap();
        let id2 = ProjectStore::create(&store, &second).await.unwrap();
        let branch = BranchName::parse("feature-a").unwrap();

        store
            .create_resource_lease(id1, &branch, PoolKind::Postgres, "pg", "db_1")
            .await
            .unwrap();
        store
            .create_resource_lease(id2, &branch, PoolKind::Postgres, "pg", "db_2")
            .await
            .unwrap();

        assert_eq!(
            store
                .find_resource_lease(id1, &branch, PoolKind::Postgres, "pg")
                .await
                .unwrap(),
            Some("db_1".to_owned())
        );
        let mut used = store
            .used_resource_names(PoolKind::Postgres, "pg")
            .await
            .unwrap();
        used.sort();
        assert_eq!(used, vec!["db_1".to_owned(), "db_2".to_owned()]);

        store.take_resource_leases(id1, &branch).await.unwrap();
        assert_eq!(
            store
                .find_resource_lease(id2, &branch, PoolKind::Postgres, "pg")
                .await
                .unwrap(),
            Some("db_2".to_owned()),
            "deleting project 1's lease must not touch project 2's"
        );
    }

    #[tokio::test]
    async fn api_token_scopes_round_trip_and_revocation() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        let unscoped_id = store
            .create_api_token("root", "hash-unscoped", None)
            .await
            .unwrap();
        let scoped_id = store
            .create_api_token("alice", "hash-scoped", Some(&[3, 1, 3]))
            .await
            .unwrap();
        let _ = (unscoped_id, scoped_id);

        // Scopes survive the round trip verbatim at this layer (sorting/
        // dedup is `ControlPlane::create_operator_token`'s job) and an
        // unscoped token reads back as `None`.
        let alice = store
            .find_operator_by_token_hash("hash-scoped")
            .await
            .unwrap()
            .expect("live token");
        assert_eq!(alice.name, "alice");
        assert_eq!(alice.scoped_projects, Some(vec![3, 1, 3]));

        let root = store
            .find_operator_by_token_hash("hash-unscoped")
            .await
            .unwrap()
            .expect("live token");
        assert_eq!(root.scoped_projects, None);

        // The summary exposes scopes too (`oxid token list` renders them).
        let summaries = store.list_api_tokens().await.unwrap();
        let summary_for = |id: u64| summaries.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            summary_for(scoped_id).scoped_projects,
            Some(vec![3, 1, 3]),
            "scopes are visible in the non-secret view"
        );
        assert_eq!(summary_for(unscoped_id).scoped_projects, None);

        // Revocation hides the identity again — scopes included.
        store.revoke_api_token(scoped_id).await.unwrap();
        assert!(
            store
                .find_operator_by_token_hash("hash-scoped")
                .await
                .unwrap()
                .is_none(),
            "a revoked token must not authenticate"
        );
    }

    #[tokio::test]
    async fn legacy_tokens_without_scope_column_read_as_unscoped() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .create_api_token("old-row", "hash-old", None)
            .await
            .unwrap();
        let identity = store
            .find_operator_by_token_hash("hash-old")
            .await
            .unwrap()
            .expect("token exists");
        assert_eq!(identity.scoped_projects, None);
    }
}
