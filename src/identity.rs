//! Who an owner process is: which run, and what it can talk to.

use std::hash::{BuildHasher as _, RandomState};
use std::sync::OnceLock;

/// One run of an owner process.
///
/// Equality means "the same process instance", which is the fact a subordinate
/// helper needs and cannot get from a version number: an owner that restarts
/// without changing its build reports the same version and a different run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Run(u64);

impl Run {
    /// Draw this process's run token, stable for the rest of its life.
    ///
    /// Drawn from the OS-seeded hasher state the standard library uses for
    /// `HashMap`, so two processes practically never agree. It is not
    /// cryptographic and carries no ordering: the only property required is
    /// that a later run differs from an earlier one, which rules out a pid
    /// (reused) and a start timestamp (a clock can step backwards).
    #[must_use]
    pub fn mint() -> Self {
        static MINE: OnceLock<u64> = OnceLock::new();
        Self(*MINE.get_or_init(|| RandomState::new().hash_one(std::process::id())))
    }

    /// Rebuild a token from its wire or file representation.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The token as carried on a wire or in a claim record.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What an owner and a helper must agree on before they can serve each other:
/// a protocol version, an ABI revision, a build fingerprint — whatever the
/// application bumps when the two sides stop understanding one another.
///
/// A helper that sees a different value is not out of date by chance; it is
/// from an install that has been replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Compat(u64);

impl Compat {
    /// Build a fingerprint in a `const` context, where [`From`] cannot go.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The value as carried on a wire or in a claim record.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for Compat {
    fn from(raw: u64) -> Self {
        Self(raw)
    }
}

impl From<u32> for Compat {
    fn from(raw: u32) -> Self {
        Self(u64::from(raw))
    }
}

/// Identity of one run of an owner process.
///
/// # The readability contract
///
/// A helper decides whether to stay by comparing this identity, so it has to
/// be able to *read* the identity of an owner it may be incompatible with.
/// Whatever channel carries [`Compat`] must therefore be frozen: readable by
/// every past and future build, never extended. In a request/response protocol
/// that usually means a dedicated method whose position and encoding never
/// change; [`Run`] may then be read through any channel, because it is only
/// consulted once the two `Compat` values agree.
///
/// Getting this backwards — putting the version inside a payload whose shape
/// changes with the version — is what makes an obsolete helper unable to learn
/// that it is obsolete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    /// Which run of the owner.
    pub run: Run,
    /// What that run can talk to.
    pub compat: Compat,
}

impl Identity {
    /// Pair a run with the compatibility fingerprint it serves.
    #[must_use]
    pub const fn new(run: Run, compat: Compat) -> Self {
        Self { run, compat }
    }

    /// This process's identity for `compat`.
    #[must_use]
    pub fn mine(compat: Compat) -> Self {
        Self::new(Run::mint(), compat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_keeps_one_run_token() {
        assert_eq!(Run::mint(), Run::mint());
    }

    #[test]
    fn a_token_survives_a_round_trip_through_its_raw_form() {
        let minted = Run::mint();
        assert_eq!(Run::from_raw(minted.get()), minted);
    }

    #[test]
    fn identities_differ_when_either_half_differs() {
        let run = Run::from_raw(1);
        let other = Run::from_raw(2);
        let compat = Compat::from(7_u32);
        assert_ne!(Identity::new(run, compat), Identity::new(other, compat));
        assert_ne!(
            Identity::new(run, compat),
            Identity::new(run, Compat::from(8_u32))
        );
    }
}
