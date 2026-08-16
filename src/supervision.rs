//! A loop over the verdict: probe the role, start the helper, wait, back off.

use std::fmt;
use std::io;
use std::process::{Child, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use crate::identity::Run;
use crate::record::Record;
use crate::role::{Occupancy, Role};
use crate::verdict::{Verdict, verdict};

/// How long to wait before starting a helper again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restart {
    /// Delay after a helper that had been running exits.
    pub base: Duration,
    /// Ceiling for a helper that keeps failing at startup.
    pub ceiling: Duration,
    /// How long a run must last to count as "it worked, then it stopped"
    /// rather than "it cannot start", which is what resets the backoff.
    pub healthy_after: Duration,
}

impl Default for Restart {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(2),
            ceiling: Duration::from_secs(60),
            healthy_after: Duration::from_secs(30),
        }
    }
}

impl Restart {
    /// The delay before the next attempt, given the previous delay and how long
    /// the run that just ended lasted.
    #[must_use]
    pub fn next_delay(&self, previous: Duration, ran_for: Duration) -> Duration {
        if ran_for >= self.healthy_after {
            self.base
        } else {
            (previous * 2).min(self.ceiling)
        }
    }
}

/// What a [`Supervisor`] just did, for the caller to log.
///
/// Reporting rather than logging keeps this crate free of a logging
/// dependency, and keeps the choice of level and format with the application.
#[derive(Debug)]
pub enum Event<'a> {
    /// The role is filled by someone who should be left alone for now.
    Occupied(&'a Occupancy),
    /// A tenant from a finished run is in the way.
    Superseded(&'a Record),
    /// Someone is in the way who never identified themselves.
    SupersededAnonymously,
    /// A helper was started.
    Started(u32),
    /// The helper exited after `ran_for`.
    Exited {
        /// How it exited.
        status: ExitStatus,
        /// How long it ran.
        ran_for: Duration,
    },
    /// The helper could not be started at all.
    SpawnFailed(&'a io::Error),
    /// Sleeping before the next attempt.
    BackingOff(Duration),
}

impl fmt::Display for Event<'_> {
    /// A ready-made log line, so reporting costs one closure and no logging
    /// dependency: `|event| tracing::info!("{event}")`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occupied(occupancy) => write!(f, "role {occupancy}"),
            Self::Superseded(record) => write!(f, "role held by a finished run: {record}"),
            Self::SupersededAnonymously => {
                f.write_str("role held by a tenant that never identified itself")
            }
            Self::Started(pid) => write!(f, "helper started, pid {pid}"),
            Self::Exited { status, ran_for } => {
                write!(f, "helper exited {status} after {ran_for:.1?}")
            }
            Self::SpawnFailed(error) => write!(f, "helper could not be started: {error}"),
            Self::BackingOff(delay) => write!(f, "waiting {delay:.1?} before the next attempt"),
        }
    }
}

/// Drives one role: waits while it is filled, starts a helper when it is free,
/// and backs off when that helper will not stay up.
///
/// Eviction is deliberately not automatic — the [`Event::Superseded`] report
/// hands the caller the record, together with everything needed to verify the
/// pid before signalling it (see the `eviction` feature).
#[derive(Debug)]
pub struct Supervisor {
    role: Role,
    mine: Run,
    grace: Duration,
    poll: Duration,
    restart: Restart,
    delay: Duration,
    waiting_since: Option<Instant>,
}

impl Supervisor {
    /// Supervise `role` on behalf of run `mine`.
    #[must_use]
    pub fn new(role: Role, mine: Run) -> Self {
        let restart = Restart::default();
        Self {
            role,
            mine,
            grace: Duration::from_secs(15),
            poll: Duration::from_millis(500),
            restart,
            delay: restart.base,
            waiting_since: None,
        }
    }

    /// How long an unidentified tenant is left alone before it counts as one
    /// that will never leave on its own.
    #[must_use]
    pub const fn grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// How often to look again while the role is filled.
    #[must_use]
    pub const fn poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// The restart policy for a helper that exits.
    #[must_use]
    pub const fn restart(mut self, restart: Restart) -> Self {
        self.restart = restart;
        self.delay = restart.base;
        self
    }

    /// One cycle: probe the role, act, and report what happened.
    ///
    /// A cycle that starts a helper lasts as long as that helper does, so a
    /// caller's loop is just `loop { supervisor.tick(&mut spawn, &mut report) }`
    /// on a thread of its own.
    ///
    /// # Errors
    ///
    /// The [`io::Error`] from probing the role. Treat it as "free" and try
    /// again: assuming the role is filled would wait forever.
    pub fn tick(
        &mut self,
        spawn: &mut impl FnMut() -> io::Result<Child>,
        report: &mut impl FnMut(Event<'_>),
    ) -> io::Result<()> {
        let occupancy = self.role.occupancy()?;
        let waited = self.waiting_since.map_or(Duration::ZERO, |at| at.elapsed());
        match verdict(&occupancy, self.mine, waited, self.grace) {
            Verdict::Start => {
                self.waiting_since = None;
                self.start(spawn, report);
            }
            Verdict::Wait => {
                self.waiting_since.get_or_insert_with(Instant::now);
                report(Event::Occupied(&occupancy));
                thread::sleep(self.poll);
            }
            Verdict::Evict(record) => {
                report(Event::Superseded(&record));
                thread::sleep(self.poll);
            }
            Verdict::EvictAnonymous => {
                report(Event::SupersededAnonymously);
                thread::sleep(self.poll);
            }
        }
        Ok(())
    }

    fn start(
        &mut self,
        spawn: &mut impl FnMut() -> io::Result<Child>,
        report: &mut impl FnMut(Event<'_>),
    ) {
        let started = Instant::now();
        match spawn() {
            Ok(mut child) => {
                report(Event::Started(child.id()));
                match child.wait() {
                    Ok(status) => report(Event::Exited {
                        status,
                        ran_for: started.elapsed(),
                    }),
                    Err(error) => report(Event::SpawnFailed(&error)),
                }
            }
            Err(error) => report(Event::SpawnFailed(&error)),
        }
        self.delay = self.restart.next_delay(self.delay, started.elapsed());
        report(Event::BackingOff(self.delay));
        thread::sleep(self.delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: Restart = Restart {
        base: Duration::from_secs(2),
        ceiling: Duration::from_secs(60),
        healthy_after: Duration::from_secs(30),
    };
    const INSTANT: Duration = Duration::from_millis(20);

    #[test]
    fn a_helper_that_dies_at_startup_is_retried_ever_more_slowly() {
        let mut delay = POLICY.next_delay(POLICY.base, INSTANT);
        assert_eq!(delay, POLICY.base * 2);
        for _ in 0..10 {
            delay = POLICY.next_delay(delay, INSTANT);
        }
        assert_eq!(delay, POLICY.ceiling);
    }

    #[test]
    fn a_run_that_lasted_resets_the_backoff() {
        assert_eq!(
            POLICY.next_delay(POLICY.ceiling, POLICY.healthy_after),
            POLICY.base
        );
    }
}
