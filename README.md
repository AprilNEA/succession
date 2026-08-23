# succession

One process per role, bound to one run of its owner — and an orderly handover
when that run ends.

An advisory lock is the usual way to keep a single helper process unique: whoever
takes it is the one. That works until the process that spawned the helper goes
away — an update, a crash, a restart. The helper keeps running and keeps the
lock, so nothing can start the helper that belongs to the *new* run. It does not
look broken; it looks exactly like a healthy tenant.

This crate is the missing half of that lock: an identity beside it, so an
onlooker can tell which run a tenant belongs to, and a helper can tell when the
run it serves is over.

## Usage

The owner mints one identity per run and passes it down when it spawns a helper:

```rust
use succession::{Compat, Identity};

const PROTOCOL: Compat = Compat::from_raw(18);
let me = Identity::mine(PROTOCOL);
// Answer `me` over IPC, and pass `me.run.get()` to the helper at spawn.
```

The helper takes the role — saying who it is in the same breath, because a
tenant nobody can identify is worse than none — and re-checks its owner on
every handshake:

```rust,no_run
use succession::{Allegiance, Identity, Record, Role, Standing, Tenant};
# use succession::{Compat, Run};
# const PROTOCOL: Compat = Compat::from_raw(18);
# let spawned_by = Run::from_raw(1);
# fn owner_identity() -> Identity { Identity::mine(Compat::from_raw(18)) }
let role = Role::new("/run/my-app", "overlay");
let record = Record::new(Identity::new(spawned_by, PROTOCOL), Tenant::current());
let tenancy = role.claim(&record)?;                // Occupied => someone else is the overlay

if let Standing::Superseded(because) = Allegiance::to(PROTOCOL, spawned_by).observe(owner_identity())
{
    eprintln!("stepping aside: {because}");        // dropping `tenancy` frees the role
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

The owner's supervisor asks whether the seat is free before filling it:

```rust,no_run
use std::time::Duration;
use succession::{Role, Run, Verdict, verdict};

let role = Role::new("/run/my-app", "overlay");
match verdict(&role.occupancy()?, Run::mint(), Duration::ZERO, Duration::from_secs(15)) {
    Verdict::Start => { /* spawn the helper */ }
    Verdict::Wait => { /* look again shortly */ }
    Verdict::Evict(record) => { /* verify the pid, then ask it to leave */ }
    // A tenant that never identified itself: `eviction::evict_anonymous`
    // recognizes it by the image this role's helper runs from.
    Verdict::EvictAnonymous => {}
}
# Ok::<(), std::io::Error>(())
```

## Features

The default build decides and nothing else: no dependencies, and it never
spawns, signals, exits, or resolves a path. Each feature adds one piece of
glue, so a process that only needs the decision pays for nothing else.

| Feature | Adds | Dependency |
|---|---|---|
| `serde` | Serialization for the identity types, for carrying `Identity` on your own wire | `serde` |
| `sysinfo` | `Tenant::look_up`, so live process facts need not be supplied by hand | `sysinfo` |
| `eviction` | `eviction::evict`, which verifies a pid against the record before signalling it, escalates on a deadline, and waits for the role to be released | `sysinfo` |
| `supervision` | `supervision::Supervisor`, a probe/spawn/wait/back-off loop over the verdict, reporting events rather than logging them | none |

Whole-process-tree containment and async supervision stay out of scope;
[`processkit`](https://docs.rs/processkit) does those well.

## License

MIT OR Apache-2.0.
