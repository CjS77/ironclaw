//! Stale-lock recovery for the bundled skill install lock.
//!
//! Each scenario runs against the in-memory and libSQL backends (compare-and-swap on the lock
//! entry's version) and, where the standalone path differs, against the host-disk backend (an OS
//! file lock in the storage root).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use futures::future::{Either, select};
use ironclaw_filesystem::{
    BackendCapabilities, CasExpectation, DirEntry, Entry, FileStat, FilesystemError,
    InMemoryBackend, LibSqlRootFilesystem, RecordVersion, RootFilesystem, VersionedEntry,
};
use ironclaw_host_api::path::VirtualPath;
use tokio::sync::Barrier;

use super::super::{
    ensure_bundled_reborn_skills_installed, ensure_bundled_reborn_skills_installed_in,
    install_bundled_skills, install_into_standalone_root,
};
use super::*;

const SKILLS_ROOT: &str = "/system/skills";
const OLD_FORMAT_LOCK: &[u8] = b"Instant { tv_sec: 690596, tv_nsec: 518279431 }";

struct Backend {
    name: &'static str,
    filesystem: Arc<dyn RootFilesystem>,
    _dir: Option<tempfile::TempDir>,
}

async fn backends() -> Vec<Backend> {
    let dir = tempfile::tempdir().expect("tempdir");
    let database = Arc::new(
        libsql::Builder::new_local(dir.path().join("install-lock.db"))
            .build()
            .await
            .expect("libsql database"),
    );
    let libsql = LibSqlRootFilesystem::new(database).expect("libsql filesystem");
    libsql.run_migrations().await.expect("libsql migrations");
    vec![
        Backend {
            name: "in-memory",
            filesystem: Arc::new(InMemoryBackend::new()),
            _dir: None,
        },
        Backend {
            name: "libsql",
            filesystem: Arc::new(libsql),
            _dir: Some(dir),
        },
    ]
}

fn skills_root() -> VirtualPath {
    VirtualPath::new(SKILLS_ROOT).expect("skills root")
}

fn lock_path() -> VirtualPath {
    VirtualPath::new(format!("{SKILLS_ROOT}/{LOCK_ENTRY_FILE}")).expect("lock path")
}

fn holder_record(
    pid: u32,
    host: &HostIdentity,
    acquired_at: DateTime<Utc>,
    nonce: &str,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "format": 1,
        "pid": pid,
        "host": {
            "hostname": host.hostname,
            "boot_id": host.boot_id,
            "pid_namespace": host.pid_namespace,
        },
        "acquired_at": acquired_at.to_rfc3339(),
        "nonce": nonce,
    }))
    .expect("holder json")
}

fn other_host() -> HostIdentity {
    HostIdentity {
        hostname: Some("some-other-host".to_string()),
        boot_id: Some("00000000-0000-0000-0000-000000000000".to_string()),
        pid_namespace: Some("pid:[1]".to_string()),
    }
}

/// A pid that belonged to a process on this host which has since exited.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn short-lived child");
    let pid = child.id();
    child.wait().expect("reap child");
    pid
}

fn short_wait() -> InstallLockPolicy {
    InstallLockPolicy {
        wait_timeout: Duration::from_millis(400),
        ..InstallLockPolicy::default()
    }
}

fn minutes_ago(minutes: i64) -> DateTime<Utc> {
    Utc::now() - chrono::Duration::minutes(minutes)
}

async fn write_lock(filesystem: &dyn RootFilesystem, body: Vec<u8>) -> RecordVersion {
    filesystem
        .put(&lock_path(), Entry::bytes(body), CasExpectation::Absent)
        .await
        .expect("seed lock entry")
}

async fn lock_body(filesystem: &dyn RootFilesystem) -> Option<Vec<u8>> {
    filesystem
        .get(&lock_path())
        .await
        .expect("read lock entry")
        .map(|entry| entry.entry.body)
}

