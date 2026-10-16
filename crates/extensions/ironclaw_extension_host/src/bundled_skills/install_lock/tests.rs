//! Stale-lock recovery and lease renewal for the bundled skill install lock.
//!
//! Each scenario in [`lock_scenarios!`] is a test over the in-memory and libSQL backends
//! (compare-and-swap on the lock entry's version), and all of them run again, in one test,
//! against PostgreSQL. That leg starts a testcontainer and is skipped, with a notice, when Docker
//! is unavailable or `IRONCLAW_SKIP_POSTGRES_TESTS` is set. The standalone path, where it differs,
//! is tested against the host-disk backend (an OS file lock in the storage root).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use futures::future::{Either, select};
use ironclaw_filesystem::{
    BackendCapabilities, CasExpectation, DirEntry, Entry, FileStat, FilesystemError,
    FilesystemOperation, InMemoryBackend, LibSqlRootFilesystem, PostgresRootFilesystem,
    RecordVersion, RootFilesystem, VersionedEntry,
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
/// A pid no process can have on any Unix (it exceeds every kernel's pid limit), so a pid probe
/// in a shared pid space reports it gone.
const FREE_PID: u32 = i32::MAX as u32;

struct Backend {
    name: &'static str,
    filesystem: Arc<dyn RootFilesystem>,
    _dir: Option<tempfile::TempDir>,
}

async fn local_backends() -> Vec<Backend> {
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

/// A PostgreSQL testcontainer for one test. Dropping it stops and removes the container.
struct PostgresServer {
    url: String,
    next_database: AtomicU64,
    _container: testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::postgres::Postgres,
    >,
}

impl PostgresServer {
    /// Starts the container, or returns `None` (with a notice) when Docker is unavailable or
    /// `IRONCLAW_SKIP_POSTGRES_TESTS` is set.
    async fn start() -> Option<Self> {
        use testcontainers_modules::testcontainers::{ImageExt, runners::AsyncRunner};

        if std::env::var_os("IRONCLAW_SKIP_POSTGRES_TESTS").is_some() {
            eprintln!(
                "skipping the PostgreSQL install-lock leg: IRONCLAW_SKIP_POSTGRES_TESTS is set"
            );
            return None;
        }
        let image = testcontainers_modules::postgres::Postgres::default()
            .with_db_name("ironclaw_test")
            .with_user("postgres")
            .with_password("postgres")
            .with_tag("16-alpine");
        let container = match image.start().await {
            Ok(container) => container,
            Err(error) => {
                eprintln!(
                    "skipping the PostgreSQL install-lock leg: docker/testcontainers unavailable \
                     ({error})"
                );
                return None;
            }
        };
        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("container port");
        Some(Self {
            url: format!("postgres://postgres:postgres@{host}:{port}/ironclaw_test"),
            next_database: AtomicU64::new(0),
            _container: container,
        })
    }

    /// A fresh database on the server, so each scenario starts with no lock entry.
    async fn backend(&self) -> Backend {
        let (admin, connection) = tokio_postgres::connect(&self.url, tokio_postgres::NoTls)
            .await
            .expect("connect to the PostgreSQL test server");
        tokio::spawn(connection);
        let name = format!(
            "install_lock_{}",
            self.next_database.fetch_add(1, Ordering::Relaxed)
        );
        admin
            .execute(&format!("CREATE DATABASE {name}"), &[])
            .await
            .expect("create a database for this scenario");
        let mut config = self
            .url
            .parse::<tokio_postgres::Config>()
            .expect("PostgreSQL url");
        config.dbname(&name);
        let pool = deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(
            config,
            tokio_postgres::NoTls,
        ))
        .max_size(4)
        .build()
        .expect("PostgreSQL pool");
        let filesystem = PostgresRootFilesystem::new(pool);
        filesystem
            .run_migrations()
            .await
            .expect("PostgreSQL migrations");
        Backend {
            name: "postgres",
            filesystem: Arc::new(filesystem),
            _dir: None,
        }
    }
}

/// Declares each scenario as a test over the in-memory and libSQL backends, plus one test that
/// runs every scenario in turn against PostgreSQL, the production backend. Running them all in
/// one test keeps the PostgreSQL leg to a single container, which that test removes when it ends.
macro_rules! lock_scenarios {
    ($(
        $(#[doc = $doc:literal])*
        $(#[cfg($cfg:meta)])?
        async fn $name:ident($backend:ident) $body:block
    )+) => {
        $(
            $(#[doc = $doc])*
            $(#[cfg($cfg)])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                for $backend in local_backends().await $body
            }
        )+

        /// Every scenario above, each on a fresh PostgreSQL database.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn every_lock_scenario_holds_on_postgres() {
            let Some(server) = PostgresServer::start().await else {
                return;
            };
            $(
                $(#[cfg($cfg)])?
                {
                    let $backend = server.backend().await;
                    $body
                }
            )+
        }
    };
}

fn skills_root() -> VirtualPath {
    VirtualPath::new(SKILLS_ROOT).expect("skills root")
}

fn lock_path() -> VirtualPath {
    VirtualPath::new(format!("{SKILLS_ROOT}/{LOCK_ENTRY_FILE}")).expect("lock path")
}

/// A lock record as another process would write it.
struct Record {
    pid: u32,
    host: HostIdentity,
    process: String,
    acquired_at: DateTime<Utc>,
    lease: Duration,
    renewals: u64,
    nonce: &'static str,
}

impl Record {
    /// A holder in another process on this host, with the default lease.
    fn local(nonce: &'static str) -> Self {
        Self {
            pid: std::process::id(),
            host: HostIdentity::current(),
            process: "another-process".to_string(),
            acquired_at: Utc::now(),
            lease: InstallLockPolicy::default().lease,
            renewals: 0,
            nonce,
        }
    }

    /// A holder on another host (or in another container), with the default lease.
    fn remote(nonce: &'static str) -> Self {
        Self {
            pid: 4242,
            host: other_host(),
            ..Self::local(nonce)
        }
    }

    fn body(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "format": 1,
            "pid": self.pid,
            "host": {
                "hostname": self.host.hostname,
                "boot_id": self.host.boot_id,
                "pid_namespace": self.host.pid_namespace,
            },
            "process": self.process,
            "acquired_at": self.acquired_at.to_rfc3339(),
            "lease_ms": u64::try_from(self.lease.as_millis()).expect("lease fits"),
            "renewals": self.renewals,
            "nonce": self.nonce,
        }))
        .expect("holder json")
    }
}

fn other_host() -> HostIdentity {
    HostIdentity {
        hostname: Some("some-other-host".to_string()),
        boot_id: Some("00000000-0000-0000-0000-000000000000".to_string()),
        pid_namespace: Some("pid:[1]".to_string()),
    }
}

fn short_wait() -> InstallLockPolicy {
    InstallLockPolicy {
        wait_timeout: Duration::from_millis(400),
        ..InstallLockPolicy::default()
    }
}

/// A policy with a lease short enough for a test to outlast it.
fn short_lease() -> InstallLockPolicy {
    InstallLockPolicy {
        wait_timeout: Duration::from_secs(5),
        lease: Duration::from_secs(1),
        renew_interval: Duration::from_millis(50),
        first_poll: Duration::from_millis(10),
        max_poll: Duration::from_millis(50),
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

async fn lock_nonce(filesystem: &dyn RootFilesystem) -> Option<String> {
    lock_body(filesystem)
        .await
        .and_then(|body| InstallLockHolder::decode(&body))
        .map(|holder| holder.nonce)
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

/// Simulates a live holder in another process: rewrites `record` with its renewal counter
/// incremented every `every`, until the returned handle is aborted.
fn keep_renewing(
    filesystem: Arc<dyn RootFilesystem>,
    mut record: Record,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            record.renewals += 1;
            filesystem
                .put(
                    &lock_path(),
                    Entry::bytes(record.body()),
                    CasExpectation::Any,
                )
                .await
                .expect("renew the simulated holder's lease");
        }
    })
}

#[test]
fn default_lease_lets_a_restart_after_a_crash_succeed_within_the_wait() {
    let policy = InstallLockPolicy::default();
    // A starter waits one full lease (plus at most one pause between looks) before replacing an
    // abandoned entry; that has to fit comfortably inside the wait.
    assert!(policy.lease + policy.max_poll <= policy.wait_timeout / 2);
    // The holder stops after half a lease without a renewal; it must get several attempts first.
    assert!(policy.renew_interval * 2 <= policy.lease / 2);
    assert!(policy.lease <= MAX_HONOURED_LEASE);
}

lock_scenarios! {
    /// A replacement container cannot probe the old holder's pid, and the holder's clock says
    /// nothing trustworthy, so an abandoned lock is recognised by its lease not being renewed.
    async fn abandoned_lock_from_another_host_is_taken_over_once_its_lease_runs_out(backend) {
        let filesystem = backend.filesystem.as_ref();
        let lease = Duration::from_millis(500);
        write_lock(
            filesystem,
            Record {
                lease,
                acquired_at: minutes_ago(1),
                ..Record::remote("abandoned")
            }
            .body(),
        )
        .await;

        let started = std::time::Instant::now();
        ensure_bundled_reborn_skills_installed_in(filesystem, &skills_root())
            .await
            .unwrap_or_else(|error| panic!("{}: install failed: {error}", backend.name));

        assert!(
            started.elapsed() >= lease,
            "{}: the lock was taken over before its lease ran out",
            backend.name
        );
        assert_installed(filesystem, backend.name).await;
    }

    /// The holder's recorded time is never compared with the starter's clock: a holder whose clock
    /// is far behind (or a starter whose clock is far ahead) keeps its lock while it renews.
    async fn renewed_lock_is_kept_however_old_its_recorded_time(backend) {
        let filesystem = backend.filesystem.as_ref();
        let record = Record {
            lease: Duration::from_secs(1),
            acquired_at: Utc::now() - chrono::Duration::days(1),
            ..Record::remote("renewing")
        };
        write_lock(filesystem, record.body()).await;
        let renewer = keep_renewing(
            Arc::clone(&backend.filesystem),
            record,
            Duration::from_millis(50),
        );

        let error = install_bundled_skills(
            filesystem,
            &skills_root(),
            None,
            &InstallLockPolicy {
                wait_timeout: Duration::from_millis(2500),
                ..InstallLockPolicy::default()
            },
        )
        .await
        .expect_err("a holder that keeps renewing must not be taken over");
        renewer.abort();

        assert!(
            error.to_string().contains("some-other-host"),
            "{}: {error}",
            backend.name
        );
        assert_eq!(
            lock_nonce(filesystem).await.as_deref(),
            Some("renewing"),
            "{}: the renewing holder's lock was replaced",
            backend.name
        );
    }

    /// A live install that outlasts its lease many times over is never interrupted, because it
    /// renews; the next starter gets the lock once it is released.
    async fn live_install_keeps_its_lock_past_the_lease_by_renewing_it(backend) {
        let policy = short_lease();
        let mut first = BundledSkillInstallLock::acquire(
            backend.filesystem.as_ref(),
            &skills_root(),
            None,
            &policy,
        )
        .await
        .expect("first starter acquires");
        let first_done = Arc::new(AtomicBool::new(false));

        let second = {
            let filesystem = Arc::clone(&backend.filesystem);
            let policy = policy.clone();
            let first_done = Arc::clone(&first_done);
            tokio::spawn(async move {
                let lock = BundledSkillInstallLock::acquire(
                    filesystem.as_ref(),
                    &skills_root(),
                    None,
                    &policy,
                )
                .await;
                (lock, first_done.load(Ordering::SeqCst))
            })
        };

        let work_time = policy.lease * 3;
        first
            .hold_while(backend.filesystem.as_ref(), async {
                tokio::time::sleep(work_time).await;
                Ok(())
            })
            .await
            .unwrap_or_else(|error| panic!("{}: live install interrupted: {error}", backend.name));
        first_done.store(true, Ordering::SeqCst);
        first
            .release(backend.filesystem.as_ref())
            .await
            .expect("first releases");

        let (second, saw_first_done) = second.await.expect("second starter task");
        let second = second
            .unwrap_or_else(|error| panic!("{}: second starter failed: {error}", backend.name));
        assert!(
            saw_first_done,
            "{}: the second starter took the lock while the first was still installing",
            backend.name
        );
        second
            .release(backend.filesystem.as_ref())
            .await
            .expect("second releases");
    }

    /// Once another starter may hold the lock, the old holder must stop writing.
    async fn install_that_loses_its_lock_stops_and_leaves_the_new_owner_alone(backend) {
        let filesystem = backend.filesystem.as_ref();
        let mut lock =
            BundledSkillInstallLock::acquire(filesystem, &skills_root(), None, &short_lease())
                .await
                .expect("acquire");
        let new_owner = Record::remote("new-owner").body();

        let held = lock.hold_while(filesystem, async {
            filesystem
                .put(
                    &lock_path(),
                    Entry::bytes(new_owner.clone()),
                    CasExpectation::Any,
                )
                .await
                .expect("another starter takes the lock over");
            futures::future::pending::<Result<(), RebornBuildError>>().await
        });
        let error = tokio::time::timeout(Duration::from_secs(5), held)
            .await
            .unwrap_or_else(|_| panic!("{}: the install kept running", backend.name))
            .expect_err("losing the lock must stop the install");
        lock.release(filesystem).await.expect("release");

        assert!(
            error.to_string().contains("took it over"),
            "{}: {error}",
            backend.name
        );
        assert_eq!(
            lock_body(filesystem).await,
            Some(new_owner),
            "{}",
            backend.name
        );
    }

    /// A holder that cannot renew stops itself before any starter could see its lease run out.
    async fn install_whose_renewals_keep_failing_stops_within_half_its_lease(backend) {
        let filesystem = Tapped::new(Arc::clone(&backend.filesystem));
        let policy = short_lease();
        let mut lock = BundledSkillInstallLock::acquire(&filesystem, &skills_root(), None, &policy)
            .await
            .expect("acquire");
        filesystem.fail_writes.store(true, Ordering::SeqCst);

        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            lock.hold_while(&filesystem, futures::future::pending::<Result<(), _>>()),
        )
        .await
        .unwrap_or_else(|_| panic!("{}: the install kept running", backend.name))
        .expect_err("an install that cannot renew must stop");

        assert!(
            started.elapsed() < policy.lease,
            "{}: stopped only after {:?}, when a starter could already have taken over",
            backend.name,
            started.elapsed()
        );
        assert!(
            error.to_string().contains("lost"),
            "{}: {error}",
            backend.name
        );
    }

    /// The pid check needs the kernel's boot id and pid namespace, which only Linux exposes here.
    #[cfg(target_os = "linux")]
    async fn lock_left_by_a_dead_process_on_this_host_is_taken_over_at_once(backend) {
        let filesystem = backend.filesystem.as_ref();
        write_lock(
            filesystem,
            Record {
                pid: FREE_PID,
                ..Record::local("dead")
            }
            .body(),
        )
        .await;

        let started = std::time::Instant::now();
        ensure_bundled_reborn_skills_installed_in(filesystem, &skills_root())
            .await
            .unwrap_or_else(|error| panic!("{}: install failed: {error}", backend.name));

        assert!(
            started.elapsed() < InstallLockPolicy::default().lease / 2,
            "{}: a dead holder's lock must be taken over without waiting for its lease",
            backend.name
        );
        assert_installed(filesystem, backend.name).await;
    }

    /// An install cancelled in this process drops its lock without releasing it. The retry in the
    /// same process must not wait on its own leftover.
    async fn lock_left_by_a_cancelled_install_in_this_process_is_taken_over_at_once(backend) {
        let filesystem = backend.filesystem.as_ref();
        let cancelled = BundledSkillInstallLock::acquire(
            filesystem,
            &skills_root(),
            None,
            &InstallLockPolicy::default(),
        )
        .await
        .expect("acquire");
        drop(cancelled);
        assert!(lock_body(filesystem).await.is_some(), "{}", backend.name);

        install_bundled_skills(filesystem, &skills_root(), None, &short_wait())
            .await
            .unwrap_or_else(|error| panic!("{}: retry failed: {error}", backend.name));

        assert_installed(filesystem, backend.name).await;
    }

    async fn lock_held_by_a_live_process_is_waited_on_and_the_timeout_names_the_holder(backend) {
        let filesystem = backend.filesystem.as_ref();
        let host = HostIdentity::current();
        let acquired_at = minutes_ago(1);
        let held = Record {
            acquired_at,
            ..Record::local("live")
        }
        .body();
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

    /// A waiting starter backs off instead of reading the entry every few milliseconds.
    async fn waiting_starter_backs_off_between_looks(backend) {
        write_lock(backend.filesystem.as_ref(), Record::remote("busy").body()).await;
        let filesystem = Tapped::new(Arc::clone(&backend.filesystem));
        let policy = InstallLockPolicy {
            wait_timeout: Duration::from_secs(3),
            ..InstallLockPolicy::default()
        };

        BundledSkillInstallLock::acquire(&filesystem, &skills_root(), None, &policy)
            .await
            .err()
            .unwrap_or_else(|| panic!("{}: a live holder was taken over", backend.name));

        // 50 ms doubling to 1 s is under ten looks in 3 s; a fixed 25 ms pause would be 120.
        let operations = filesystem.operations.load(Ordering::SeqCst);
        assert!(
            operations <= 20,
            "{}: {operations} filesystem calls while waiting 3 s",
            backend.name
        );
    }

    async fn old_format_lock_is_taken_over_once_past_the_grace_period(backend) {
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

    async fn fresh_old_format_lock_is_waited_on(backend) {
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

    /// A pid is only meaningful on the host (and boot, and pid namespace) that issued it: a lock
    /// from another host is waited on even when the same pid number is free here.
    async fn lock_from_another_host_is_waited_on_whatever_its_pid(backend) {
        let filesystem = backend.filesystem.as_ref();
        let held = Record {
            pid: FREE_PID,
            ..Record::remote("remote")
        }
        .body();
        write_lock(filesystem, held.clone()).await;

        let error = install_bundled_skills(filesystem, &skills_root(), None, &short_wait())
            .await
            .expect_err("a lock from another host must be waited on");

        assert!(
            error.to_string().contains("some-other-host"),
            "{}: {error}",
            backend.name
        );
        assert_eq!(lock_body(filesystem).await, Some(held), "{}", backend.name);
    }

    async fn two_starters_racing_for_a_stale_lock_produce_exactly_one_holder(backend) {
        // A lease of zero is stale at first sight, so both starters judge it stale at once.
        write_lock(
            backend.filesystem.as_ref(),
            Record {
                lease: Duration::ZERO,
                ..Record::remote("stale")
            }
            .body(),
        )
        .await;
        let filesystem = Arc::new(Tapped::pausing_takeovers(Arc::clone(&backend.filesystem)));
        let policy = InstallLockPolicy {
            wait_timeout: Duration::from_secs(20),
            ..InstallLockPolicy::default()
        };
        let starter = |filesystem: Arc<Tapped>, policy: InstallLockPolicy| {
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

    async fn a_holder_that_was_taken_over_does_not_delete_the_new_owners_lock(backend) {
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
        let new_owner = Record::remote("new-owner").body();
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

/// A test double over a real backend: counts calls, can fail every write, and can hold back
/// take-over writes until two of them are in flight.
struct Tapped {
    inner: Arc<dyn RootFilesystem>,
    operations: AtomicUsize,
    fail_writes: AtomicBool,
    takeover_gate: Option<Barrier>,
    takeovers: AtomicUsize,
}

impl Tapped {
    fn new(inner: Arc<dyn RootFilesystem>) -> Self {
        Self {
            inner,
            operations: AtomicUsize::new(0),
            fail_writes: AtomicBool::new(false),
            takeover_gate: None,
            takeovers: AtomicUsize::new(0),
        }
    }

    /// Holds back every take-over write until two are in flight, so both starters have read
    /// the same stale lock before either tries to replace it.
    fn pausing_takeovers(inner: Arc<dyn RootFilesystem>) -> Self {
        Self {
            takeover_gate: Some(Barrier::new(2)),
            ..Self::new(inner)
        }
    }
}

#[async_trait]
impl RootFilesystem for Tapped {
    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    async fn put(
        &self,
        path: &VirtualPath,
        entry: Entry,
        cas: CasExpectation,
    ) -> Result<RecordVersion, FilesystemError> {
        self.operations.fetch_add(1, Ordering::SeqCst);
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(FilesystemError::Backend {
                path: path.clone(),
                operation: FilesystemOperation::WriteFile,
                reason: "injected write failure".to_string(),
            });
        }
        if let Some(gate) = &self.takeover_gate
            && matches!(cas, CasExpectation::Version(_))
            && self.takeovers.fetch_add(1, Ordering::SeqCst) < 2
        {
            gate.wait().await;
        }
        self.inner.put(path, entry, cas).await
    }

    async fn get(&self, path: &VirtualPath) -> Result<Option<VersionedEntry>, FilesystemError> {
        self.operations.fetch_add(1, Ordering::SeqCst);
        self.inner.get(path).await
    }

    async fn list_dir(&self, path: &VirtualPath) -> Result<Vec<DirEntry>, FilesystemError> {
        self.operations.fetch_add(1, Ordering::SeqCst);
        self.inner.list_dir(path).await
    }

    async fn stat(&self, path: &VirtualPath) -> Result<FileStat, FilesystemError> {
        self.operations.fetch_add(1, Ordering::SeqCst);
        self.inner.stat(path).await
    }

    async fn delete(&self, path: &VirtualPath) -> Result<(), FilesystemError> {
        self.operations.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(path).await
    }

    async fn delete_if_version(
        &self,
        path: &VirtualPath,
        expected_version: RecordVersion,
    ) -> Result<(), FilesystemError> {
        self.operations.fetch_add(1, Ordering::SeqCst);
        self.inner.delete_if_version(path, expected_version).await
    }
}

fn standalone_lock_file(standalone_root: &std::path::Path) -> std::path::PathBuf {
    standalone_root.join("system/skills").join(LOCK_ENTRY_FILE)
}

fn prepare_standalone_root() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let standalone_root = dir.path().join("standalone");
    let lock_file = standalone_lock_file(&standalone_root);
    std::fs::create_dir_all(lock_file.parent().expect("parent")).expect("skills dir");
    (dir, standalone_root, lock_file)
}

/// The incident shape: an install killed mid-way on a host disk leaves the lock behind, and the
/// next start of the same server home must not hang on it.
#[tokio::test]
async fn disk_lock_left_by_a_killed_install_is_taken_over() {
    let (_dir, standalone_root, lock_file) = prepare_standalone_root();
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

/// Holding the OS lock proves only that no other installer on this host that could take the OS
/// lock is running. A writer that could not take it (no lock support on its mount, or another
/// host sharing the directory) may still be renewing the entry, so the entry's lease decides.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_entry_renewed_by_a_writer_without_the_host_lock_is_not_taken_over() {
    let (_dir, standalone_root, lock_file) = prepare_standalone_root();
    let mut record = Record {
        lease: Duration::from_secs(1),
        ..Record::remote("renewing")
    };
    std::fs::write(&lock_file, record.body()).expect("live lock");
    let stop = Arc::new(AtomicBool::new(false));
    let renewer = {
        let stop = Arc::clone(&stop);
        let lock_file = lock_file.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                record.renewals += 1;
                let staging = lock_file.with_extension("renewing");
                std::fs::write(&staging, record.body()).expect("stage renewal");
                std::fs::rename(&staging, &lock_file).expect("publish renewal");
            }
        })
    };

    let error = install_into_standalone_root(
        &standalone_root,
        &InstallLockPolicy {
            wait_timeout: Duration::from_millis(2500),
            ..InstallLockPolicy::default()
        },
    )
    .await
    .expect_err("a renewed entry must be waited on even while the host lock is free");
    stop.store(true, Ordering::SeqCst);
    renewer.join().expect("renewer thread");

    assert!(error.to_string().contains("some-other-host"), "{error}");
    let body = std::fs::read(&lock_file).expect("read lock");
    assert_eq!(
        InstallLockHolder::decode(&body).map(|holder| holder.nonce),
        Some("renewing".to_string())
    );
}

#[tokio::test]
async fn disk_entry_no_longer_renewed_is_taken_over_once_its_lease_runs_out() {
    let (_dir, standalone_root, lock_file) = prepare_standalone_root();
    std::fs::write(
        &lock_file,
        Record {
            lease: Duration::from_millis(300),
            ..Record::remote("abandoned")
        }
        .body(),
    )
    .expect("abandoned lock");

    install_into_standalone_root(&standalone_root, &short_lease())
        .await
        .expect("install over an abandoned lock entry");

    assert!(!lock_file.exists());
}

#[tokio::test]
async fn disk_install_waits_for_a_live_holder_of_the_host_lock() {
    let (_dir, standalone_root, lock_file) = prepare_standalone_root();
    let acquired_at = minutes_ago(1);
    let held = Record {
        acquired_at,
        ..Record::local("live")
    }
    .body();
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

/// The open itself refuses a symlink, so nothing can swap one in between a check and the open.
#[cfg(unix)]
#[test]
fn host_lock_file_that_is_a_symlink_is_refused_by_the_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("elsewhere");
    std::fs::write(&target, b"not yours").expect("symlink target");
    let link = dir.path().join(HOST_LOCK_FILE);
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let error = open_host_lock_file(&link).expect_err("a symlink must not be opened");

    assert!(error.to_string().contains("symlink"), "{error}");
    assert_eq!(std::fs::read(&target).expect("read target"), b"not yours");
}
