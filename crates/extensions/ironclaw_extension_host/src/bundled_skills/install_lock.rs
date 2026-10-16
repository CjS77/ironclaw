//! Cross-process lock around the startup install of bundled skills.
//!
//! # The lock
//!
//! The lock is an entry, [`LOCK_ENTRY_FILE`], directly under the skill root. It is created with
//! `CasExpectation::Absent`, so of several processes starting against one skill root only one
//! installs at a time. Its body is an [`InstallLockHolder`] record: who holds the lock (pid,
//! [`HostIdentity`], a per-process instance id), when it took the lock, the lease length it
//! promises to renew within, a renewal counter, and a random nonce.
//!
//! # The lease
//!
//! The holder renews the entry every [`InstallLockPolicy::renew_interval`] (2 s) for as long as
//! the install runs ([`BundledSkillInstallLock::hold_while`]). Each renewal rewrites the record
//! with its counter incremented, so the entry's body and, on versioned backends, its version
//! change. Renewal runs inside the same future as the install, so it stops as soon as the install
//! finishes, the lock is released, or the install future is dropped.
//!
//! A starter that finds an entry watches it. The lease has expired once the starter has itself
//! seen the entry stay unchanged for the lease length the record declares (by default
//! [`InstallLockPolicy::lease`], 10 s). The starter measures that with its own monotonic clock,
//! from the first time it saw the current body, so no clock is ever compared with another: not
//! the holder's, not the database server's. Clock skew between hosts, and pid namespaces that
//! hide the holder's process, do not matter. The cost is that a starter always waits one full
//! lease before replacing an abandoned entry, even one abandoned long ago; with a 10 s lease and
//! a 30 s wait ([`InstallLockPolicy::wait_timeout`]) a restart after a crash still succeeds. A
//! declared lease is honoured up to [`MAX_HONOURED_LEASE`], so a corrupt record cannot hold the
//! root for longer than that.
//!
//! The holder treats the lease as lost once no renewal has succeeded for half the lease. A
//! renewal that finds the entry replaced (another starter took it over), or renewals that keep
//! failing for that long, stop the install with an error, so by the time a starter can take
//! over, the old holder has stopped writing. Half the lease leaves room for writes already in
//! flight. A storage outage longer than that during the install therefore fails this start; the
//! next start installs again (the install is idempotent).
//!
//! # Faster recovery where it is safe
//!
//! Two cases need no wait, because the holder is known to be gone:
//!
//! - **The holder ran in this pid space and its pid is gone.** A pid means something only on the
//!   machine, boot and pid namespace that issued it, so this check runs only on Unix and only
//!   when the record's hostname, kernel boot id and pid namespace all equal this process's. A
//!   holder from another host or container falls back to the lease.
//! - **The holder was this process, and the lock is no longer held here.** An install future
//!   dropped mid-way (cancelled) leaves its entry behind. The record's instance id says this
//!   process wrote it, and the process-wide set of held nonces says no live lock in this process
//!   owns it, so a retry takes it over at once instead of waiting out its own lease.
//!
//! # Records in an unrecognised format
//!
//! A record that cannot be read (for example one written by an older binary, whose lock body was
//! a monotonic clock reading and which never renews) is judged by the entry's modification time
//! and is stale once older than [`InstallLockPolicy::unrecognised_grace`] (2 minutes, longer
//! than such an install took even under heavy load). This is the one place a second clock is
//! consulted (the backend's, against this process's), because such an entry carries nothing
//! else to go on; the risk is limited to an older binary starting at the same moment as this
//! one. Every backend reports a modification time for a plain file entry; one that does not
//! gives nothing to wait for, so the entry is taken over at once, since waiting instead would
//! leave the root locked forever.
//!
//! A live holder is waited on for [`InstallLockPolicy::wait_timeout`] (30 s), after which the
//! error names the holder and when it took the lock. While waiting, a starter looks at the entry
//! with exponential backoff, from [`InstallLockPolicy::first_poll`] (50 ms) up to
//! [`InstallLockPolicy::max_poll`] (1 s), with jitter, one read per look.
//!
//! # Replacing a stale lock without a race
//!
//! How a stale entry is replaced, how a holder renews, and how it releases depend on what the
//! backend offers ([`Arbiter`]):
//!
//! - **Versioned backends** (in-memory, libSQL, PostgreSQL). Take-over is a `put` with
//!   `CasExpectation::Version` of the stale entry, so when two starters judge the same entry stale
//!   only the first write lands; the other gets a version mismatch, watches the new holder and
//!   waits. Renewal is a `put` on the version this holder last wrote, and release is
//!   `delete_if_version` on it, so a holder that was taken over can neither renew nor delete its
//!   successor's lock.
//! - **Host disk** (the standalone storage root). The disk backend has no entry versions, so the
//!   installer also holds an exclusive OS file lock on [`HOST_LOCK_FILE`] in the storage root for
//!   as long as it holds the entry. Installers on one host that can take the OS lock are
//!   serialised by it, so take-over among them is a plain overwrite. The entry is still judged by
//!   its lease, because a writer that could not take the OS lock (for example on an NFS mount
//!   without lock support, or on another host sharing the directory) may be renewing it.
//!   Renewal and release first check that the entry still carries this holder's nonce. If the
//!   storage root's filesystem cannot take OS file locks, the disk path falls back to the entry
//!   alone: live installers still exclude each other through the atomic create, but a stale entry
//!   cannot be replaced safely, and the error says to remove it by hand.
//!
//! The shared bounded CAS helper (`ironclaw_filesystem::cas_update`) does not fit: it retries a
//! read-modify-write until the write lands, whereas a lock attempt must stop at the first conflict
//! and judge the new holder; and it works on a `ScopedFilesystem`, while this lock lives on the
//! `RootFilesystem` the install is given.
//!
//! Two narrow windows remain:
//!
//! - On versioned backends a version number restarts at 1 when an entry is deleted and created
//!   again, so a starter that read a stale entry and then stalled between that read and its
//!   take-over write — for longer than another starter's whole take-over, install and release,
//!   and a third starter's fresh acquire — could find its expected version current again and
//!   replace a live lock.
//! - On the host disk the nonce check before a renewal is a separate read, so a holder without
//!   the OS lock whose renewal write stalls for more than half a lease can overwrite a successor
//!   that took over in between. The successor's next renewal then finds the entry is no longer
//!   its own and stops its install, so only one install keeps running.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use ironclaw_filesystem::{
    CasExpectation, Entry, FilesystemError, RecordVersion, RootFilesystem, VersionedEntry,
};
use ironclaw_host_api::path::VirtualPath;
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep, timeout_at};

