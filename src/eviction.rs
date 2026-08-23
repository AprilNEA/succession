//! Asking a superseded tenant to leave, without signalling the wrong process.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessesToUpdate, Signal, System};

use crate::record::{Record, Sameness, Tenant};

/// How hard to press a tenant that will not leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// What to do when the record carries nothing but a pid, so the process
    /// cannot be confirmed. Refusing is the safe default: a pid can be reused.
    pub unconfirmed: Unconfirmed,
    /// How long to wait for a polite exit before escalating to a kill.
    /// `None` never escalates.
    pub escalate_after: Option<Duration>,
    /// How long to keep waiting in total.
    pub deadline: Duration,
    /// How often to look again while waiting.
    pub poll: Duration,
}

impl Default for Policy {
    /// Refuse to act on an unconfirmed pid, kill after two seconds, give up
    /// after five.
    fn default() -> Self {
        Self {
            unconfirmed: Unconfirmed::Refuse,
            escalate_after: Some(Duration::from_secs(2)),
            deadline: Duration::from_secs(5),
            poll: Duration::from_millis(100),
        }
    }
}

/// What to do about a tenant whose record proves nothing beyond its pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unconfirmed {
    /// Leave it alone. Correct whenever another process could plausibly have
    /// inherited the pid.
    Refuse,
    /// Signal it anyway — for a tenant population that predates claim records,
    /// where refusing means the role stays wedged forever.
    Signal,
}

/// How an eviction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The pid was already gone before anything was signalled.
    AlreadyGone,
    /// It left after being signalled.
    Left,
    /// Nothing was signalled: the live process is not the one in the record.
    Refused(Sameness),
    /// It was signalled and was still running when the deadline elapsed.
    Stubborn,
}

/// Ask the tenant `record` describes to leave.
///
/// The pid is verified against the record first ([`Tenant::compare`]), because
/// a claim record can outlive its writer and the pid may since belong to
/// something unrelated. Only [`Sameness::Same`] — or
/// [`Unconfirmed::Signal`] against [`Sameness::Inconclusive`] — is ever
/// signalled.
///
/// Releasing the role is the point, so this waits for the process to actually
/// disappear rather than assuming the signal landed.
#[must_use]
pub fn evict(record: &Record, policy: &Policy) -> Outcome {
    let Some(live) = Tenant::look_up(record.tenant.pid) else {
        return Outcome::AlreadyGone;
    };
    match record.tenant.compare(&live) {
        Sameness::Same => {}
        Sameness::Inconclusive if policy.unconfirmed == Unconfirmed::Signal => {}
        refused => return Outcome::Refused(refused),
    }

    match press(&record.tenant, policy) {
        Pressed::Left => Outcome::Left,
        Pressed::Stubborn => Outcome::Stubborn,
    }
}

/// How an eviction with nothing to verify against ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnonymousOutcome {
    /// Nothing running matches `expected`, so the holder is not the caller's
    /// helper. Nothing was signalled.
    NoCandidate,
    /// Several processes match `expected` and only one of them can be holding
    /// the role. Nothing was signalled: which one is unknowable from here, and
    /// guessing means killing a bystander.
    Ambiguous {
        /// How many matched.
        running: usize,
    },
    /// The single candidate left after being signalled.
    Left,
    /// It was signalled and was still running when the deadline elapsed.
    Stubborn,
}

/// Ask an unidentified tenant to leave, recognizing it by the image it runs.
///
/// The answer to [`Verdict::EvictAnonymous`](crate::Verdict::EvictAnonymous),
/// which reports a held role that published nothing readable — an install that
/// predates claim records, or a tenant whose [`publish`](crate::Tenancy::publish)
/// failed. There is no record to verify a pid against, so
/// [`evict`] cannot help and the role would otherwise stay wedged for as long
/// as that process lives.
///
/// What makes signalling defensible here is `expected`: the absolute path of
/// the executable the caller starts for this role. Every process behind that
/// path is the caller's own helper, so evicting one is within its authority in
/// a way that acting on a bare pid never is. Matching is exact — a bare file
/// name would reach strangers — and literal, against the path the OS reports,
/// so pass the resolved one rather than a route to it.
///
/// Refuses whenever the choice is not forced: no match means the holder is
/// something else entirely, and more than one match means the holder cannot be
/// told from its siblings. Both leave the role alone.
///
/// The role is not probed here. Ask again through
/// [`Role::occupancy`](crate::Role::occupancy) afterwards — a supervisor's next
/// tick does exactly that — because a candidate leaving proves the process is
/// gone, not that it was the one holding the lock.
#[must_use]
pub fn evict_anonymous(expected: &Path, policy: &Policy) -> AnonymousOutcome {
    match sole(running_from(expected)) {
        Err(refusal) => refusal,
        Ok(candidate) => match press(&candidate, policy) {
            Pressed::Left => AnonymousOutcome::Left,
            Pressed::Stubborn => AnonymousOutcome::Stubborn,
        },
    }
}

/// The one candidate worth pressing, or the refusal to report instead.
///
/// Separate from the process walk so the guard that keeps a bystander alive is
/// testable without arranging a process table.
fn sole(mut candidates: Vec<Tenant>) -> Result<Tenant, AnonymousOutcome> {
    match candidates.len() {
        0 => Err(AnonymousOutcome::NoCandidate),
        1 => Ok(candidates.remove(0)),
        running => Err(AnonymousOutcome::Ambiguous { running }),
    }
}

