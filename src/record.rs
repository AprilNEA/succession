//! The identity a tenant writes beside the lock, and how to read it back.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::identity::{Compat, Identity, Run};

/// The OS-level handle to a process filling a role.
///
/// `pid` alone cannot identify a process — the OS reuses pids — so a tenant
/// records whatever corroborating facts it has. They are optional because not
/// every caller can obtain them; supplying them is what lets an evictor prove
/// it is signalling the process it read about. See [`Tenant::compare`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tenant {
    /// Process id.
    pub pid: u32,
    /// Process start time, in whatever unit the caller's source reports.
    /// Compared for equality only, so the unit need only be consistent within
    /// one application.
    pub started_at: Option<u64>,
    /// Executable image behind the process.
    pub image: Option<PathBuf>,
}

impl Tenant {
    /// This process.
    ///
    /// With the `sysinfo` feature this includes the start time; without it,
    /// only what the standard library can see — the pid and the executable
    /// path — which leaves an evictor at [`Sameness::Inconclusive`].
    #[must_use]
    pub fn current() -> Self {
        #[cfg(feature = "sysinfo")]
        if let Some(detailed) = Self::look_up(std::process::id()) {
            return detailed;
        }
        Self {
            pid: std::process::id(),
            started_at: None,
            image: std::env::current_exe().ok(),
        }
    }

    /// The live facts for `pid`, or `None` if no such process is running.
    ///
    /// The lookup an evictor needs before it signals anyone: pass the result
    /// to [`Tenant::compare`] against the record.
    #[cfg(feature = "sysinfo")]
    #[must_use]
    pub fn look_up(pid: u32) -> Option<Self> {
        use sysinfo::{Pid, ProcessesToUpdate, System};

        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let process = system.process(pid)?;
        Some(Self {
            pid: process.pid().as_u32(),
            started_at: Some(process.start_time()),
            image: process.exe().map(Path::to_path_buf),
        })
    }

    /// Add the process start time an inspection source reported.
    #[must_use]
    pub fn started(mut self, at: u64) -> Self {
        self.started_at = Some(at);
        self
    }

    /// Whether `live` — the facts just looked up for this record's pid — is
    /// still the process that wrote the record.
    ///
    /// Call this before signalling anyone: a record can outlive its writer, and
    /// its pid may since belong to something unrelated.
    #[must_use]
    pub fn compare(&self, live: &Self) -> Sameness {
        if self.pid != live.pid {
            return Sameness::Different;
        }
        let start = Corroboration::of(self.started_at.as_ref(), live.started_at.as_ref());
        let image = Corroboration::of(self.image.as_ref(), live.image.as_ref());
        match (start, image) {
            (Corroboration::Contradicts, _) | (_, Corroboration::Contradicts) => {
                Sameness::Different
            }
            (Corroboration::Confirms, _) | (_, Corroboration::Confirms) => Sameness::Same,
            (Corroboration::Unknown, Corroboration::Unknown) => Sameness::Inconclusive,
        }
    }
}

/// What one recorded fact says about two processes sharing a pid.
enum Corroboration {
    Confirms,
    Contradicts,
    Unknown,
}

impl Corroboration {
    fn of<T: PartialEq>(recorded: Option<&T>, live: Option<&T>) -> Self {
        match (recorded, live) {
            (Some(recorded), Some(live)) if recorded == live => Self::Confirms,
            (Some(_), Some(_)) => Self::Contradicts,
            _ => Self::Unknown,
        }
    }
}

/// Whether a live process is the one a record describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Sameness {
    /// Same pid, and every fact both sides know agrees.
    Same,
    /// A different pid, or a fact that disagrees.
    Different,
    /// Only the pid was available to compare. Not a soft "probably" — the
    /// answer is unknown, which is a reason to refuse to escalate.
    Inconclusive,
}

/// What a tenant publishes about itself while it holds a role.
///
/// Advisory: the lock proves a tenant is alive, the record says *which* tenant,
/// and therefore whether it belongs to the current run of the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Record {
    /// The owner run this tenant serves.
    pub identity: Identity,
    /// The process filling the role.
    pub tenant: Tenant,
}

/// Marker line, so a person — or a future parser — can tell what the file is.
const HEADER: &str = "# succession 1";

impl Record {
    /// Pair an owner identity with the process serving it.
    #[must_use]
    pub const fn new(identity: Identity, tenant: Tenant) -> Self {
        Self { identity, tenant }
    }