use super::{child_path, delete_if_exists, invalid_config};
use crate::RebornBuildError;

/// The lock entry, directly under the skill root.
pub(super) const LOCK_ENTRY_FILE: &str = ".ironclaw-reborn-bundled.lock";
/// The OS-locked file in a standalone storage root. It is never deleted: removing a file that
/// another process may have open and be waiting to lock would let two processes lock different
/// files under one name.
pub(super) const HOST_LOCK_FILE: &str = ".ironclaw-reborn-bundled-skills.host-lock";

/// The current [`InstallLockHolder`] format; a record carrying any other value is unrecognised.
const HOLDER_FORMAT: u8 = 1;
/// The longest lease a record may declare and have honoured. A starter treats a longer declared
/// lease as this long, so a corrupt record cannot lock the skill root for longer.
pub(super) const MAX_HONOURED_LEASE: Duration = Duration::from_secs(60);
/// Longest host-identity field shown in an error or log line. The fields come from whichever
/// process wrote the lock, possibly on another host, so they are bounded and escaped when shown.
const SHOWN_FIELD_MAX_CHARS: usize = 64;

/// How long to wait for a live holder, how long a lease lasts and how often it is renewed. The
/// defaults are the production values; see the module docs for why each is what it is.
#[derive(Debug, Clone)]
pub(super) struct InstallLockPolicy {
    /// How long a starter waits for a live holder before failing.
    pub(super) wait_timeout: Duration,
    /// The lease this process declares when it takes the lock: how long a starter must see the
    /// entry unchanged before taking it over.
    pub(super) lease: Duration,
    /// How often the holder renews its lease while the install runs.
    pub(super) renew_interval: Duration,
    /// First pause between looks at a live holder's entry; it doubles after each look.
    pub(super) first_poll: Duration,
    /// Longest pause between looks at a live holder's entry.
    pub(super) max_poll: Duration,
    /// Age after which an entry whose record cannot be read is stale.
    pub(super) unrecognised_grace: Duration,
}