/// Live processes whose executable is exactly `expected`, never including this
/// one — a single-binary application can run its own helper mode, and
/// signalling ourselves is never the intent.
fn running_from(expected: &Path) -> Vec<Tenant> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mine = Pid::from_u32(std::process::id());
    system
        .processes()
        .iter()
        .filter(|(pid, process)| **pid != mine && process.exe() == Some(expected))
        .map(|(pid, process)| Tenant {
            pid: pid.as_u32(),
            started_at: Some(process.start_time()),
            image: process.exe().map(Path::to_path_buf),
        })
        .collect()
}

/// Whether a pressed tenant gave up the role.
enum Pressed {
    Left,
    Stubborn,
}

/// Signal `tenant`, escalate if it lingers, and wait for it to actually go.
///
/// Releasing the role is the point, so this watches the process rather than
/// assuming the signal landed. A pid reused inside the deadline reads as gone:
/// whatever holds it now, it is not the tenant.
fn press(tenant: &Tenant, policy: &Policy) -> Pressed {
    let pid = Pid::from_u32(tenant.pid);
    let mut system = System::new();
    // An undelivered signal means the process vanished in between, or that it
    // cannot be signalled at all; the wait below tells those apart.
    let _delivered = signal(&mut system, pid, Signal::Term);

    let started = Instant::now();
    let mut escalated = policy.escalate_after.is_none();
    loop {
        if Tenant::look_up(tenant.pid).is_none_or(|now| tenant.compare(&now) == Sameness::Different)
        {
            return Pressed::Left;
        }
        let waited = started.elapsed();
        if waited >= policy.deadline {
            return Pressed::Stubborn;
        }
        if !escalated && policy.escalate_after.is_some_and(|after| waited >= after) {
            escalated = true;
            signal(&mut system, pid, Signal::Kill);
        }
        thread::sleep(policy.poll);
    }
}

/// Send `signal`, reporting whether it was delivered.
fn signal(system: &mut System, pid: Pid, signal: Signal) -> bool {
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .and_then(|process| process.kill_with(signal))
        .unwrap_or(false)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "panic helpers are idiomatic in tests")]
mod tests {
    use super::*;
    use crate::identity::{Compat, Identity, Run};

    fn tenant(pid: u32) -> Tenant {
        Tenant {
            pid,
            started_at: Some(1),
            image: None,
        }
    }

    fn record(tenant: Tenant) -> Record {
        Record::new(Identity::new(Run::from_raw(1), Compat::from_raw(1)), tenant)
    }

    /// A pid that is genuinely not running: start a process, wait for it, and
    /// close its handle.
    ///
    /// Guessing at an impossible pid does not work — pid 0 is the System Idle
    /// Process on Windows and is visible to a process lookup, so a record
    /// naming it takes the comparison path instead of the "already gone" one.
    fn a_reaped_pid() -> u32 {
        // `--list` makes the test binary print its test names and exit, which
        // needs no platform-specific command.
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("the test binary's own path"),
        )
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning the test binary");
        let pid = child.id();
        child.wait().expect("waiting for the child");
        // Windows keeps a terminated process visible while a handle to it is
        // open, and `Child` holds one until it is dropped.
        drop(child);
        pid
    }

    #[test]
    fn a_pid_that_is_gone_needs_no_signal() {
        let outcome = evict(
            &record(Tenant {
                pid: a_reaped_pid(),
                started_at: Some(1),
                image: None,
            }),
            &Policy::default(),
        );
        assert_eq!(outcome, Outcome::AlreadyGone);
    }

    #[test]
    fn a_pid_that_no_longer_matches_its_record_is_never_signalled() {
        // This process, described wrongly: the start time refutes the record,
        // so eviction must refuse rather than kill the test runner.
        let outcome = evict(
            &record(Tenant {
                pid: std::process::id(),
                started_at: Some(1),
                image: None,
            }),
            &Policy::default(),
        );
        assert_eq!(outcome, Outcome::Refused(Sameness::Different));
    }

    #[test]
    fn the_evictor_never_counts_itself_as_a_candidate() {
        // Without the filter this walk would hand `evict_anonymous` the test
        // runner, and pressing it would end the suite. A single-binary
        // application whose helper mode is the same image makes that a real
        // shape, not a hypothetical one.
        let mine = std::env::current_exe().expect("this test binary's path");
        let candidates = running_from(&mine);
        assert!(
            !candidates
                .iter()
                .any(|tenant| tenant.pid == std::process::id()),
            "the current process must never be a candidate"
        );
    }

    #[test]
    fn nothing_running_from_the_expected_image_is_left_alone() {
        let outcome = evict_anonymous(
            Path::new("/nonexistent/succession-helper"),
            &Policy::default(),
        );
        assert_eq!(outcome, AnonymousOutcome::NoCandidate);
    }

    #[test]
    fn siblings_that_cannot_be_told_apart_are_all_spared() {
        // Only one of them holds the role and nothing here says which, so
        // signalling either one is a coin flip with a bystander's life.
        let candidates = vec![tenant(11), tenant(12), tenant(13)];
        assert_eq!(
            sole(candidates),
            Err(AnonymousOutcome::Ambiguous { running: 3 })
        );
    }

    #[test]
    fn a_forced_choice_is_taken() {
        assert_eq!(sole(vec![tenant(11)]).map(|found| found.pid), Ok(11));
        assert_eq!(sole(Vec::new()), Err(AnonymousOutcome::NoCandidate));
    }

    #[test]
    fn an_unconfirmable_pid_is_refused_by_default() {
        let outcome = evict(
            &record(Tenant {
                pid: std::process::id(),
                started_at: None,
                image: None,
            }),
            &Policy::default(),
        );
        assert_eq!(outcome, Outcome::Refused(Sameness::Inconclusive));
    }
}
