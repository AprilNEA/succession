//! Asking a superseded tenant to leave, without signalling the wrong process.

use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, Signal, System};

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

    let pid = Pid::from_u32(record.tenant.pid);
    let mut system = System::new();
    // An undelivered signal means the process vanished in between, or that it
    // cannot be signalled at all; the wait below tells those apart.
    let _delivered = signal(&mut system, pid, Signal::Term);

    let started = Instant::now();
    let mut escalated = policy.escalate_after.is_none();
    loop {
        if Tenant::look_up(record.tenant.pid).is_none_or(|now| {
            // A pid reused within the deadline is not our tenant any more.
            record.tenant.compare(&now) == Sameness::Different
        }) {
            return Outcome::Left;
        }
        let waited = started.elapsed();
        if waited >= policy.deadline {
            return Outcome::Stubborn;
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