impl Default for InstallLockPolicy {
    fn default() -> Self {
        Self {
            wait_timeout: Duration::from_secs(30),
            lease: Duration::from_secs(10),
            renew_interval: Duration::from_secs(2),
            first_poll: Duration::from_millis(50),
            max_poll: Duration::from_secs(1),
            unrecognised_grace: Duration::from_secs(2 * 60),
        }
    }
}

/// Where a process runs, as far as pids are concerned. Each field is `None` where the platform
/// does not expose it; two identities share a pid space only when all three are known and equal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct HostIdentity {
    pub(super) hostname: Option<String>,
    /// The kernel's per-boot id (`/proc/sys/kernel/random/boot_id`). It distinguishes a pid issued
    /// before a reboot from the same number issued after it.
    pub(super) boot_id: Option<String>,
    /// The pid namespace (`/proc/self/ns/pid`). It distinguishes containers that share a kernel.
    pub(super) pid_namespace: Option<String>,
}

impl HostIdentity {
    pub(super) fn current() -> Self {
        Self {
            hostname: current_hostname(),
            boot_id: read_trimmed(Path::new("/proc/sys/kernel/random/boot_id")),
            pid_namespace: std::fs::read_link("/proc/self/ns/pid")
                .ok()
                .map(|target| target.to_string_lossy().into_owned()),
        }
    }

    fn shares_pid_space_with(&self, other: &Self) -> bool {
        fn known_and_equal(left: &Option<String>, right: &Option<String>) -> bool {
            matches!((left, right), (Some(left), Some(right)) if left == right)
        }
        known_and_equal(&self.hostname, &other.hostname)
            && known_and_equal(&self.boot_id, &other.boot_id)
            && known_and_equal(&self.pid_namespace, &other.pid_namespace)
    }
}

#[cfg(unix)]
fn current_hostname() -> Option<String> {
    nix::unistd::gethostname()
        .ok()
        .map(|name| name.to_string_lossy().trim().to_string())
        .filter(|name| !name.is_empty())
}

#[cfg(not(unix))]
fn current_hostname() -> Option<String> {
    None
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A random id for this process, written into every record it holds. It tells this process's
/// own records apart from every other process's, whatever the pid space.
fn process_instance() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| format!("{:016x}", rand::random::<u64>()))
}

/// The nonces of the locks this process currently holds. A record written by this process whose
/// nonce is not here was left behind by a lock that was dropped without being released.
fn held_here() -> MutexGuard<'static, HashSet<String>> {
    static HELD: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    // Every use of the set is a single insert, remove or lookup, so a panic elsewhere cannot
    // leave it half-updated and a poisoned guard is still sound to use.
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Marks a nonce as held by this process until dropped. It is registered before the lock entry
/// is written, so another task in this process never sees the entry without the mark.
struct HeldHere {
    nonce: String,
}

impl HeldHere {
    fn register(nonce: &str) -> Self {
        held_here().insert(nonce.to_string());
        Self {
            nonce: nonce.to_string(),
        }
    }
}

impl Drop for HeldHere {
    fn drop(&mut self) {
        held_here().remove(&self.nonce);
    }
}

/// The body of the lock entry: who holds the lock, since when, and the lease it keeps renewing.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallLockHolder {
    format: u8,
    pid: u32,
    host: HostIdentity,
    /// The writing process's instance id ([`process_instance`]).
    process: String,
    /// When the holder took the lock, by its own clock. Shown in errors; never used to judge
    /// whether the lock is stale.
    acquired_at: DateTime<Utc>,
    /// How long a starter must see this record unchanged before taking the lock over.
    lease_ms: u64,
    /// Incremented by every renewal, so that each renewal changes the entry's body.
    renewals: u64,
    /// Random per acquisition; tells this holder's entry apart from a successor's where the
    /// backend has no entry versions.
    nonce: String,
}

impl InstallLockHolder {
    fn new(host: HostIdentity, lease: Duration) -> Self {
        Self {
            format: HOLDER_FORMAT,
            pid: std::process::id(),
            host,
            process: process_instance().to_string(),
            acquired_at: Utc::now(),
            lease_ms: u64::try_from(lease.as_millis()).unwrap_or(u64::MAX),
            renewals: 0,
            nonce: format!("{:016x}", rand::random::<u64>()),
        }
    }

