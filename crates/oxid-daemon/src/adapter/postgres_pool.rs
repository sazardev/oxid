//! Postgres resource-pool adapter (SPEC.md §3.1): creates/drops the
//! per-branch logical databases carved out of a single shared Postgres
//! instance, so branches don't each need their own database container.
//!
//! Deliberately a plain struct with inherent methods, not a generic port
//! swapped for a fake in `ControlPlane`'s tests — there's exactly one real
//! implementation and nothing in the API layer needs to substitute it
//! (`ControlPlane` already mixes concrete adapters like `SqliteStore` with
//! generic ports like `GitPort`/`ContainerPort` for this reason). Tested
//! against a real Postgres container the same way `oci.rs`'s exec-exit-code
//! fix is: an `#[ignore]`d integration test, not a fake.

use oxid_core::PoolError;
use sqlx::postgres::PgPoolOptions;

/// Talks to a shared Postgres instance via an admin connection string.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresPool;

impl PostgresPool {
    /// Creates a fresh, short-lived connection to `admin_url`. Deploys are
    /// infrequent, so there's no need to keep a pool alive between calls.
    async fn connect(admin_url: &str) -> Result<sqlx::PgPool, PoolError> {
        PgPoolOptions::new()
            .max_connections(1)
            .connect(admin_url)
            .await
            .map_err(|e| PoolError::Failure(format!("cannot connect to `{admin_url}`: {e}")))
    }

    /// Ensures a logical database named `db_name` exists on the shared
    /// instance at `admin_url`, creating it if missing. Idempotent: safe to
    /// call on every deploy of a branch that already has its database.
    ///
    /// # Errors
    /// Returns [`PoolError::Failure`] on a connection or SQL failure.
    pub async fn ensure_database(&self, admin_url: &str, db_name: &str) -> Result<(), PoolError> {
        validate_identifier(db_name)?;
        let pool = Self::connect(admin_url).await?;

        let exists = sqlx::query("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(db_name)
            .fetch_optional(&pool)
            .await
            .map_err(|e| PoolError::Failure(format!("checking database `{db_name}`: {e}")))?
            .is_some();
        if exists {
            return Ok(());
        }

        // Postgres doesn't support parameter binding for identifiers in DDL;
        // `validate_identifier` above is what makes this interpolation safe.
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&pool)
            .await
            .map_err(|e| PoolError::Failure(format!("creating database `{db_name}`: {e}")))?;
        Ok(())
    }

    /// Drops `db_name` if it exists. Used when the branch that owned it is
    /// destroyed.
    ///
    /// # Errors
    /// Returns [`PoolError::Failure`] on a connection or SQL failure.
    pub async fn drop_database(&self, admin_url: &str, db_name: &str) -> Result<(), PoolError> {
        validate_identifier(db_name)?;
        let pool = Self::connect(admin_url).await?;
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&pool)
            .await
            .map_err(|e| PoolError::Failure(format!("dropping database `{db_name}`: {e}")))?;
        Ok(())
    }
}

/// Rejects anything but `[a-z0-9_]`, since `db_name` gets interpolated
/// directly into DDL (Postgres has no parameter-binding for identifiers).
/// `control_plane.rs` only ever derives `db_name` from already-sanitized
/// project/branch labels, but this is the actual safety boundary, not that
/// caller discipline.
fn validate_identifier(name: &str) -> Result<(), PoolError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(PoolError::Failure(format!(
            "invalid database identifier `{name}`; expected only lowercase letters, digits and underscores"
        )))
    }
}

/// Rewrites the path component of `admin_url` to point at `db_name` instead
/// of whatever database the admin connection used — this is the connection
/// string injected into the branch's container.
///
/// # Errors
/// Returns [`PoolError::Failure`] if `admin_url` isn't a valid URL.
pub fn database_url(admin_url: &str, db_name: &str) -> Result<String, PoolError> {
    let mut url = url::Url::parse(admin_url)
        .map_err(|e| PoolError::Failure(format!("invalid OXID_POSTGRES_URL: {e}")))?;
    url.set_path(&format!("/{db_name}"));
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[test]
    fn validates_identifiers() {
        assert!(validate_identifier("db_feature_a").is_ok());
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("db feature").is_err());
        assert!(validate_identifier("db;DROP TABLE users;--").is_err());
        assert!(validate_identifier("DB_UPPER").is_err());
    }

    #[test]
    fn rewrites_database_url() {
        let url = database_url("postgres://user:pass@host:5432/postgres", "db_feature_a").unwrap();
        assert_eq!(url, "postgres://user:pass@host:5432/db_feature_a");
    }

    /// Integration test gated on a running Postgres instance; ignored by
    /// default. Run with `cargo test -p oxid-daemon -- --ignored` against a
    /// machine with `postgres://oxid:oxid@127.0.0.1:5432/postgres` reachable
    /// (e.g. `docker run -e POSTGRES_USER=oxid -e POSTGRES_PASSWORD=oxid -p
    /// 5432:5432 postgres:16-alpine`).
    #[tokio::test]
    #[ignore = "requires a running Postgres instance"]
    async fn create_and_drop_a_real_database() {
        let admin_url = "postgres://oxid:oxid@127.0.0.1:5432/postgres";
        let pool = PostgresPool;
        let db_name = "oxid_test_pool_db";

        pool.drop_database(admin_url, db_name).await.unwrap();
        pool.ensure_database(admin_url, db_name).await.unwrap();
        // Idempotent: calling it again on an already-existing database
        // must not error.
        pool.ensure_database(admin_url, db_name).await.unwrap();

        let dsn = database_url(admin_url, db_name).unwrap();
        let conn = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&dsn)
            .await
            .unwrap();
        let row = sqlx::query("SELECT current_database()")
            .fetch_one(&conn)
            .await
            .unwrap();
        let name: String = row.try_get(0).unwrap();
        assert_eq!(name, db_name);
        drop(conn);

        pool.drop_database(admin_url, db_name).await.unwrap();
    }
}
