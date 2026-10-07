//! Errors returned by the engine.

use std::io;
use std::path::{Path, PathBuf};

use crate::hash::ObjectId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No `.nexus` directory in the starting directory or any parent.
    #[error(
        "not inside a NexusVCS repository (searched upward from {}); run `nexus init` to create one",
        .0.display()
    )]
    NotARepository(PathBuf),

    #[error("already inside a NexusVCS repository at {}", .0.display())]
    AlreadyARepository(PathBuf),

    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("object {0} is missing from the object store")]
    MissingObject(ObjectId),

    #[error("object {id} is corrupt: {reason}")]
    CorruptObject { id: ObjectId, reason: String },

    /// Repository metadata other than objects (the index, refs, `HEAD`,
    /// `OPLOG`, config) is damaged.
    #[error("{}: {reason}", path.display())]
    Corrupt { path: PathBuf, reason: String },

    #[error(
        "another nexus command is running ({holder}); if none is, delete {}",
        path.display()
    )]
    Locked { path: PathBuf, holder: String },

    /// The request can't be carried out; the message says why.
    #[error("{0}")]
    Invalid(String),
}

/// Attaches the path an I/O operation was working on to its error.
pub(crate) trait IoResultExt<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.as_ref().to_path_buf(),
            source,
        })
    }
}