    fn decode(body: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(body)
            .ok()
            .filter(|holder| holder.format == HOLDER_FORMAT)
    }

    fn entry(&self) -> Result<Entry, RebornBuildError> {
        serde_json::to_vec(self).map(Entry::bytes).map_err(|error| {
            invalid_config(format!(
                "failed to encode the bundled skill install lock record: {error}"
            ))
        })
    }

    /// The lease a starter honours for this record.
    fn lease(&self) -> Duration {
        Duration::from_millis(self.lease_ms).min(MAX_HONOURED_LEASE)
    }

    fn describe(&self) -> String {
        format!(
            "pid {} on host {} since {} (lease {}s)",
            self.pid,
            shown(self.host.hostname.as_deref()),
            self.acquired_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            self.lease().as_secs_f32()
        )
    }
}

fn shown(value: Option<&str>) -> String {
    match value {
        Some(value) => format!(
            "{:?}",
            value
                .chars()
                .take(SHOWN_FIELD_MAX_CHARS)
                .collect::<String>()
        ),
        None => "unknown".to_string(),
    }
}

/// What a starter found in an existing lock entry.
enum ObservedHolder {
    Holder(InstallLockHolder),
    /// The body is not a current-format record; `modified` is the entry's modification time
    /// where the backend reports one.
    Unrecognised {
        modified: Option<SystemTime>,
    },
}

impl ObservedHolder {
    async fn read(
        filesystem: &dyn RootFilesystem,
        path: &VirtualPath,
        entry: &VersionedEntry,
    ) -> Self {
        if let Some(holder) = InstallLockHolder::decode(&entry.entry.body) {
            return Self::Holder(holder);
        }
        // A failed stat leaves the age unknown, which the staleness rule treats like a backend
        // that keeps no modification times.
        let modified = filesystem
            .stat(path)
            .await
            .ok()
            .and_then(|stat| stat.modified);
        Self::Unrecognised { modified }
    }

    fn describe(&self) -> String {
        match self {
            Self::Holder(holder) => holder.describe(),
            Self::Unrecognised {
                modified: Some(modified),
            } => format!(
                "a lock entry in an unrecognised format, last modified {}",
                DateTime::<Utc>::from(*modified).to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            Self::Unrecognised { modified: None } => {
                "a lock entry in an unrecognised format".to_string()
            }
        }
    }
}

/// What a starter last saw in the lock entry, and since when by its own monotonic clock.
struct Watch {
    version: RecordVersion,
    body: Vec<u8>,
    since: Instant,
}

impl Watch {
    /// Records a look at `current` and returns how long the entry has looked the same.
    fn observe(watch: &mut Option<Self>, current: &VersionedEntry) -> Duration {
        let now = Instant::now();
        match watch {
            Some(seen) if seen.version == current.version && seen.body == current.entry.body => {
                now.saturating_duration_since(seen.since)
            }
            _ => {
                *watch = Some(Self {
                    version: current.version,
                    body: current.entry.body.clone(),
                    since: now,
                });
                Duration::ZERO
            }
        }
    }
}

/// What decides that a stale entry may be replaced, and how a holder renews and releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arbiter {
    /// Compare-and-swap on the entry's version.
    EntryVersion,
    /// This process holds the OS lock on [`HOST_LOCK_FILE`].
    HostFileLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleReason {
    /// The holder ran in this pid space and its pid no longer names a process.
    HolderExited,
    /// The holder was this process and no lock in this process holds it any more.
    LeftByThisProcess,
    /// The entry stayed unchanged for longer than its lease.
    LeaseExpired,
    /// An unreadable entry older than the grace period.
    UnrecognisedExpired,
    /// An unreadable entry on a backend that reports no modification time.
    UnrecognisedUndated,
}

impl StaleReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::HolderExited => "holder process exited",
            Self::LeftByThisProcess => "left behind by a cancelled install in this process",
            Self::LeaseExpired => "lease was not renewed in time",
            Self::UnrecognisedExpired => "unrecognised record older than the grace period",
            Self::UnrecognisedUndated => "unrecognised record with no modification time",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Live,
    Stale(StaleReason),
}

