//! Cross-process lock around the startup install of bundled skills.
//!
//! # The lock
//!
//! The lock is an entry, [`LOCK_ENTRY_FILE`], directly under the skill root. It is created with
//! `CasExpectation::Absent`, so of several processes starting against one skill root only one
//! installs at a time. Its body is an [`InstallLockHolder`] record: the holder's pid and
//! [`HostIdentity`], the wall-clock time it took the lock, and a random nonce.
//!
//! # Recovering a lock whose holder died
//!
//! A process killed between acquire and release leaves the entry behind. A starter that finds an
//! entry judges whether its holder can still be working (see [`judge`]):
//!
//! - **The holder ran in this pid space and its pid is gone.** A pid means something only on the
//!   machine, boot and pid namespace that issued it, so the pid check runs only when the record's
//!   hostname, kernel boot id and pid namespace all equal this process's. The pid namespace is
//!   part of the identity because containers on one machine share the kernel's boot id (and often
//!   a hostname) while each has its own pid numbering. A holder from another host, or with any of
//!   the three unknown, gets only the age rule.
//! - **The holder is older than [`InstallLockPolicy::max_hold`]** (10 minutes), wherever it ran.
//!   The slowest install seen so far, 16 skills on a heavily loaded machine with a debug build,
//!   took 88 s; ten minutes is several times that, so a live install is never cut short in
//!   practice, while a holder that crashed on another host blocks starts for at most ten minutes.
//!   The age is measured against the holder's own wall clock, so clock skew between hosts shifts
//!   it; a timestamp in the future counts as age zero.
//! - **The record cannot be read** (for example one written by an older binary, whose lock body
//!   was a monotonic clock reading). Its age is taken from the entry's modification time and it is
//!   stale once older than [`InstallLockPolicy::unrecognised_grace`] (2 minutes, longer than such
//!   an install took even under heavy load). Every backend reports a modification time for a plain
//!   file entry; one that does not gives nothing to wait for, so the entry is taken over at once.
//!   Waiting instead would leave the root locked forever, and the risk is bounded: only a binary
//!   that predates the current record format writes such an entry, so the only live holder it can
//!   belong to is an older binary starting at the same moment.
//!
//! A live holder is waited on for [`InstallLockPolicy::wait_timeout`] (30 s), after which the
//! error names the holder and when it took the lock.
//!
//! # Replacing a stale lock without a race
//!
//! How a stale entry is replaced, and how a holder releases, depends on what the backend offers
//! ([`Arbiter`]):
//!
//! - **Versioned backends** (in-memory, libSQL, PostgreSQL). The take-over is a `put` with
//!   `CasExpectation::Version` of the stale entry, so when two starters judge the same entry stale
//!   only the first write lands; the other gets a version mismatch, reads the new holder, finds it
//!   alive, and waits. Release is `delete_if_version` on the version this process wrote, so a
//!   holder that stalled long enough to be taken over cannot delete its successor's lock.
//! - **Host disk** (the standalone storage root). The disk backend has no entry versions, so the
//!   installer also holds an exclusive OS file lock on [`HOST_LOCK_FILE`] in the storage root for
//!   as long as it holds the entry. The kernel drops that lock when its process exits, however it
//!   exits. While holding it no other installer on this host can touch the entry, so take-over is
//!   a plain overwrite and release deletes the entry only if it still carries this holder's nonce.
//!   It also means a readable entry found under the OS lock was left by a process that died:
//!   every writer of the current record format held the OS lock for as long as it held the entry.
//!   Such an entry is taken over at once; an unreadable one still waits out the grace period,
//!   since an older binary that never takes the OS lock may be installing. If the storage root's
//!   filesystem cannot take OS file locks, the disk path falls back to the entry alone: live
//!   installers still exclude each other, but a stale entry cannot be replaced safely, and the
//!   error says to remove it by hand.
//!
//! The shared bounded CAS helper (`ironclaw_filesystem::cas_update`) does not fit: it retries a
//! read-modify-write until the write lands, whereas a lock attempt must stop at the first conflict
//! and judge the new holder; and it works on a `ScopedFilesystem`, while this lock lives on the
//! `RootFilesystem` the install is given.
//!
//! One window remains on versioned backends. A version number restarts at 1 when an entry is
//! deleted and created again, so a starter that read a stale entry and then stalled between that
//! read and its take-over write — for longer than another starter's whole take-over, install and
//! release, and a third starter's fresh acquire — could find its expected version current again
//! and replace a live lock.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use ironclaw_filesystem::{
    CasExpectation, Entry, FilesystemError, RecordVersion, RootFilesystem, VersionedEntry,
};
use ironclaw_host_api::path::VirtualPath;
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep};

