//! One process per role, bound to one run of its owner.
//!
//! An advisory lock keeps a helper process unique — until the process that
//! spawned it goes away. The orphan keeps the lock, so nothing can start the
//! helper the new run needs, and it does not look broken: it looks exactly
//! like a healthy tenant.
//!
//! This crate pairs the lock with an identity — which run a tenant serves — so
//! an onlooker can recognize an obsolete tenant ([`verdict`]) and the tenant
//! can recognize that its own run is over ([`Allegiance`]).
//!
//! Roles come in three kinds: peers negotiate with each other, subordinates
//! serve one run of an owner, and user-owned ones are never evicted at all.
//! This crate implements the subordinate kind.
//!
//! # Two contracts
//!
//! 1. **[`Compat`] must be readable across incompatible versions.** A helper
//!    decides whether to stay by reading its owner's identity, so that read
//!    cannot depend on the two agreeing: give `Compat` a frozen channel — a
//!    protocol method whose position and encoding never change — and read
//!    [`Run`] only once the fingerprints match.
//! 2. **The claim record must be tolerant.** [`Record::parse`] ignores unknown
//!    keys, so a record from a newer build still reads, and an unreadable one
//!    degrades to [`Occupancy::HeldAnonymously`] rather than to an error.
//!
//! Anonymity is the state both contracts exist to avoid, because a tenant
//! nobody can identify cannot be reasoned with — only
//! [`eviction::evict_anonymous`] can remove it, and only by recognizing the
//! image it runs. [`Role::claim`] therefore writes the record itself and hands
//! the role back if it cannot, so the state is reached by inheritance from
//! older installs rather than manufactured anew.
//!
//! # Features
//!
//! The core is dependency-free and decides only: it never spawns, signals,
//! exits, or resolves a path. Each feature adds one piece of glue.
//!
//! | Feature | Adds | Dependency |
//! |---|---|---|
//! | `serde` | Serialization for the identity types, for carrying [`Identity`] on an application's own wire | `serde` |
//! | `sysinfo` | `Tenant::look_up`, so live process facts need not be supplied by hand | `sysinfo` |
//! | `eviction` | `eviction::evict`, which verifies a pid before signalling it and waits for the role to be released, and `eviction::evict_anonymous` for a tenant that published no record | `sysinfo` |
//! | `supervision` | `supervision::Supervisor`, a probe/spawn/wait/back-off loop over the verdict | none |
//!
//! Whole-process-tree containment and async supervision stay out of scope;
//! [`processkit`](https://docs.rs/processkit) does those well.
//!
//! # Examples
//!
//! The helper takes the role, publishes who it is, and re-checks its owner on
//! every handshake:
//!
//! ```no_run
//! use succession::{Allegiance, ClaimError, Compat, Identity, Record, Role, Run, Standing, Tenant};
//!
//! # fn owner_identity() -> Identity { Identity::mine(Compat::from_raw(18)) }
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! const PROTOCOL: Compat = Compat::from_raw(18);
//! let spawned_by = Run::from_raw(std::env::var("MY_APP_RUN")?.parse()?);
//!
//! let role = Role::new("/run/my-app", "overlay");
//! let record = Record::new(Identity::new(spawned_by, PROTOCOL), Tenant::current());
//! let _tenancy = match role.claim(&record) {
//!     Ok(tenancy) => tenancy,
//!     // Someone is already the overlay. Our supervisor will deal with it.
//!     Err(ClaimError::Occupied) => return Ok(()),
//!     Err(error) => return Err(error.into()),
//! };
//!
//! let allegiance = Allegiance::to(PROTOCOL, spawned_by);
//! if let Standing::Superseded(because) = allegiance.observe(owner_identity()) {
//!     eprintln!("stepping aside: {because}");
//!     return Ok(()); // dropping `tenancy` frees the role for the replacement
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Its owner's supervisor asks whether the seat is free before filling it:
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//! use succession::{Role, Run, Verdict, verdict};
//!
//! # fn spawn_helper() {}
//! # fn ask_to_leave(_: u32) {}
//! # fn main() -> std::io::Result<()> {
//! let role = Role::new("/run/my-app", "overlay");
//! let waiting_since = Instant::now();
//!
//! match verdict(&role.occupancy()?, Run::mint(), waiting_since.elapsed(), Duration::from_secs(15))
//! {
//!     Verdict::Start => spawn_helper(),
//!     Verdict::Wait => std::thread::sleep(Duration::from_millis(500)),
//!     // Verify the pid is still that process before signalling it.
//!     Verdict::Evict(record) => ask_to_leave(record.tenant.pid),
//!     // No record to verify against: `eviction::evict_anonymous` finds the
//!     // tenant by the image this role's helper runs from.
//!     Verdict::EvictAnonymous => {}
//! }
//! # Ok(())
//! # }
//! ```

#![cfg_attr(docsrs, feature(doc_auto_cfg))]

/// Compile-tests the README's examples, so the front page cannot drift away
/// from the API it advertises.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

#[cfg(feature = "eviction")]
pub mod eviction;
#[cfg(feature = "supervision")]
pub mod supervision;

mod identity;
mod record;
mod role;
mod standing;
mod verdict;

pub use identity::{Compat, Identity, Run};
pub use record::{MalformedRecord, Record, Sameness, Tenant};
pub use role::{ClaimError, Occupancy, Role, Tenancy};
pub use standing::{Allegiance, Because, Standing};
pub use verdict::{Verdict, verdict};
