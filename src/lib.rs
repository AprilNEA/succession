//! One process per role, bound to one run of its owner.
//!
//! A desktop application is rarely one process. A background service owns the
//! hardware, a window shows the state, a helper draws something on demand.
//! Each of those is a *role* exactly one live process should fill, and the
//! usual way to enforce that is an advisory lock: whoever takes it is the one.
//!
//! That works until the process that spawned the helper goes away — an update,
//! a crash, a restart. The helper keeps running, keeps the lock, and now
//! nothing can start the helper that belongs to the new run. It is not
//! detectably broken: it looks exactly like a healthy tenant. If the restart
//! came from an update it is worse, because the surviving helper may not even
//! be able to talk to its new owner.
//!
//! This crate is the missing half of that lock: **an identity beside it**, so
//! an onlooker can tell *which run* the tenant belongs to, and a helper can
//! tell when the run it serves is over.
//!
//! # Choose a policy per role
//!
//! Before reaching for any mechanism, decide what kind of role this is. There
//! are only three, and the choice determines everything else:
//!
//! | Policy | Who may end it | How it behaves |
//! |---|---|---|
//! | **Peer** | itself | Negotiates with other instances of itself — newest wins, the loser exits |
//! | **Subordinate** | itself, then its owner | Serves one run of an owner; exits when that run ends |
//! | **User-owned** | the person | Never evicted; reports a mismatch and asks the person to act |
//!
//! This crate implements the **subordinate** policy. Getting the choice wrong
//! is the actual bug behind most orphaned helpers: a process written as a
//! subordinate but living like a peer holds a role forever with nobody
//! entitled to take it back.
//!
//! # The two contracts
//!
//! The mechanism is simple; these two rules are what make it work when it
//! matters, which is precisely when the two sides are *not* the same build:
//!
//! 1. **The compatibility fingerprint must be readable across incompatible
//!    versions.** A helper decides whether to stay by reading its owner's
//!    identity, so that read cannot depend on the two agreeing. Give
//!    [`Compat`] a frozen channel — a protocol method whose position and
//!    encoding never change — and read [`Run`] only once the fingerprints
//!    match. See [`Identity`].
//! 2. **The claim record must be tolerant.** [`Record::parse`] ignores keys it
//!    does not know and accepts missing optional facts, so a record written by
//!    a newer build still reads. An unreadable record degrades to
//!    [`Occupancy::HeldAnonymously`] — the tenant is alive but says nothing —
//!    rather than to an error.
//!
//! # What this crate does not do
//!
//! It has no dependencies and takes no liberties. It does not spawn, signal,
//! or exit; it does not resolve paths; it does not schedule retries. Those
//! belong to the caller, or to crates that already do them well — process
//! supervision with restart policies and containment, for instance, is
//! [`processkit`](https://docs.rs/processkit)'s job. What is left here is the
//! decision layer: who is in the seat, and may I have it.
//!
//! # Using it
//!
//! The owner mints one identity per run and passes it down when it spawns the
//! helper, so even the helper's first handshake can catch an orphan:
//!
//! ```
//! use succession::{Compat, Identity};
//!
//! const PROTOCOL: Compat = Compat::from_raw(18);
//!
//! let me = Identity::mine(PROTOCOL);
//! // Answer `me` over IPC, and pass `me.run.get()` to the helper at spawn.
//! ```
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
//! let tenancy = match role.claim() {
//!     Ok(tenancy) => tenancy,
//!     // Someone is already the overlay. Our supervisor will deal with it.
//!     Err(ClaimError::Occupied) => return Ok(()),
//!     Err(error) => return Err(error.into()),
//! };
//! // Advisory: a failure here costs identification, not the role.
//! let _ = tenancy.publish(&Record::new(
//!     Identity::new(spawned_by, PROTOCOL),
//!     Tenant::current(),
//! ));
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
//! The owner's supervisor asks whether the seat is free before filling it:
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//! use succession::{Role, Run, Verdict, verdict};
//!
//! # fn spawn_helper() {}
//! # fn ask_to_leave(_: u32) {}
//! # fn main() -> std::io::Result<()> {
//! let role = Role::new("/run/my-app", "overlay");
//! let mine = Run::mint();
//! let waiting_since = Instant::now();
//!
//! match verdict(&role.occupancy()?, mine, waiting_since.elapsed(), Duration::from_secs(15)) {
//!     Verdict::Start => spawn_helper(),
//!     Verdict::Wait => std::thread::sleep(Duration::from_millis(500)),
//!     // Verify the pid is still that process before signalling it.
//!     Verdict::Evict(record) => ask_to_leave(record.tenant.pid),
//!     Verdict::EvictAnonymous => { /* fall back on what you know out of band */ }
//! }
//! # Ok(())
//! # }
//! ```

/// Compile-tests the README's examples, so the front page cannot drift away
/// from the API it advertises.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

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
