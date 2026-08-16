//! The role itself: taking it, publishing who took it, and looking at who has.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::record::Record;

/// A job exactly one live process may do at a time.
///
/// Backed by two files in a directory the caller chooses: `<name>.lock`, whose
/// advisory lock proves a tenant is alive, and `<name>.claim`, which says who
/// the tenant is. The split is deliberate — the lock file's inode is never
/// rewritten, so a reader can never catch it half-written, and a stale claim
/// file left by a crash is harmless because [`Role::occupancy`] only reads it
/// while the lock is actually held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    lock: PathBuf,
    record: PathBuf,
}

impl Role {
    /// A role named `name`, kept in `dir`.
    ///
    /// Resolving `dir` is the caller's business: this crate has no opinion on
    /// where an application's runtime state belongs, and no way to know
    /// whether the caller separates development from production installs.
    #[must_use]
    pub fn new(dir: impl AsRef<Path>, name: &str) -> Self {
        let dir = dir.as_ref();
        Self {
            lock: dir.join(format!("{name}.lock")),
            record: dir.join(format!("{name}.claim")),
        }
    }

    /// Take the role, creating the directory if needed.
    ///
    /// Publish a [`Record`] immediately afterwards — until then the tenancy is
    /// anonymous, and an onlooker can only tell that *someone* holds the role.
    ///
    /// # Errors
    ///
    /// [`ClaimError::Occupied`] when another process holds the role, which is
    /// the ordinary "not me, then" answer rather than a fault;
    /// [`ClaimError::Io`] when the lock file cannot be created or locked.
    pub fn claim(&self) -> Result<Tenancy, ClaimError> {
        if let Some(parent) = self.lock.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock)?;
        match file.try_lock() {
            Ok(()) => Ok(Tenancy {
                _lock: file,
                record: self.record.clone(),
            }),
            Err(fs::TryLockError::WouldBlock) => Err(ClaimError::Occupied),
            Err(fs::TryLockError::Error(source)) => Err(ClaimError::Io(source)),
        }
    }

    /// Who holds the role right now.
    ///
    /// A probe with no side effects: it never creates the lock file, and it
    /// releases the lock immediately if it turns out to be free. That makes
    /// [`Occupancy::Free`] a snapshot rather than a reservation — a caller
    /// acting on it must still tolerate losing the race to another starter.
    ///
    /// # Errors
    ///
    /// The underlying [`io::Error`] when the lock file exists but cannot be
    /// opened or locked. A caller that wants to keep going should treat that
    /// as [`Occupancy::Free`] and let the real [`Role::claim`] report the
    /// trouble properly; assuming "held" instead would wait forever.
    pub fn occupancy(&self) -> io::Result<Occupancy> {
        let file = match OpenOptions::new().read(true).write(true).open(&self.lock) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Occupancy::Free),
            Err(error) => return Err(error),
        };
        match file.try_lock() {
            Ok(()) => Ok(Occupancy::Free),
            Err(fs::TryLockError::WouldBlock) => Ok(self.identify_holder()),
            Err(fs::TryLockError::Error(source)) => Err(source),
        }
    }

    /// The lock file backing this role.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock
    }

    /// The claim file backing this role.
    #[must_use]
    pub fn record_path(&self) -> &Path {
        &self.record
    }

    fn identify_holder(&self) -> Occupancy {
        fs::read_to_string(&self.record)
            .ok()
            .and_then(|text| Record::parse(&text).ok())
            .map_or(Occupancy::HeldAnonymously, Occupancy::HeldBy)
    }
}

/// Proof that this process holds a role. Dropping it gives the role up.
///
/// The lock is released by the operating system when the file closes, so a
/// tenancy also ends when the process dies however unceremoniously — which is
/// what makes crash recovery free and a stale claim file harmless.
#[derive(Debug)]
pub struct Tenancy {
    // Held only so the OS keeps the lock; never read again.
    _lock: File,
    record: PathBuf,
}

impl Tenancy {
    /// Say who is holding the role.
    ///
    /// Written to a temporary file and renamed into place, so a reader sees
    /// either the previous record or this one, never a half-written line.
    /// Call it again to correct the record if the facts change.
    ///
    /// # Errors
    ///
    /// The underlying [`io::Error`]. Publication is advisory: a tenant that
    /// cannot publish still holds the role, and callers are expected to log
    /// the failure and carry on rather than give the role up.
    pub fn publish(&self, record: &Record) -> io::Result<()> {
        let mut temporary = self.record.clone().into_os_string();
        temporary.push(format!(".{}.tmp", std::process::id()));
        let temporary = PathBuf::from(temporary);
        let mut file = File::create(&temporary)?;
        file.write_all(record.to_text().as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &self.record).inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
    }
}

impl Drop for Tenancy {
    fn drop(&mut self) {
        // Best effort: the lock closing is what actually frees the role, and a
        // leftover record is ignored by every reader while the lock is free.
        let _ = fs::remove_file(&self.record);
    }
}

/// Who, if anyone, is filling a role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// Nobody holds it.
    Free,
    /// Held by a tenant that said who it is.
    HeldBy(Record),
    /// Held by something that published no readable record — an older build
    /// that predates the convention, a foreign process, or a tenant that could
    /// not write. It is alive, but nothing about it can be reasoned with.
    HeldAnonymously,
}

impl fmt::Display for Occupancy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Free => f.write_str("free"),
            Self::HeldBy(record) => write!(f, "held by {record}"),
            Self::HeldAnonymously => f.write_str("held by an unidentified tenant"),
        }
    }
}

/// Why a role could not be taken.
#[derive(Debug)]
pub enum ClaimError {
    /// Another live process holds the role. The expected answer, not a fault.
    Occupied,
    /// The lock file could not be created, opened, or locked.
    Io(io::Error),
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occupied => f.write_str("the role is already held"),
            Self::Io(_) => f.write_str("the role's lock file could not be taken"),
        }
    }
}

impl Error for ClaimError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Occupied => None,
            Self::Io(source) => Some(source),
        }
    }
}

impl From<io::Error> for ClaimError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}