async fn assert_installed(filesystem: &dyn RootFilesystem, backend: &str) {
    let skill = VirtualPath::new(format!("{SKILLS_ROOT}/coding/SKILL.md")).expect("skill path");
    assert!(
        filesystem.get(&skill).await.expect("read skill").is_some(),
        "{backend}: bundled skills were not installed"
    );
    assert!(
        lock_body(filesystem).await.is_none(),
        "{backend}: the install lock was not released"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_left_by_a_dead_process_on_this_host_is_taken_over_at_once() {
    for backend in backends().await {
        let filesystem = backend.filesystem.as_ref();
        write_lock(
            filesystem,
            holder_record(dead_pid(), &HostIdentity::current(), Utc::now(), "dead"),
        )
        .await;

        let started = std::time::Instant::now();
        ensure_bundled_reborn_skills_installed_in(filesystem, &skills_root())
            .await
            .unwrap_or_else(|error| panic!("{}: install failed: {error}", backend.name));

        assert!(
            started.elapsed() < InstallLockPolicy::default().wait_timeout / 2,
            "{}: a dead holder's lock must be taken over without waiting",
            backend.name
        );
        assert_installed(filesystem, backend.name).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_held_by_a_live_process_is_waited_on_and_the_timeout_names_the_holder() {
    for backend in backends().await {
        let filesystem = backend.filesystem.as_ref();
        let host = HostIdentity::current();
        let acquired_at = minutes_ago(1);
        let held = holder_record(std::process::id(), &host, acquired_at, "live");
        write_lock(filesystem, held.clone()).await;

        let started = std::time::Instant::now();
        let error = install_bundled_skills(filesystem, &skills_root(), None, &short_wait())
            .await
            .expect_err("a live holder must not be taken over");
        let message = error.to_string();

        assert!(
            started.elapsed() >= short_wait().wait_timeout,
            "{}: gave up before the wait timeout",
            backend.name
        );
        assert!(
            message.contains(&format!("pid {}", std::process::id())),
            "{}: {message}",
            backend.name
        );
        if let Some(hostname) = &host.hostname {
            assert!(message.contains(hostname), "{}: {message}", backend.name);
        }
        assert!(
            message.contains(&acquired_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
            "{}: {message}",
            backend.name
        );
        assert_eq!(
            lock_body(filesystem).await,
            Some(held),
            "{}: the live holder's lock was disturbed",
            backend.name
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_format_lock_is_taken_over_once_past_the_grace_period() {
    for backend in backends().await {
        let filesystem = backend.filesystem.as_ref();
        write_lock(filesystem, OLD_FORMAT_LOCK.to_vec()).await;
        let policy = InstallLockPolicy {
            unrecognised_grace: Duration::ZERO,
            ..short_wait()
        };

        install_bundled_skills(filesystem, &skills_root(), None, &policy)
            .await
            .unwrap_or_else(|error| panic!("{}: install failed: {error}", backend.name));

        assert_installed(filesystem, backend.name).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_old_format_lock_is_waited_on() {
    for backend in backends().await {
        let filesystem = backend.filesystem.as_ref();
        write_lock(filesystem, OLD_FORMAT_LOCK.to_vec()).await;

        let error = install_bundled_skills(filesystem, &skills_root(), None, &short_wait())
            .await
            .expect_err("a just-written lock in an unknown format must be waited on");

        assert!(
            error.to_string().contains("unrecognised format"),
            "{}: {error}",
            backend.name
        );
        assert_eq!(
            lock_body(filesystem).await.as_deref(),
            Some(OLD_FORMAT_LOCK),
            "{}",
            backend.name
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_from_another_host_older_than_the_maximum_hold_is_taken_over() {
    for backend in backends().await {
        let filesystem = backend.filesystem.as_ref();
        let max_hold_minutes =
            i64::try_from(InstallLockPolicy::default().max_hold.as_secs() / 60).expect("minutes");
        write_lock(
            filesystem,
            holder_record(
                4242,
                &other_host(),
                minutes_ago(max_hold_minutes + 1),
                "old",
            ),
        )
        .await;

        ensure_bundled_reborn_skills_installed_in(filesystem, &skills_root())
            .await
            .unwrap_or_else(|error| panic!("{}: install failed: {error}", backend.name));

        assert_installed(filesystem, backend.name).await;
    }
}

/// A pid is only meaningful on the host (and boot, and pid namespace) that issued it: a recent
/// lock from another host is waited on even when the same pid number is free here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recent_lock_from_another_host_is_waited_on_whatever_its_pid() {
    for backend in backends().await {
        let filesystem = backend.filesystem.as_ref();
        let held = holder_record(dead_pid(), &other_host(), minutes_ago(1), "remote");
        write_lock(filesystem, held.clone()).await;

        let error = install_bundled_skills(filesystem, &skills_root(), None, &short_wait())
            .await
            .expect_err("a recent lock from another host must be waited on");

        assert!(
            error.to_string().contains("some-other-host"),
            "{}: {error}",
            backend.name
        );
        assert_eq!(lock_body(filesystem).await, Some(held), "{}", backend.name);
    }
}

/// Holds back every takeover write until two of them are in flight, so both starters have read
/// the same stale lock before either tries to replace it.
struct PauseTakeovers {
    inner: Arc<dyn RootFilesystem>,
    both_read: Barrier,
    takeovers: AtomicUsize,
}

impl PauseTakeovers {
    fn new(inner: Arc<dyn RootFilesystem>) -> Self {
        Self {
            inner,
            both_read: Barrier::new(2),
            takeovers: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl RootFilesystem for PauseTakeovers {
    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    async fn put(
        &self,
        path: &VirtualPath,
        entry: Entry,
        cas: CasExpectation,
    ) -> Result<RecordVersion, FilesystemError> {
        if matches!(cas, CasExpectation::Version(_))
            && self.takeovers.fetch_add(1, Ordering::SeqCst) < 2
        {
            self.both_read.wait().await;
        }
        self.inner.put(path, entry, cas).await
    }

    async fn get(&self, path: &VirtualPath) -> Result<Option<VersionedEntry>, FilesystemError> {
        self.inner.get(path).await
    }

    async fn list_dir(&self, path: &VirtualPath) -> Result<Vec<DirEntry>, FilesystemError> {
        self.inner.list_dir(path).await
    }

    async fn stat(&self, path: &VirtualPath) -> Result<FileStat, FilesystemError> {
        self.inner.stat(path).await
    }

    async fn delete(&self, path: &VirtualPath) -> Result<(), FilesystemError> {
        self.inner.delete(path).await
    }

    async fn delete_if_version(
        &self,
        path: &VirtualPath,
        expected_version: RecordVersion,
    ) -> Result<(), FilesystemError> {
        self.inner.delete_if_version(path, expected_version).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_starters_racing_for_a_stale_lock_produce_exactly_one_holder() {
    for backend in backends().await {
        write_lock(
            backend.filesystem.as_ref(),
            holder_record(dead_pid(), &HostIdentity::current(), Utc::now(), "dead"),
        )
        .await;
        let filesystem = Arc::new(PauseTakeovers::new(Arc::clone(&backend.filesystem)));
        let policy = InstallLockPolicy {
            wait_timeout: Duration::from_secs(20),
            ..InstallLockPolicy::default()
        };
        let starter = |filesystem: Arc<PauseTakeovers>, policy: InstallLockPolicy| {
            tokio::spawn(async move {
                BundledSkillInstallLock::acquire(filesystem.as_ref(), &skills_root(), None, &policy)
                    .await
            })
        };
        let first = starter(Arc::clone(&filesystem), policy.clone());
        let second = starter(Arc::clone(&filesystem), policy);

        let (winner, loser) =
            match tokio::time::timeout(Duration::from_secs(10), select(first, second))
                .await
                .unwrap_or_else(|_| panic!("{}: neither starter took the lock", backend.name))
            {
                Either::Left((winner, loser)) | Either::Right((winner, loser)) => (winner, loser),
            };
        let winner = winner
            .expect("starter task")
            .unwrap_or_else(|error| panic!("{}: winner failed: {error}", backend.name));

        assert_eq!(
            filesystem.takeovers.load(Ordering::SeqCst),
            2,
            "{}: both starters must have tried to replace the stale lock",
            backend.name
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !loser.is_finished(),
            "{}: the losing starter must wait for the winner",
            backend.name
        );

        winner
            .release(filesystem.as_ref())
            .await
            .expect("winner releases");
        let loser = tokio::time::timeout(Duration::from_secs(10), loser)
            .await
            .unwrap_or_else(|_| panic!("{}: loser never took the released lock", backend.name))
            .expect("starter task")
            .unwrap_or_else(|error| panic!("{}: loser failed: {error}", backend.name));
        loser
            .release(filesystem.as_ref())
            .await
            .expect("loser releases");
        assert!(
            lock_body(filesystem.as_ref()).await.is_none(),
            "{}",
            backend.name
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_that_was_taken_over_does_not_delete_the_new_owners_lock() {
    for backend in backends().await {
        let filesystem = backend.filesystem.as_ref();
        let lock = BundledSkillInstallLock::acquire(
            filesystem,
            &skills_root(),
            None,
            &InstallLockPolicy::default(),
        )
        .await
        .expect("acquire");
        let mine = filesystem
            .get(&lock_path())
            .await
            .expect("read lock")
            .expect("lock entry present");
        let new_owner = holder_record(4242, &other_host(), Utc::now(), "new-owner");
        filesystem
            .put(
                &lock_path(),
                Entry::bytes(new_owner.clone()),
                CasExpectation::Version(mine.version),
            )
            .await
            .expect("another starter takes the lock over");

        lock.release(filesystem)
            .await
            .expect("release after being taken over");

        assert_eq!(
            lock_body(filesystem).await,
            Some(new_owner),
            "{}: release deleted a lock it no longer held",
            backend.name
        );
    }
}

fn standalone_lock_file(standalone_root: &std::path::Path) -> std::path::PathBuf {
    standalone_root.join("system/skills").join(LOCK_ENTRY_FILE)
}

/// The incident shape: an install killed mid-way on a host disk leaves the lock behind, and the
/// next start of the same server home must not hang on it.
#[tokio::test]
async fn disk_lock_left_by_a_killed_install_is_taken_over() {
    let dir = tempfile::tempdir().expect("tempdir");
    let standalone_root = dir.path().join("standalone");
    let lock_file = standalone_lock_file(&standalone_root);
    std::fs::create_dir_all(lock_file.parent().expect("parent")).expect("skills dir");
    std::fs::write(&lock_file, OLD_FORMAT_LOCK).expect("old lock");
    std::fs::File::options()
        .write(true)
        .open(&lock_file)
        .expect("open old lock")
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .expect("backdate old lock");

    let started = std::time::Instant::now();
    ensure_bundled_reborn_skills_installed(&standalone_root)
        .await
        .expect("install over a stale lock");

    assert!(started.elapsed() < InstallLockPolicy::default().wait_timeout / 2);
    assert!(!lock_file.exists());
    assert!(
        standalone_root
            .join("system/skills/coding/SKILL.md")
            .is_file()
    );
}

/// On a host disk every installer holds the OS file lock for as long as it holds the lock entry,
/// so a current-format entry found while that OS lock is free was left by a process that died,
/// whatever its pid now names.
#[tokio::test]
async fn disk_lock_entry_is_taken_over_when_no_process_holds_the_host_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let standalone_root = dir.path().join("standalone");
    let lock_file = standalone_lock_file(&standalone_root);
    std::fs::create_dir_all(lock_file.parent().expect("parent")).expect("skills dir");
    std::fs::write(
        &lock_file,
        holder_record(
            std::process::id(),
            &HostIdentity::current(),
            Utc::now(),
            "orphan",
        ),
    )
    .expect("orphaned lock");

    install_into_standalone_root(&standalone_root, &short_wait())
        .await
        .expect("install over an orphaned lock entry");

    assert!(!lock_file.exists());
}

#[tokio::test]
async fn disk_install_waits_for_a_live_holder_of_the_host_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let standalone_root = dir.path().join("standalone");
    let lock_file = standalone_lock_file(&standalone_root);
    std::fs::create_dir_all(lock_file.parent().expect("parent")).expect("skills dir");
    let acquired_at = minutes_ago(1);
    let held = holder_record(
        std::process::id(),
        &HostIdentity::current(),
        acquired_at,
        "live",
    );
    std::fs::write(&lock_file, &held).expect("live lock");
    let host_lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(standalone_root.join(HOST_LOCK_FILE))
        .expect("open host lock");
    host_lock.lock().expect("hold host lock");

    let error = install_into_standalone_root(&standalone_root, &short_wait())
        .await
        .expect_err("a live installer holding the host lock must be waited on");
    let message = error.to_string();

    assert!(
        message.contains(&format!("pid {}", std::process::id())),
        "{message}"
    );
    assert!(
        message.contains(&acquired_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
        "{message}"
    );
    assert_eq!(std::fs::read(&lock_file).expect("read lock"), held);
    drop(host_lock);
}