/// Whether `pid` certainly names no process in this pid space.
#[cfg(unix)]
fn pid_is_gone(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // `kill` treats 0 and negative numbers as process groups, so only a positive pid is probed.
    let Some(raw) = i32::try_from(pid).ok().filter(|raw| *raw > 0) else {
        return false;
    };
    // Signal 0 checks for existence without delivering anything. EPERM means the process exists
    // but belongs to another user; any other error leaves the answer unknown.
    matches!(kill(Pid::from_raw(raw), None), Err(Errno::ESRCH))
}

/// Without a way to probe a pid, no holder is ever known to be gone; the lease decides.
#[cfg(not(unix))]
fn pid_is_gone(_pid: u32) -> bool {
    false
}

/// Decides whether the holder of an existing lock entry may still be working, given how long
/// this starter has seen the entry unchanged.
fn judge(
    observed: &ObservedHolder,
    unchanged_for: Duration,
    local: &HostIdentity,
    policy: &InstallLockPolicy,
) -> Verdict {
    match observed {
        ObservedHolder::Holder(holder) => {
            if holder.process == process_instance() && !held_here().contains(&holder.nonce) {
                Verdict::Stale(StaleReason::LeftByThisProcess)
            } else if local.shares_pid_space_with(&holder.host) && pid_is_gone(holder.pid) {
                Verdict::Stale(StaleReason::HolderExited)
            } else if unchanged_for >= holder.lease() {
                Verdict::Stale(StaleReason::LeaseExpired)
            } else {
                Verdict::Live
            }
        }
        ObservedHolder::Unrecognised {
            modified: Some(modified),
        } => {
            let age = SystemTime::now()
                .duration_since(*modified)
                .unwrap_or(Duration::ZERO);
            if age >= policy.unrecognised_grace {
                Verdict::Stale(StaleReason::UnrecognisedExpired)
            } else {
                Verdict::Live
            }
        }
        ObservedHolder::Unrecognised { modified: None } => {
            Verdict::Stale(StaleReason::UnrecognisedUndated)
        }
    }
}

fn log_take_over(
    path: &VirtualPath,
    observed: &ObservedHolder,
    unchanged_for: Duration,
    reason: StaleReason,
) {
    match observed {
        ObservedHolder::Holder(holder) => tracing::debug!(
            lock = %path,
            previous_pid = holder.pid,
            previous_host = %shown(holder.host.hostname.as_deref()),
            unchanged_for_ms = u64::try_from(unchanged_for.as_millis()).unwrap_or(u64::MAX),
            reason = reason.as_str(),
            "took over a stale bundled skill install lock"
        ),
        ObservedHolder::Unrecognised { modified } => tracing::debug!(
            lock = %path,
            unchanged_for_secs = modified
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .map(|age| age.as_secs()),
            reason = reason.as_str(),
            "took over a stale bundled skill install lock"
        ),
    }
}

fn lock_io_error(path: &VirtualPath, action: &str, error: FilesystemError) -> RebornBuildError {
    invalid_config(format!(
        "failed to {action} the bundled skill install lock {path}: {error}"
    ))
}

fn timed_out(path: &VirtualPath, policy: &InstallLockPolicy, holder: &str) -> RebornBuildError {
    invalid_config(format!(
        "timed out after {}s waiting for the bundled skill install lock {path}, held by {holder}",
        policy.wait_timeout.as_secs_f32()
    ))
}

/// The pause before the next look at a live holder: `poll` shortened by up to half at random, so
/// starters that began together spread out their reads, and never past `deadline`.
fn next_pause(poll: Duration, deadline: Instant) -> Duration {
    poll.mul_f64(rand::random_range(0.5..=1.0))
        .min(deadline.saturating_duration_since(Instant::now()))
}

/// The result of a write made on condition that the entry is still this holder's.
enum ConditionalWrite {
    Written(RecordVersion),
    /// The entry is gone or belongs to someone else; nothing was written.
    NotMine,
}

/// Proof that this process holds the bundled skill install lock. Run the install inside
/// [`hold_while`](Self::hold_while), which keeps the lease renewed, then
/// [`release`](Self::release) it. Dropping it without releasing leaves the entry for the next
/// starter to take over: at once from this process, after the lease from any other.
pub(super) struct BundledSkillInstallLock {
    path: VirtualPath,
    holder: InstallLockHolder,
    version: RecordVersion,
    arbiter: Arbiter,
    renew_interval: Duration,
    /// When the write that last established the lease began, by this process's monotonic clock.
    renewed_at: Instant,
    /// Held until the entry is released; dropping the file releases the OS lock.
    host_lock: Option<File>,
    /// Marks this lock's nonce as held by this process until the lock is dropped.
    _held_here: HeldHere,
}

