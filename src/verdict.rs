//! The supervisor's side: may I start my helper, or is someone in the seat?

use std::time::Duration;

use crate::identity::Run;
use crate::record::Record;
use crate::role::Occupancy;

/// What a supervisor should do about the state of a role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The role is free: start the helper.
    Start,
    /// Someone is in the seat who should be left alone for now. Look again
    /// shortly.
    Wait,
    /// The tenant belongs to a run that is over. Ask it to leave, then look
    /// again — do not assume the role is yours until the lock is actually
    /// free.
    ///
    /// Verify before signalling: compare the record's [`crate::Tenant`]
    /// against the live process facts for that pid, because a pid can be
    /// reused.
    Evict(Record),
    /// Someone holds the role, published nothing readable, and has had long
    /// enough to leave politely. Nothing can be verified about it, so an
    /// evictor has to fall back on whatever it knows out of band — the
    /// expected image name, say — and should prefer a polite signal.
    EvictAnonymous,
}

/// Decide what to do about `occupancy`, given the run the supervisor belongs
/// to and how long it has already been waiting.
///
/// The whole decision, with no I/O in it:
///
/// | occupancy | verdict |
/// |---|---|
/// | free | [`Verdict::Start`] |
/// | held by `mine` | [`Verdict::Wait`] |
/// | held by another run | [`Verdict::Evict`] |
/// | held anonymously, within `grace` | [`Verdict::Wait`] |
/// | held anonymously, past `grace` | [`Verdict::EvictAnonymous`] |
///
/// A tenant of the supervisor's own run is never evicted, however long it
/// takes: the supervisor spawned it, so it holds a handle to it and can deal
/// with a wedged child directly. What this decision is for is the tenant
/// nobody owns any more.
///
/// `grace` exists only for tenants that publish no record — a population that
/// disappears as an application's installs turn over. Once every tenant
/// publishes, an obsolete one is recognized on sight and the wait is over.
#[must_use]
pub fn verdict(occupancy: &Occupancy, mine: Run, waited: Duration, grace: Duration) -> Verdict {
    match occupancy {
        Occupancy::Free => Verdict::Start,
        Occupancy::HeldBy(record) if record.identity.run == mine => Verdict::Wait,
        Occupancy::HeldBy(record) => Verdict::Evict(record.clone()),
        Occupancy::HeldAnonymously if waited < grace => Verdict::Wait,
        Occupancy::HeldAnonymously => Verdict::EvictAnonymous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Compat, Identity};
    use crate::record::Tenant;

    const GRACE: Duration = Duration::from_secs(15);
    const ALMOST_GRACE: Duration = Duration::from_millis(14_999);
    const MINE: Run = Run::from_raw(7);

    fn held_by(run: u64) -> Occupancy {
        Occupancy::HeldBy(Record::new(
            Identity::new(Run::from_raw(run), Compat::from_raw(18)),
            Tenant {
                pid: 4242,
                started_at: None,
                image: None,
            },
        ))
    }

    #[test]
    fn a_free_role_is_taken_at_once() {
        assert_eq!(
            verdict(&Occupancy::Free, MINE, Duration::ZERO, GRACE),
            Verdict::Start
        );
    }

    #[test]
    fn the_supervisors_own_tenant_is_never_evicted() {
        // However long it sits there: the supervisor owns that child handle
        // and must not race itself for the role.
        assert_eq!(
            verdict(&held_by(7), MINE, Duration::from_secs(3600), GRACE),
            Verdict::Wait
        );
    }

    #[test]
    fn a_tenant_from_a_finished_run_is_evicted_without_waiting() {
        // No deadline guesswork: the record says whose it is.
        let Verdict::Evict(record) = verdict(&held_by(9), MINE, Duration::ZERO, GRACE) else {
            panic!("a foreign tenant must be evicted");
        };
        assert_eq!(record.tenant.pid, 4242);
    }

    #[test]
    fn an_anonymous_tenant_is_given_the_grace_period_first() {
        assert_eq!(
            verdict(&Occupancy::HeldAnonymously, MINE, Duration::ZERO, GRACE),
            Verdict::Wait
        );
        assert_eq!(
            verdict(&Occupancy::HeldAnonymously, MINE, ALMOST_GRACE, GRACE),
            Verdict::Wait
        );
        assert_eq!(
            verdict(&Occupancy::HeldAnonymously, MINE, GRACE, GRACE),
            Verdict::EvictAnonymous
        );
    }
}
