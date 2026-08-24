//! Periodic on-disk database backups — the disaster-recovery posture for
//! the deliberately single-node `SQLite` design (SPEC.md §6 self-hosting).
//!
//! Every `interval` seconds the control-plane database is snapshotted with
//! `SQLite`'s `VACUUM INTO` (consistent against the live pool — same
//! mechanism as the `oxid backup` CLI, see [`SqliteStore::backup_to`] and
//! `ControlPlane::backup_database`) into a rotating directory. Old
//! snapshots beyond `keep` are deleted newest-first by filename, so the
//! on-disk cost is bounded at `keep` × database size.
//!
//! Restoring is the existing flow: stop the daemon, drop the archive's
//! `audit.sqlite` into `/data` (or use `oxid restore`, which stages it for
//! the next restart). For streaming replication to off-site storage run a
//! Litestream sidecar next to the daemon instead — see the example in the
//! shipped `docker-compose.yml`.
//!
//! Off by default (`OXID_BACKUP_INTERVAL_SECS` unset or `0`): backups only
//! matter once there's data worth keeping, and an operator who wants them
//! also wants to decide where they live.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oxid_core::{ContainerPort, GitPort};

use crate::service::control_plane::ControlPlane;

/// Where and how often to snapshot, plus how many snapshots to keep.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Seconds between snapshots.
    pub interval: Duration,
    /// Snapshots retained before rotation deletes the oldest.
    pub keep: usize,
    /// Directory snapshots are written to.
    pub dir: PathBuf,
}

/// Filename prefix every snapshot shares — rotation only ever deletes
/// files it recognizes as its own, never unrelated operator files that
/// happen to live in the same directory.
const SNAPSHOT_PREFIX: &str = "oxid-backup-";
const SNAPSHOT_SUFFIX: &str = ".sqlite";

/// Reads `OXID_BACKUP_INTERVAL_SECS` / `OXID_BACKUP_KEEP`. Returns `None`
/// (backups disabled) unless the interval is set to at least 1 second;
/// keep defaults to 7 and is floored at 1.
///
/// # Panics
/// Never — malformed values fall back to their defaults rather than
/// refusing to start the daemon over a typo'd env var.
#[must_use]
pub fn config_from_env(data_dir: &Path) -> Option<BackupConfig> {
    let interval_secs: u64 = std::env::var("OXID_BACKUP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if interval_secs == 0 {
        return None;
    }
    let keep: usize = std::env::var("OXID_BACKUP_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&k| k >= 1)
        .unwrap_or(7);
    Some(BackupConfig {
        interval: Duration::from_secs(interval_secs.max(1)),
        keep,
        dir: data_dir.join("backups"),
    })
}

/// Snapshot filenames sort lexicographically in chronological order
/// (`YYYYMMDD-HHMMSS`), so "newest" is a plain string comparison.
fn snapshot_name(now: oxid_core::OffsetDateTime) -> String {
    let format = time::macros::format_description!("[year][month][day]-[hour][minute][second]");
    format!(
        "{SNAPSHOT_PREFIX}{}{SNAPSHOT_SUFFIX}",
        now.format(&format)
            .unwrap_or_else(|_| now.unix_timestamp().to_string())
    )
}

/// Pure rotation decision: given snapshot filenames sorted however they
/// came out of the directory listing, returns those beyond the `keep`
/// newest (ties broken by name, which is chronological). Files without the
/// snapshot prefix/suffix are ignored entirely — never candidates for
/// deletion.
fn stale_backups<'a>(names: impl IntoIterator<Item = &'a str>, keep: usize) -> Vec<String> {
    let mut own: Vec<&str> = names
        .into_iter()
        .filter(|n| n.starts_with(SNAPSHOT_PREFIX) && n.ends_with(SNAPSHOT_SUFFIX))
        .collect();
    own.sort_unstable();
    own.into_iter()
        .rev()
        .skip(keep)
        .map(str::to_owned)
        .collect()
}

/// Applies [`stale_backups`] to `dir` for real: lists, decides, unlinks.
/// Returns how many files were removed.
///
/// # Errors
/// Propagates I/O errors from reading the directory; individual unlink
/// failures are logged and skipped (a file held open by an operator's
/// `tar` shouldn't abort the whole sweep).
fn rotate_dir(dir: &Path, keep: usize) -> std::io::Result<usize> {
    let names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let stale = stale_backups(names.iter().map(String::as_str), keep);
    let mut removed = 0;
    for name in &stale {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => removed += 1,
            Err(err) => {
                tracing::warn!(file = %name, error = %err, "backup rotation could not delete file");
            }
        }
    }
    Ok(removed)
}