impl BundledSkillInstallLock {
    /// Takes the lock for `system_skills_root`, replacing a stale one, and waits up to
    /// `policy.wait_timeout` for a live holder.
    ///
    /// `host_lock_file` is the OS-locked file to hold as well, for a skill root on a host disk.
    pub(super) async fn acquire(
        filesystem: &dyn RootFilesystem,
        system_skills_root: &VirtualPath,
        host_lock_file: Option<&Path>,
        policy: &InstallLockPolicy,
    ) -> Result<Self, RebornBuildError> {
        let path = child_path(system_skills_root, LOCK_ENTRY_FILE)?;
        let deadline = Instant::now() + policy.wait_timeout;
        let host_lock = match host_lock_file {
            Some(file) => acquire_host_lock(filesystem, &path, file, deadline, policy).await?,
            None => None,
        };
        let arbiter = if host_lock.is_some() {
            Arbiter::HostFileLock
        } else {
            Arbiter::EntryVersion
        };
        let local = HostIdentity::current();
        let holder = InstallLockHolder::new(local.clone(), policy.lease);
        let held_here = HeldHere::register(&holder.nonce);
        let entry = holder.entry()?;
        let mut watch = None;
        let mut poll = policy.first_poll;
        let mut last_seen = None;
        loop {
            let attempt_started = Instant::now();
            let current = filesystem
                .get(&path)
                .await
                .map_err(|error| lock_io_error(&path, "read", error))?;
            let written = match current {
                None => match filesystem
                    .put(&path, entry.clone(), CasExpectation::Absent)
                    .await
                {
                    Ok(version) => Some(version),
                    // Another starter created it first; the next look judges the new holder.
                    Err(FilesystemError::VersionMismatch { .. }) => None,
                    Err(error) => return Err(lock_io_error(&path, "create", error)),
                },
                Some(current) => {
                    let unchanged_for = Watch::observe(&mut watch, &current);
                    let observed = ObservedHolder::read(filesystem, &path, &current).await;
                    let reason = match judge(&observed, unchanged_for, &local, policy) {
                        Verdict::Stale(reason) => reason,
                        Verdict::Live => {
                            if Instant::now() >= deadline {
                                return Err(timed_out(&path, policy, &observed.describe()));
                            }
                            last_seen = Some(observed.describe());
                            sleep(next_pause(poll, deadline)).await;
                            poll = poll.saturating_mul(2).min(policy.max_poll);
                            continue;
                        }
                    };
                    let cas = match arbiter {
                        Arbiter::EntryVersion => CasExpectation::Version(current.version),
                        Arbiter::HostFileLock => CasExpectation::Any,
                    };
                    match filesystem.put(&path, entry.clone(), cas).await {
                        Ok(version) => {
                            log_take_over(&path, &observed, unchanged_for, reason);
                            Some(version)
                        }
                        // Another starter replaced or released the entry first.
                        Err(
                            FilesystemError::VersionMismatch { .. }
                            | FilesystemError::NotFound { .. },
                        ) => None,
                        Err(error @ FilesystemError::Unsupported { .. }) => {
                            return Err(invalid_config(format!(
                                "the bundled skill install lock {path} is stale ({}: {}), but \
                                 this filesystem cannot replace it safely ({error}); remove it \
                                 by hand once no install is running",
                                observed.describe(),
                                reason.as_str()
                            )));
                        }
                        Err(error) => return Err(lock_io_error(&path, "take over", error)),
                    }
                }
            };
            if let Some(version) = written {
                return Ok(Self {
                    path,
                    holder,
                    version,
                    arbiter,
                    renew_interval: policy.renew_interval,
                    renewed_at: attempt_started,
                    host_lock,
                    _held_here: held_here,
                });
            }
            if Instant::now() >= deadline {
                let holder = last_seen
                    .as_deref()
                    .unwrap_or("a succession of other starters");
                return Err(timed_out(&path, policy, holder));
            }
        }
    }