use super::{child_path, delete_if_exists, invalid_config, reject_existing_symlink};
use crate::RebornBuildError;

/// The lock entry, directly under the skill root.
pub(super) const LOCK_ENTRY_FILE: &str = ".ironclaw-reborn-bundled.lock";
/// The OS-locked file in a standalone storage root. It is never deleted: removing a file that
/// another process may have open and be waiting to lock would let two processes lock different
/// files under one name.
pub(super) const HOST_LOCK_FILE: &str = ".ironclaw-reborn-bundled-skills.host-lock";

/// The current [`InstallLockHolder`] format; a record carrying any other value is unrecognised.
const HOLDER_FORMAT: u8 = 1;
/// Longest host-identity field shown in an error or log line. The fields come from whichever
/// process wrote the lock, possibly on another host, so they are bounded and escaped when shown.
const SHOWN_FIELD_MAX_CHARS: usize = 64;

/// How long to wait for a live holder and when a holder counts as stale. The defaults are the
/// production values; see the module docs for why each is what it is.
#[derive(Debug, Clone)]
pub(super) struct InstallLockPolicy {
    /// How long a starter waits for a live holder before failing.
    pub(super) wait_timeout: Duration,
    /// Pause between looks at a live holder's lock.
    pub(super) retry_interval: Duration,
    /// Age after which a holder is stale wherever it ran.
    pub(super) max_hold: Duration,
    /// Age after which an entry whose record cannot be read is stale.
    pub(super) unrecognised_grace: Duration,
}

