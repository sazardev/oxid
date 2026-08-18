//! `SQLite` persistence adapter implementing the domain ports
//! (SPEC.md §2.2 "Persistencia").
//!
//! Storage encoding:
//! - timestamps as `INTEGER` unix-seconds,
//! - TTLs as `INTEGER` whole seconds,
//! - enum states as `TEXT` (`Display`/`FromStr`),
//! - variable-length collections (`on_start`, `dependencies`) as JSON.

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use oxid_core::{
    AuditEvent, AuditStore, Branch, BranchName, BuildConfig, Dependency, DomainError, Environment,
    EnvironmentId, EnvironmentState, EnvironmentStore, EnvVarScope, OffsetDateTime, Project,
    ProjectConfig, ProjectId, ProjectStore, RepoUrl, RepositoryError, SecretContext, SecretStore,
    SecretValue, StateTransition, Ttl,
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
    cipher: Cipher,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path` with `WAL` journaling
    /// and foreign keys enabled, then runs the embedded migrations.
    ///
    /// # Errors
    /// Returns [`StoreError`] on connection or migration failure.
    pub async fn open(path: impl AsRef<Path>, cipher: Cipher) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
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

    async fn open_with(opts: SqliteConnectOptions, cipher: Cipher) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, cipher })
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
}

const PROJECT_COLUMNS: &str = "id, name, repo_url, base_domain, pause_after_seconds, \
     destroy_after_seconds, port, dockerfile, build_context, on_start_json, dependencies_json";

const PROJECT_COLUMNS_NO_ID: &str = "name, repo_url, base_domain, pause_after_seconds, \
     destroy_after_seconds, port, dockerfile, build_context, on_start_json, dependencies_json";

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

    let on_start: Vec<String> =
        serde_json::from_str(&on_start_json).map_err(|e| storage(e.to_string()))?;
    let dependencies: Vec<Dependency> =
        serde_json::from_str(&dependencies_json).map_err(|e| storage(e.to_string()))?;

    let build = BuildConfig {
        dockerfile,
        context: build_context,
        on_start,
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
             (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"
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
}

// ---------------------------------------------------------------------------
// environment mapping
// ---------------------------------------------------------------------------

const ENV_COLUMNS: &str = "id, project_id, branch_name, commit_sha, state, url, \
     created_at, updated_at, last_accessed_at";

const ENV_COLUMNS_NO_ID: &str = "project_id, branch_name, commit_sha, state, url, \
     created_at, updated_at, last_accessed_at";

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
    Ok(env)
}

impl EnvironmentStore for SqliteStore {
    async fn create(&self, env: &Environment) -> Result<EnvironmentId, RepositoryError> {
        let binds = env_to_binds(env);
        let row = sqlx::query(&format!(
            "INSERT INTO environments ({ENV_COLUMNS_NO_ID}) VALUES \
             (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"
        ))
        .bind(binds.project_id)
        .bind(binds.branch_name)
        .bind(binds.commit_sha)
        .bind(binds.state)
        .bind(binds.url)
        .bind(binds.created_at)
        .bind(binds.updated_at)
        .bind(binds.last_accessed_at)
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
             state = ?, url = ?, created_at = ?, updated_at = ?, last_accessed_at = ? \
             WHERE id = ?",
        )
        .bind(binds.project_id)
        .bind(binds.branch_name)
        .bind(binds.commit_sha)
        .bind(binds.state)
        .bind(binds.url)
        .bind(binds.created_at)
        .bind(binds.updated_at)
        .bind(binds.last_accessed_at)
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

const AUDIT_COLUMNS: &str = "id, environment_id, kind, detail, occurred_at";

fn audit_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<AuditEvent, RepositoryError> {
    let id = id_from_row(row, "id")?;
    let environment_id = id_from_row(row, "environment_id")?;
    let kind: String = row.try_get("kind").map_err(storage)?;
    let detail: Option<String> = row.try_get("detail").map_err(storage)?;
    let occurred_at = ts_from_row(row, "occurred_at")?;

    Ok(AuditEvent::new(
        id,
        EnvironmentId(environment_id),
        kind.parse::<StateTransition>()
            .map_err(|e| validation(&e))?,
        detail,
        occurred_at,
    ))
}

impl AuditStore for SqliteStore {
    async fn record(&self, event: &AuditEvent) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO audit_events (environment_id, kind, detail, occurred_at) \
                     VALUES (?, ?, ?, ?)",
        )
        .bind(id_as_i64(event.environment_id.0))
        .bind(event.kind.to_string())
        .bind(event.detail.as_deref())
        .bind(ts(&event.occurred_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn list_by_environment(
        &self,
        environment_id: EnvironmentId,
    ) -> Result<Vec<AuditEvent>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_events WHERE environment_id = ? \
             ORDER BY occurred_at ASC, id ASC"
        ))
        .bind(id_as_i64(environment_id.0))
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(audit_from_row).collect()
    }

    async fn list_recent(&self, limit: u64) -> Result<Vec<AuditEvent>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_events ORDER BY occurred_at DESC, id DESC \
             LIMIT ?"
        ))
        .bind(i64::try_from(limit).expect("limit fits in i64"))
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
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
        let value_enc = self.cipher.encrypt(value.as_str()).map_err(storage)?;
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

        let mut ctx = SecretContext::new();
        for row in rows {
            let name: String = row.try_get("name").map_err(storage)?;
            let scope: String = row.try_get("scope").map_err(storage)?;
            let scope = scope
                .parse::<EnvVarScope>()
                .map_err(storage)?;
            let value_enc: String = row.try_get("value_enc").map_err(storage)?;
            let value = self.cipher.decrypt(&value_enc).map_err(storage)?;
            ctx.set(name, scope, SecretValue::new(value));
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
                let scope = scope
                    .parse::<EnvVarScope>()
                    .map_err(storage)?;
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
        let res = sqlx::query(
            "DELETE FROM secrets WHERE project_id IS ? AND branch IS ? AND name = ?",
        )
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

/// Filter matching global (`project_id IS NULL`), project and branch secrets
/// for a context. `?1` = project id (used twice), `?2` = branch name.
const SECRET_CONTEXT_FILTER: &str = "project_id IS NULL OR project_id = ? OR \
     (project_id = ? AND (branch IS NULL OR branch = ?))";

const SECRET_COLUMNS: &str = "name, scope, value_enc";

#[cfg(test)]
mod tests {
    use super::*;
    use oxid_core::{AuditStore, EnvironmentStore, PoolKind, ProjectStore, StateTransition};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

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

        let events = AuditStore::list_by_environment(&store, EnvironmentId(1))
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, StateTransition::Woken);

        let recent = AuditStore::list_recent(&store, 1).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].kind, StateTransition::IdleTimeout);
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
}