    /// Runs `work` while renewing the lease, and returns its result.
    ///
    /// If the lease is lost (another starter took the lock over, or no renewal succeeded for
    /// half the lease), `work` is dropped at its next await point and the error says why: once
    /// another starter may hold the lock, this one must stop writing.
    pub(super) async fn hold_while<T>(
        &mut self,
        filesystem: &dyn RootFilesystem,
        work: impl Future<Output = Result<T, RebornBuildError>>,
    ) -> Result<T, RebornBuildError> {
        let work = std::pin::pin!(work);
        tokio::select! {
            biased;
            result = work => result,
            lost = self.keep_renewed(filesystem) => Err(lost),
        }
    }

    /// Renews the lease every `renew_interval` until it is lost, and returns why it was lost.
    async fn keep_renewed(&mut self, filesystem: &dyn RootFilesystem) -> RebornBuildError {
        loop {
            sleep(self.renew_interval).await;
            let give_up_at = self.renewed_at + self.holder.lease() / 2;
            let attempt_started = Instant::now();
            match timeout_at(give_up_at, self.renew(filesystem)).await {
                Ok(Ok(ConditionalWrite::Written(version))) => {
                    self.version = version;
                    self.renewed_at = attempt_started;
                }
                Ok(Ok(ConditionalWrite::NotMine)) => {
                    return self.lost("another starter took it over");
                }
                Ok(Err(error)) if Instant::now() < give_up_at => {
                    tracing::debug!(
                        lock = %self.path,
                        %error,
                        "failed to renew the bundled skill install lock; retrying"
                    );
                }
                Ok(Err(error)) => {
                    return self.lost(&format!("renewing it kept failing: {error}"));
                }
                Err(_elapsed) => {
                    return self.lost(&format!(
                        "no renewal succeeded within {}s",
                        (self.holder.lease() / 2).as_secs_f32()
                    ));
                }
            }
        }
    }

    /// Rewrites the entry with the renewal counter bumped, if it is still this holder's.
    async fn renew(
        &mut self,
        filesystem: &dyn RootFilesystem,
    ) -> Result<ConditionalWrite, RebornBuildError> {
        self.holder.renewals = self.holder.renewals.saturating_add(1);
        let entry = self.holder.entry()?;
        if self.arbiter == Arbiter::EntryVersion {
            match filesystem
                .put(
                    &self.path,
                    entry.clone(),
                    CasExpectation::Version(self.version),
                )
                .await
            {
                Ok(version) => return Ok(ConditionalWrite::Written(version)),
                Err(FilesystemError::VersionMismatch { .. } | FilesystemError::NotFound { .. }) => {
                    return Ok(ConditionalWrite::NotMine);
                }
                // A backend without versioned writes: compare the nonce instead.
                Err(FilesystemError::Unsupported { .. }) => {}
                Err(error) => return Err(lock_io_error(&self.path, "renew", error)),
            }
        }
        if !self.entry_is_still_mine(filesystem).await? {
            return Ok(ConditionalWrite::NotMine);
        }
        filesystem
            .put(&self.path, entry, CasExpectation::Any)
            .await
            .map(ConditionalWrite::Written)
            .map_err(|error| lock_io_error(&self.path, "renew", error))
    }

    fn lost(&self, why: &str) -> RebornBuildError {
        invalid_config(format!(
            "lost the bundled skill install lock {} during the install ({why}); the install \
             stopped so that two installs never write at once, and the next start installs again",
            self.path
        ))
    }

    /// Deletes the lock entry if it is still this holder's, then drops the host file lock.
    ///
    /// An entry that another starter has taken over (this holder lost its lease) is left alone:
    /// it is the new holder's lock.
    pub(super) async fn release(
        mut self,
        filesystem: &dyn RootFilesystem,
    ) -> Result<(), RebornBuildError> {
        let result = match self.arbiter {
            Arbiter::EntryVersion => {
                match filesystem.delete_if_version(&self.path, self.version).await {
                    Ok(()) => Ok(()),
                    Err(FilesystemError::NotFound { .. }) => {
                        tracing::debug!(lock = %self.path, "bundled skill install lock was already gone at release");
                        Ok(())
                    }
                    Err(FilesystemError::VersionMismatch { .. }) => {
                        self.log_left_for_successor();
                        Ok(())
                    }
                    // A backend without versioned deletes: compare the nonce instead.
                    Err(FilesystemError::Unsupported { .. }) => {
                        self.delete_if_nonce_matches(filesystem).await
                    }
                    Err(error) => Err(lock_io_error(&self.path, "release", error)),
                }
            }
            Arbiter::HostFileLock => self.delete_if_nonce_matches(filesystem).await,
        };
        drop(self.host_lock.take());
        result
    }

