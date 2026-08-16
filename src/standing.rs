//! The subordinate's side: am I still the helper this owner wants?

use std::fmt;
use std::sync::OnceLock;

use crate::identity::{Compat, Identity, Run};

/// The owner run a helper serves.
///
/// A helper holds its role for as long as it lives, so it has to notice when
/// the run it belongs to is over — otherwise it keeps a role its replacement
/// needs. Show it the owner's [`Identity`] on every handshake and it will say
/// whether it is still the right helper.
///
/// The owner run is remembered the first time it is seen, unless
/// [`Allegiance::to`] was told up front — which is worth doing whenever the
/// owner can pass its run down at spawn time, because then even the very first
/// handshake catches a helper that outlived its owner.
#[derive(Debug)]
pub struct Allegiance {
    ours: Compat,
    owner: OnceLock<Run>,
}

impl Allegiance {
    /// Serve whichever owner run answers first.
    #[must_use]
    pub const fn new(ours: Compat) -> Self {
        Self {
            ours,
            owner: OnceLock::new(),
        }
    }

    /// Serve one known owner run — the one that spawned this process.
    #[must_use]
    pub fn to(ours: Compat, owner: Run) -> Self {
        let allegiance = Self::new(ours);
        let _ = allegiance.owner.set(owner);
        allegiance
    }

    /// Judge the owner that just answered.
    ///
    /// [`Standing::Superseded`] is final: this process cannot become the right
    /// helper again, so the useful response is to release the role and exit —
    /// which this crate deliberately leaves to the caller, because a library
    /// has no business ending a process.
    #[must_use]
    pub fn observe(&self, owner: Identity) -> Standing {
        if owner.compat != self.ours {
            return Standing::Superseded(Because::Incompatible {
                ours: self.ours,
                owner: owner.compat,
            });
        }
        match self.owner.get() {
            Some(&sworn) if sworn != owner.run => Standing::Superseded(Because::NewRun {
                sworn,
                owner: owner.run,
            }),
            Some(_) => Standing::Current,
            None => {
                let _ = self.owner.set(owner.run);
                Standing::Current
            }
        }
    }

    /// The owner run this helper serves, once one is known.
    #[must_use]
    pub fn owner(&self) -> Option<Run> {
        self.owner.get().copied()
    }
}

/// Whether a helper is still the one its owner wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Still serving the right owner.
    Current,
    /// Superseded: give the role up.
    Superseded(Because),
}

/// What made a helper obsolete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Because {
    /// The owner cannot be talked to at all — this helper is from an install
    /// that has since been replaced.
    Incompatible {
        /// What this helper speaks.
        ours: Compat,
        /// What the owner speaks.
        owner: Compat,
    },
    /// The owner is a different run than the one this helper serves. Same
    /// build, most likely — a restart, which a version number cannot see.
    NewRun {
        /// The run this helper was serving.
        sworn: Run,
        /// The run now answering.
        owner: Run,
    },
}

impl fmt::Display for Because {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incompatible { ours, owner } => write!(
                f,
                "owner speaks {} and this helper speaks {}",
                owner.get(),
                ours.get()
            ),
            Self::NewRun { sworn, owner } => write!(
                f,
                "owner is run {} and this helper serves run {}",
                owner.get(),
                sworn.get()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: Compat = Compat::from_raw(18);

    fn owner(run: u64, compat: u64) -> Identity {
        Identity::new(Run::from_raw(run), Compat::from_raw(compat))
    }

    #[test]
    fn the_first_owner_seen_is_adopted_not_refused() {
        // A helper that gave its role up on the first handshake would be
        // restarted straight back into the same verdict.
        let allegiance = Allegiance::new(OURS);
        assert_eq!(allegiance.observe(owner(7, 18)), Standing::Current);
        assert_eq!(allegiance.owner(), Some(Run::from_raw(7)));
    }

    #[test]
    fn the_same_owner_keeps_the_helper() {
        let allegiance = Allegiance::new(OURS);
        assert_eq!(allegiance.observe(owner(7, 18)), Standing::Current);
        assert_eq!(allegiance.observe(owner(7, 18)), Standing::Current);
    }

    #[test]
    fn a_restarted_owner_supersedes_the_helper() {
        let allegiance = Allegiance::to(OURS, Run::from_raw(7));
        assert_eq!(
            allegiance.observe(owner(9, 18)),
            Standing::Superseded(Because::NewRun {
                sworn: Run::from_raw(7),
                owner: Run::from_raw(9),
            })
        );
    }

    #[test]
    fn a_spawn_time_hint_catches_an_orphan_on_its_very_first_handshake() {
        let allegiance = Allegiance::to(OURS, Run::from_raw(7));
        assert!(matches!(
            allegiance.observe(owner(9, 18)),
            Standing::Superseded(_)
        ));
    }

    #[test]
    fn incompatibility_outranks_the_run_check() {
        // The run cannot be trusted across an incompatible boundary, so it
        // must not be what the helper reports — or acts on.
        let allegiance = Allegiance::to(OURS, Run::from_raw(7));
        assert_eq!(
            allegiance.observe(owner(7, 17)),
            Standing::Superseded(Because::Incompatible {
                ours: OURS,
                owner: Compat::from_raw(17),
            })
        );
    }
}