/// Runs the periodic snapshot loop forever. Meant to be spawned as a
/// background task from `main.rs`, next to the GC scheduler:
/// `tokio::spawn(backup::run(cp.clone(), cfg))`. A failed snapshot is
/// logged and retried on the next tick — one full disk (or one transient
/// I/O hiccup) must not take down the loop that would recover from it.
pub async fn run<G, O>(cp: ControlPlane<G, O>, config: BackupConfig)
where
    G: GitPort + Clone + Send + Sync + 'static,
    O: ContainerPort + Clone + Send + Sync + 'static,
{
    let BackupConfig {
        interval,
        keep,
        dir,
    } = config;
    if std::fs::create_dir_all(&dir).is_err() {
        tracing::error!(
            dir = %dir.display(),
            "backup directory cannot be created; periodic backups disabled"
        );
        return;
    }
    tracing::info!(
        interval_secs = interval.as_secs(),
        keep,
        dir = %dir.display(),
        "periodic database backups enabled"
    );
    let mut ticker = tokio::time::interval(interval);
    // `tokio::time::interval` fires immediately at t=0 — consume that one
    // outside the loop: snapshotting the just-migrated (possibly empty)
    // database right at startup buys nothing.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let dest = dir.join(snapshot_name(oxid_core::OffsetDateTime::now_utc()));
        // `VACUUM INTO` fails if the destination exists; within a
        // second-resolution timestamp two snapshots can collide when the
        // interval is 1s, so remove any such leftover first.
        let _ = std::fs::remove_file(&dest);
        match cp.backup_database(&dest).await {
            Ok(()) => {
                match rotate_dir(&dir, keep) {
                    Ok(n) if n > 0 => tracing::debug!(removed = n, "backup rotation completed"),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(error = %err, "backup rotation failed"),
                }
                tracing::info!(snapshot = %dest.display(), "database backup written");
            }
            Err(err) => {
                tracing::error!(error = %err, "periodic database backup failed");
                let _ = std::fs::remove_file(&dest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Filenames sort lexicographically == chronologically, so the `keep`
    /// newest survive rotation regardless of listing order.
    #[test]
    fn stale_backups_keeps_newest_by_name_order() {
        let names = [
            format!("{SNAPSHOT_PREFIX}20260824-101500{SNAPSHOT_SUFFIX}"),
            format!("{SNAPSHOT_PREFIX}20260824-100000{SNAPSHOT_SUFFIX}"),
            format!("{SNAPSHOT_PREFIX}20260824-103000{SNAPSHOT_SUFFIX}"),
        ];
        let stale = stale_backups(names.iter().map(String::as_str), 2);
        assert_eq!(
            stale,
            vec![format!("{SNAPSHOT_PREFIX}20260824-100000{SNAPSHOT_SUFFIX}")]
        );
        // Keeping exactly everything is a no-op.
        assert!(stale_backups(names.iter().map(String::as_str), 3).is_empty());
    }

    /// Files that aren't ours (different prefix/suffix) are never
    /// candidates for deletion — operators may keep unrelated files in the
    /// backup directory.
    #[test]
    fn stale_backups_ignores_foreign_files() {
        let own = format!("{SNAPSHOT_PREFIX}20260824-100000{SNAPSHOT_SUFFIX}");
        let foreign = ["notes.txt", "audit.sqlite", "oxid-backup-partial"];
        let stale = stale_backups(
            foreign.iter().copied().chain(std::iter::once(own.as_str())),
            0,
        );
        assert_eq!(stale, vec![own]);
    }

    /// End-to-end over a real tempdir: writing five snapshots and rotating
    /// with keep=2 leaves exactly the two newest on disk.
    #[test]
    fn rotate_dir_deletes_only_oldest_beyond_keep() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "oxid-backup-20260824-090000.sqlite",
            "oxid-backup-20260824-093000.sqlite",
            "oxid-backup-20260824-100000.sqlite",
            "unrelated.txt",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let removed = rotate_dir(dir.path(), 2).unwrap();
        assert_eq!(removed, 1);
        let mut remaining: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            vec![
                "oxid-backup-20260824-093000.sqlite",
                "oxid-backup-20260824-100000.sqlite",
                "unrelated.txt",
            ]
        );
    }

    /// Zero-padded timestamps are what make lexicographic == chronological
    /// hold across month boundaries (without padding, October `"10"` would
    /// sort before September `"09"`).
    #[test]
    fn snapshot_names_sort_chronologically_across_months() {
        let september = time::OffsetDateTime::from_unix_timestamp(1_789_000_000).unwrap();
        let october = september
            .replace_month(time::Month::October)
            .unwrap()
            .replace_year(2027)
            .unwrap();
        assert!(snapshot_name(september) < snapshot_name(october));
    }
}