    async fn entry_is_still_mine(
        &self,
        filesystem: &dyn RootFilesystem,
    ) -> Result<bool, RebornBuildError> {
        let current = filesystem
            .get(&self.path)
            .await
            .map_err(|error| lock_io_error(&self.path, "read", error))?;
        Ok(current
            .as_ref()
            .and_then(|entry| InstallLockHolder::decode(&entry.entry.body))
            .is_some_and(|holder| holder.nonce == self.holder.nonce))
    }

    async fn delete_if_nonce_matches(
        &self,
        filesystem: &dyn RootFilesystem,
    ) -> Result<(), RebornBuildError> {
        if self.entry_is_still_mine(filesystem).await? {
            delete_if_exists(filesystem, &self.path).await
        } else {
            self.log_left_for_successor();
            Ok(())
        }
    }

    fn log_left_for_successor(&self) {
        tracing::debug!(
            lock = %self.path,
            "bundled skill install lock is no longer this holder's; leaving it in place"
        );
    }
}

/// Takes the OS file lock on `host_lock_file`, waiting until `deadline` for another installer
/// on this host. Returns `None` when the filesystem cannot take OS locks at all, in which case
/// the caller falls back to the lock entry alone.
async fn acquire_host_lock(
    filesystem: &dyn RootFilesystem,
    entry_path: &VirtualPath,
    host_lock_file: &Path,
    deadline: Instant,
    policy: &InstallLockPolicy,
) -> Result<Option<File>, RebornBuildError> {
    let file = open_host_lock_file(host_lock_file)?;
    let mut poll = policy.first_poll;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    let holder = describe_entry_holder(filesystem, entry_path).await;
                    return Err(timed_out(entry_path, policy, &holder));
                }
                sleep(next_pause(poll, deadline)).await;
                poll = poll.saturating_mul(2).min(policy.max_poll);
            }
            Err(TryLockError::Error(error)) => {
                tracing::debug!(
                    %error,
                    host_lock = %host_lock_file.display(),
                    "OS file locks unavailable for the bundled skill install; relying on the lock \
                     entry alone"
                );
                return Ok(None);
            }
        }
    }
}

/// Opens (creating if needed) the OS-locked file without following a symlink at its path.
///
/// The open itself refuses the symlink, which leaves no gap between a check and the open for a
/// symlink to be swapped in: on Unix the open uses `O_NOFOLLOW`, which fails on a symlink; on
/// Windows it opens a reparse point itself rather than its target. Either way the opened handle
/// must then be a regular file.
fn open_host_lock_file(path: &Path) -> Result<File, RebornBuildError> {
    let refused = |reason: String| {
        invalid_config(format!(
            "bundled skill install host lock {} {reason}",
            path.display()
        ))
    };
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // `FILE_FLAG_OPEN_REPARSE_POINT`: open a symlink itself, not what it points to.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) => {
            return Err(refused(format!("must not be a symlink ({error})")));
        }
        Err(error) => return Err(refused(format!("could not be opened: {error}"))),
    };
    let metadata = file
        .metadata()
        .map_err(|error| refused(format!("could not be inspected: {error}")))?;
    if !metadata.is_file() {
        return Err(refused(
            "must be a regular file, not a symlink or anything else".to_string(),
        ));
    }
    Ok(file)
}

/// Names whoever holds the lock entry, for an error. Never fails: the error it feeds is already
/// the one worth reporting.
async fn describe_entry_holder(filesystem: &dyn RootFilesystem, path: &VirtualPath) -> String {
    match filesystem.get(path).await {
        Ok(Some(entry)) => ObservedHolder::read(filesystem, path, &entry)
            .await
            .describe(),
        Ok(None) => {
            "another process on this host that has not written the lock entry yet".to_string()
        }
        Err(error) => format!("another process on this host (lock entry unreadable: {error})"),
    }
}

#[cfg(test)]
mod tests;