    /// Render the record in the on-disk format: a boring `key = value` text
    /// file, because every build in the tree has to be able to read it.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = String::from(HEADER);
        text.push_str("\nrun = ");
        text.push_str(&self.identity.run.get().to_string());
        text.push_str("\ncompat = ");
        text.push_str(&self.identity.compat.get().to_string());
        text.push_str("\npid = ");
        text.push_str(&self.tenant.pid.to_string());
        if let Some(started_at) = self.tenant.started_at {
            text.push_str("\nstarted_at = ");
            text.push_str(&started_at.to_string());
        }
        if let Some(image) = self.tenant.image.as_deref().and_then(Path::to_str) {
            text.push_str("\nimage = ");
            text.push_str(image);
        }
        text.push('\n');
        text
    }

    /// Parse a record. Unknown keys and absent optional facts are tolerated, so
    /// a record written by a newer build still reads.
    ///
    /// # Errors
    ///
    /// [`MalformedRecord`] if a required key is missing or a numeric value does
    /// not parse. A reader should treat any error as "held by someone who did
    /// not identify themselves": an unreadable record is exactly what an
    /// obsolete tenant leaves.
    pub fn parse(text: &str) -> Result<Self, MalformedRecord> {
        let mut run = None;
        let mut compat = None;
        let mut pid = None;
        let mut started_at = None;
        let mut image = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "run" => run = Some(number(value, "run")?),
                "compat" => compat = Some(number(value, "compat")?),
                "pid" => {
                    let parsed = number(value, "pid")?;
                    pid = Some(
                        u32::try_from(parsed)
                            .map_err(|_| MalformedRecord::NotANumber { key: "pid" })?,
                    );
                }
                "started_at" => started_at = Some(number(value, "started_at")?),
                "image" => image = Some(PathBuf::from(value)),
                _ => {}
            }
        }

        Ok(Self {
            identity: Identity::new(
                Run::from_raw(run.ok_or(MalformedRecord::Missing { key: "run" })?),
                Compat::from(compat.ok_or(MalformedRecord::Missing { key: "compat" })?),
            ),
            tenant: Tenant {
                pid: pid.ok_or(MalformedRecord::Missing { key: "pid" })?,
                started_at,
                image,
            },
        })
    }
}

fn number(value: &str, key: &'static str) -> Result<u64, MalformedRecord> {
    value
        .parse()
        .map_err(|_| MalformedRecord::NotANumber { key })
}

/// Why a claim record could not be read.
///
/// Implemented by hand rather than derived: the core of this crate carries no
/// dependencies, so that every process in a tree can link it freely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MalformedRecord {
    /// A key the record cannot do without is absent.
    Missing {
        /// The absent key.
        key: &'static str,
    },
    /// A key that must hold a number holds something else.
    NotANumber {
        /// The offending key.
        key: &'static str,
    },
}

impl fmt::Display for MalformedRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { key } => write!(f, "claim record has no `{key}`"),
            Self::NotANumber { key } => write!(f, "claim record's `{key}` is not a number"),
        }
    }
}

impl Error for MalformedRecord {}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "panic helpers are idiomatic in tests")]
mod tests {
    use super::*;

    fn record() -> Record {
        Record::new(
            Identity::new(Run::from_raw(7), Compat::from(18_u32)),
            Tenant {
                pid: 4242,
                started_at: Some(1_723_800_000),
                image: Some(PathBuf::from("/opt/app/helper")),
            },
        )
    }

    #[test]
    fn a_record_survives_a_round_trip() {
        let written = record();
        let read = Record::parse(&written.to_text()).expect("a written record must parse");
        assert_eq!(read, written);
    }

    #[test]
    fn optional_facts_may_be_absent() {
        let bare = Record::new(
            Identity::new(Run::from_raw(1), Compat::from(2_u32)),
            Tenant {
                pid: 3,
                started_at: None,
                image: None,
            },
        );
        let read = Record::parse(&bare.to_text()).expect("a bare record must parse");
        assert_eq!(read, bare);
    }

    #[test]
    fn a_reader_ignores_what_it_does_not_know() {
        // Forward compatibility: a newer tenant may write more.
        let text = format!("{}\nfuture_fact = 9\n", record().to_text());
        assert_eq!(Record::parse(&text).expect("must still parse"), record());
    }

    #[test]
    fn a_path_containing_the_separator_survives() {
        let mut odd = record();
        odd.tenant.image = Some(PathBuf::from("/opt/a=b/helper"));
        let read = Record::parse(&odd.to_text()).expect("must parse");
        assert_eq!(read.tenant.image, odd.tenant.image);
    }

    #[test]
    fn required_keys_are_required() {
        assert_eq!(
            Record::parse("# succession 1\ncompat = 1\npid = 2\n"),
            Err(MalformedRecord::Missing { key: "run" })
        );
        assert_eq!(
            Record::parse("run = 1\ncompat = 1\npid = wat\n"),
            Err(MalformedRecord::NotANumber { key: "pid" })
        );
        assert_eq!(
            Record::parse("run = 1\ncompat = 1\npid = 99999999999\n"),
            Err(MalformedRecord::NotANumber { key: "pid" })
        );
    }

    #[test]
    fn a_pid_alone_cannot_confirm_a_process() {
        let recorded = Tenant {
            pid: 10,
            started_at: None,
            image: None,
        };
        let live = Tenant {
            pid: 10,
            started_at: Some(5),
            image: None,
        };
        assert_eq!(recorded.compare(&live), Sameness::Inconclusive);
    }

    #[test]
    fn one_agreeing_fact_confirms_and_one_disagreeing_fact_refutes() {
        let recorded = record();
        let mut live = recorded.tenant.clone();
        assert_eq!(recorded.tenant.compare(&live), Sameness::Same);

        live.started_at = Some(999);
        assert_eq!(recorded.tenant.compare(&live), Sameness::Different);

        live.started_at = recorded.tenant.started_at;
        live.image = Some(PathBuf::from("/opt/app/something-else"));
        assert_eq!(recorded.tenant.compare(&live), Sameness::Different);

        live.pid = 1;
        assert_eq!(recorded.tenant.compare(&live), Sameness::Different);
    }

    #[cfg(feature = "sysinfo")]
    #[test]
    fn this_process_can_be_looked_up_and_matches_itself() {
        let looked_up = Tenant::look_up(std::process::id()).expect("this process must be visible");
        assert!(
            looked_up.started_at.is_some(),
            "the lookup adds a start time"
        );
        assert_eq!(Tenant::current().compare(&looked_up), Sameness::Same);
    }
}
