//! The role's filesystem behavior, which unit tests cannot cover: an actual
//! lock, an actual claim file, and what an onlooker makes of them.

#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic helpers are idiomatic in tests"
)]

use std::fs;
use std::path::{Path, PathBuf};

use succession::{ClaimError, Compat, Identity, Occupancy, Record, Role, Run, Tenant};

const PROTOCOL: Compat = Compat::from_raw(18);

/// A directory of its own per test, so the tests can run in parallel.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!("succession-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn record(run: u64) -> Record {
    Record::new(
        Identity::new(Run::from_raw(run), PROTOCOL),
        Tenant::current().started(1_723_800_000),
    )
}

#[test]
fn an_unclaimed_role_is_free() {
    let scratch = Scratch::new("unclaimed");
    let role = Role::new(scratch.path(), "helper");
    assert_eq!(role.occupancy().unwrap(), Occupancy::Free);
    assert!(
        !role.lock_path().exists(),
        "probing must not create the lock file"
    );
}

#[test]
fn taking_the_role_is_what_publishes_the_record() {
    let scratch = Scratch::new("published");
    let role = Role::new(scratch.path(), "helper");
    let _tenancy = role
        .claim(&record(7))
        .expect("a free role must be claimable");

    match role.occupancy().unwrap() {
        Occupancy::HeldBy(seen) => assert_eq!(seen, record(7)),
        other => panic!("expected an identified tenant, got {other:?}"),
    }
}

#[test]
fn a_record_that_cannot_be_written_gives_the_role_straight_back() {
    // A tenant nobody can identify holds the role while being unrecognizable
    // to the next run, which is worse than no tenant at all: the role must
    // come back free so an attempt that can identify itself gets its turn.
    let scratch = Scratch::new("unpublishable");
    let role = Role::new(scratch.path(), "helper");
    fs::create_dir(role.record_path()).expect("a directory where the record belongs");

    let error = role
        .claim(&record(7))
        .expect_err("a record that cannot be written must fail the claim");

    assert!(
        matches!(error, ClaimError::Unpublishable(_)),
        "expected an unpublishable claim, got {error:?}"
    );
    assert_eq!(
        role.occupancy().unwrap(),
        Occupancy::Free,
        "the lock must be released along with the failed claim"
    );
}

#[test]
fn an_unreadable_record_does_not_hide_the_tenant() {
    let scratch = Scratch::new("unreadable");
    let role = Role::new(scratch.path(), "helper");
    let _tenancy = role
        .claim(&record(7))
        .expect("a free role must be claimable");
    fs::write(role.record_path(), "this is not a claim record").expect("writing junk");
    // Garbage costs identification, never liveness: the tenant is still there.
    assert_eq!(role.occupancy().unwrap(), Occupancy::HeldAnonymously);
}

#[test]
fn a_held_role_cannot_be_claimed_again() {
    let scratch = Scratch::new("contested");
    let role = Role::new(scratch.path(), "helper");
    let _tenancy = role
        .claim(&record(7))
        .expect("a free role must be claimable");
    assert!(matches!(role.claim(&record(9)), Err(ClaimError::Occupied)));
}

#[test]
fn releasing_a_tenancy_frees_the_role() {
    let scratch = Scratch::new("released");
    let role = Role::new(scratch.path(), "helper");
    let tenancy = role
        .claim(&record(7))
        .expect("a free role must be claimable");
    drop(tenancy);

    assert_eq!(role.occupancy().unwrap(), Occupancy::Free);
    assert!(
        !role.record_path().exists(),
        "a released tenancy takes its record with it"
    );
}

#[test]
fn a_record_left_behind_by_a_dead_tenant_is_ignored() {
    // The crash case: the lock died with the process, but the claim file did
    // not. Liveness comes from the lock, so the leftover must not read as a
    // tenant — otherwise every crash would wedge the role permanently.
    let scratch = Scratch::new("leftover");
    let role = Role::new(scratch.path(), "helper");
    fs::write(role.record_path(), record(7).to_text()).expect("writing a leftover record");
    fs::write(role.lock_path(), "").expect("writing a leftover lock file");

    assert_eq!(role.occupancy().unwrap(), Occupancy::Free);
}

#[test]
fn republishing_replaces_the_record() {
    let scratch = Scratch::new("republished");
    let role = Role::new(scratch.path(), "helper");
    let tenancy = role
        .claim(&record(7))
        .expect("a free role must be claimable");
    tenancy.publish(&record(9)).expect("republishing");

    match role.occupancy().unwrap() {
        Occupancy::HeldBy(seen) => assert_eq!(seen.identity.run, Run::from_raw(9)),
        other => panic!("expected an identified tenant, got {other:?}"),
    }
    let leftovers = fs::read_dir(scratch.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
        .count();
    assert_eq!(leftovers, 0, "publishing must not leave temporary files");
}
