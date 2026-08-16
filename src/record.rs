//! The identity a tenant writes beside the lock, and how to read it back.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::identity::{Compat, Identity, Run};

/// The OS-level handle to a process filling a role.
///
/// `pid` alone cannot identify a process — the OS reuses pids — so a tenant
/// records whatever corroborating facts it has. Each of them is optional
/// because not every caller can obtain them without a process-inspection
/// dependency; supplying them is what lets an evictor prove it is signalling
/// the process it read about. See [`Tenant::compare`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    /// Process id.
    pub pid: u32,
    /// Process start time, in whatever unit the caller's process-inspection
    /// source reports. Compared for equality only, so the unit only has to be
    /// consistent within one application.
    pub started_at: Option<u64>,
    /// Executable image behind the process.
    pub image: Option<PathBuf>,
}

impl Tenant {
    /// This process, described with what the standard library can see: its pid
    /// and its executable path.
    ///
    /// Add [`Tenant::started`] when a process-inspection source is available —
    /// without it an evictor can only reach [`Sameness::Inconclusive`].
    #[must_use]
    pub fn current() -> Self {
        Self {
            pid: std::process::id(),
            started_at: None,
            image: std::env::current_exe().ok(),
        }
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
    /// Call this before signalling anyone. A record can outlive its writer,
    /// and the pid it names may since belong to something unrelated.
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
///
/// [`Sameness::Inconclusive`] is not a soft "probably": it means the record
/// carried nothing beyond a pid, so the answer is unknown. Treat it as a
/// reason to be careful — logging it, or refusing to escalate past a polite
/// signal — rather than as a yes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sameness {
    /// Corroborated: same pid, and every fact both sides know agrees.
    Same,
    /// Refuted: a different pid, or a fact that disagrees.
    Different,
    /// Only the pid was available to compare.
    Inconclusive,
}

/// What a tenant publishes about itself while it holds a role.
///
/// The record is advisory. The lock is what proves a tenant is alive; this is
/// what lets an onlooker say *which* tenant, and therefore whether it belongs
/// to the current run of the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The owner run this tenant serves.
    pub identity: Identity,
    /// The process filling the role.
    pub tenant: Tenant,
}

/// Marker line written first so a human reading the file — or a future parser
/// — can tell what it is looking at.
const HEADER: &str = "# succession 1";

impl Record {
    /// Pair an owner identity with the process serving it.
    #[must_use]
    pub const fn new(identity: Identity, tenant: Tenant) -> Self {
        Self { identity, tenant }
    }

    /// Render the record in the on-disk format.
    ///
    /// Deliberately a boring `key = value` text file: it has to be readable by
    /// every past and future build of every process in the tree, and by a
    /// person debugging at 2am. Readers ignore keys they do not know, so new
    /// facts can be added without a format version.
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

    /// Parse a record.
    ///
    /// Unknown keys, comments and blank lines are ignored, and every optional
    /// fact may be absent, so a record written by a newer build still reads.
    ///
    /// # Errors
    ///
    /// [`MalformedRecord`] if a required key is missing or a numeric value
    /// does not parse. Callers reading a claim file should treat any error as
    /// "held by someone who did not identify themselves" rather than as a
    /// failure: an unreadable record is exactly what an obsolete tenant leaves.
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
/// Written out by hand rather than derived: this crate carries no
/// dependencies, so that every process in a tree can depend on it without
/// inheriting a build graph.
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
        // The forward-compatibility rule: a newer tenant may write more.
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
}