impl Default for InstallLockPolicy {
    fn default() -> Self {
        Self {
            wait_timeout: Duration::from_secs(30),
            retry_interval: Duration::from_millis(25),
            max_hold: Duration::from_secs(10 * 60),
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

/// The body of the lock entry: who holds the lock and since when.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallLockHolder {
    format: u8,
    pid: u32,
    host: HostIdentity,
    acquired_at: DateTime<Utc>,
    /// Random per acquisition; tells this holder's entry apart from a successor's where the
    /// backend has no entry versions.
    nonce: String,
}

impl InstallLockHolder {
    fn new(host: HostIdentity) -> Self {
        Self {
            format: HOLDER_FORMAT,
            pid: std::process::id(),
            host,
            acquired_at: Utc::now(),
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

    fn age(&self, now: DateTime<Utc>) -> Duration {
        (now - self.acquired_at).to_std().unwrap_or(Duration::ZERO)
    }

    fn describe(&self) -> String {
        format!(
            "pid {} on host {} since {}",
            self.pid,
            shown(self.host.hostname.as_deref()),
            self.acquired_at.to_rfc3339_opts(SecondsFormat::Secs, true)
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

/// What decides that a stale entry may be replaced, and how a holder releases.
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
    /// The holder took the lock longer ago than the maximum hold time.
    Expired,
    /// A readable entry found while holding the host file lock, which its writer would still
    /// hold if it were alive.
    Orphaned,
    /// An unreadable entry older than the grace period.
    UnrecognisedExpired,
    /// An unreadable entry on a backend that reports no modification time.
    UnrecognisedUndated,
}

impl StaleReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::HolderExited => "holder process exited",
            Self::Expired => "held longer than the maximum hold time",
            Self::Orphaned => "holder no longer holds the host lock",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidState {
    Running,
    Gone,
    /// The platform cannot tell, or the number is not a valid pid here.
    Unknown,
}

#[cfg(unix)]
fn pid_state(pid: u32) -> PidState {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // `kill` treats 0 and negative numbers as process groups, so only a positive pid is probed.
    let Some(raw) = i32::try_from(pid).ok().filter(|raw| *raw > 0) else {
        return PidState::Unknown;
    };
    // Signal 0 checks for existence without delivering anything. EPERM means the process exists
    // but belongs to another user.
    match kill(Pid::from_raw(raw), None) {
        Ok(()) | Err(Errno::EPERM) => PidState::Running,
        Err(Errno::ESRCH) => PidState::Gone,
        Err(_) => PidState::Unknown,
    }
}

#[cfg(not(unix))]
fn pid_state(_pid: u32) -> PidState {
    PidState::Unknown
}

/// Decides whether the holder of an existing lock entry may still be working.
fn judge(
    observed: &ObservedHolder,
    arbiter: Arbiter,
    local: &HostIdentity,
    policy: &InstallLockPolicy,
) -> Verdict {
    match observed {
        ObservedHolder::Holder(_) if arbiter == Arbiter::HostFileLock => {
            Verdict::Stale(StaleReason::Orphaned)
        }
        ObservedHolder::Holder(holder) => {
            if holder.age(Utc::now()) >= policy.max_hold {
                Verdict::Stale(StaleReason::Expired)
            } else if local.shares_pid_space_with(&holder.host)
                && pid_state(holder.pid) == PidState::Gone
            {
                Verdict::Stale(StaleReason::HolderExited)
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

fn log_take_over(path: &VirtualPath, observed: &ObservedHolder, reason: StaleReason) {
    match observed {
        ObservedHolder::Holder(holder) => tracing::debug!(
            lock = %path,
            previous_pid = holder.pid,
            previous_host = %shown(holder.host.hostname.as_deref()),
            held_for_secs = holder.age(Utc::now()).as_secs(),
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

/// Proof that this process holds the bundled skill install lock. Release it with
/// [`release`](Self::release); dropping it releases only the host file lock, if any.
pub(super) struct BundledSkillInstallLock {
    path: VirtualPath,
    holder: InstallLockHolder,
    version: RecordVersion,
    arbiter: Arbiter,
    /// Held until the entry is released; dropping the file releases the OS lock.
    host_lock: Option<File>,
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
        let mut last_seen = None;
        loop {
            let holder = InstallLockHolder::new(local.clone());
            match filesystem
                .put(&path, holder.entry()?, CasExpectation::Absent)
                .await
            {
                Ok(version) => {
                    return Ok(Self {
                        path,
                        holder,
                        version,
                        arbiter,
                        host_lock,
                    });
                }
                Err(FilesystemError::VersionMismatch { .. }) => {}
                Err(error) => return Err(lock_io_error(&path, "create", error)),
            }

            // The lock is taken; see by whom. An entry that vanished since the create attempt
            // was released, so the next attempt can take it.
            if let Some(current) = filesystem
                .get(&path)
                .await
                .map_err(|error| lock_io_error(&path, "read", error))?
            {
                let observed = ObservedHolder::read(filesystem, &path, &current).await;
                match judge(&observed, arbiter, &local, policy) {
                    Verdict::Stale(reason) => {
                        let cas = match arbiter {
                            Arbiter::EntryVersion => CasExpectation::Version(current.version),
                            Arbiter::HostFileLock => CasExpectation::Any,
                        };
                        match filesystem.put(&path, holder.entry()?, cas).await {
                            Ok(version) => {
                                log_take_over(&path, &observed, reason);
                                return Ok(Self {
                                    path,
                                    holder,
                                    version,
                                    arbiter,
                                    host_lock,
                                });
                            }
                            // Another starter replaced or released the entry first; look again.
                            Err(
                                FilesystemError::VersionMismatch { .. }
                                | FilesystemError::NotFound { .. },
                            ) => {}
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
                    Verdict::Live => {
                        if Instant::now() >= deadline {
                            return Err(timed_out(&path, policy, &observed.describe()));
                        }
                        last_seen = Some(observed.describe());
                        sleep(policy.retry_interval).await;
                        continue;
                    }
                }
            }
            if Instant::now() >= deadline {
                let holder = last_seen
                    .as_deref()
                    .unwrap_or("a succession of other starters");
                return Err(timed_out(&path, policy, holder));
            }
        }
    }

    /// Deletes the lock entry if it is still this holder's, then drops the host file lock.
    ///
    /// An entry that another starter has taken over (this holder outlived the maximum hold time)
    /// is left alone: it is the new holder's lock.
    pub(super) async fn release(
        self,
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
        drop(self.host_lock);
        result
    }

    async fn delete_if_nonce_matches(
        &self,
        filesystem: &dyn RootFilesystem,
    ) -> Result<(), RebornBuildError> {
        let current = filesystem
            .get(&self.path)
            .await
            .map_err(|error| lock_io_error(&self.path, "read", error))?;
        let still_mine = current
            .as_ref()
            .and_then(|entry| InstallLockHolder::decode(&entry.entry.body))
            .is_some_and(|holder| holder.nonce == self.holder.nonce);
        if still_mine {
            delete_if_exists(filesystem, &self.path).await
        } else {
            if current.is_some() {
                self.log_left_for_successor();
            }
            Ok(())
        }
    }

    fn log_left_for_successor(&self) {
        tracing::debug!(
            lock = %self.path,
            "bundled skill install lock was taken over by another starter; leaving it in place"
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
    reject_existing_symlink(host_lock_file, "bundled skill install host lock")?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(host_lock_file)
        .map_err(|error| {
            invalid_config(format!(
                "failed to open the bundled skill install host lock {}: {error}",
                host_lock_file.display()
            ))
        })?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    let holder = describe_entry_holder(filesystem, entry_path).await;
                    return Err(timed_out(entry_path, policy, &holder));
                }
                sleep(policy.retry_interval).await;
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
