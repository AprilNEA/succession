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

The helper takes the role, says who it is, and re-checks its owner on every
handshake:

```rust,no_run
use succession::{Allegiance, Identity, Record, Role, Standing, Tenant};
# use succession::{Compat, Run};
# const PROTOCOL: Compat = Compat::from_raw(18);
# let spawned_by = Run::from_raw(1);
# fn owner_identity() -> Identity { Identity::mine(Compat::from_raw(18)) }
let role = Role::new("/run/my-app", "overlay");
let tenancy = role.claim()?;                       // Occupied => someone else is the overlay
tenancy.publish(&Record::new(Identity::new(spawned_by, PROTOCOL), Tenant::current()))?;

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
    Verdict::EvictAnonymous => { /* a tenant that never identified itself */ }
}
# Ok::<(), std::io::Error>(())
```

## What it does not do

No dependencies, and no liberties: it does not spawn, signal, or exit, it does
not resolve paths, and it does not schedule retries. Process supervision with
restart policies and containment is [`processkit`](https://docs.rs/processkit)'s
job; this crate is the decision layer — who is in the seat, and may I have it.

## License

MIT OR Apache-2.0.
