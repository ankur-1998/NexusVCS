//! Finding, creating, and opening repositories (spec §4).

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, IoResultExt as _, Result};
use crate::fsutil;
use crate::index::Index;
use crate::odb::ObjectStore;
use crate::path::RepoPath;
use crate::refs::{DEFAULT_BRANCH, Head, RefStore};

/// The repository's metadata directory, inside the working tree's root.
pub const DIR_NAME: &str = ".nexus";
/// Per-directory ignore rules, in gitignore syntax.
pub const IGNORE_FILE: &str = ".nexusignore";

/// What `nexus init` writes to a new repository's `.nexusignore`.
pub const DEFAULT_IGNORE: &str = "\
# Paths NexusVCS doesn't track, in .gitignore syntax.
.git/
node_modules/
target/
dist/
build/
.env
*.log
";

const CONFIG_TEMPLATE: &str = "\
# This repository's settings. They override the global config.
";

#[derive(Clone, Debug)]
pub struct Repo {
    root: PathBuf,
    dir: PathBuf,
    odb: ObjectStore,
    refs: RefStore,
}

impl Repo {
    /// The repository containing `start`: the nearest directory, `start` or
    /// one of its parents, that has a `.nexus` directory with a `HEAD`.
    pub fn discover(start: &Path) -> Result<Self> {
        Self::find_root(start)
            .map(|root| Self::open(&root))
            .ok_or_else(|| Error::NotARepository(start.to_path_buf()))
    }

    /// The root of the repository containing `start`, if there is one.
    pub fn find_root(start: &Path) -> Option<PathBuf> {
        start
            .ancestors()
            .find(|dir| dir.join(DIR_NAME).join("HEAD").is_file())
            .map(Path::to_path_buf)
    }

    fn open(root: &Path) -> Self {
        let dir = root.join(DIR_NAME);
        Self {
            root: root.to_path_buf(),
            odb: ObjectStore::new(&dir.join("objects")),
            refs: RefStore::new(&dir),
            dir,
        }
    }

    /// Creates the `.nexus` layout in `root`, with `HEAD` on the unborn
    /// `main` branch. Recording the first operation is up to the caller.
    pub fn create(root: &Path) -> Result<Self> {
        let repo = Self::open(root);
        for dir in [
            repo.dir.join("objects"),
            repo.dir.join("refs").join("heads"),
        ] {
            fs::create_dir_all(&dir).at(&dir)?;
        }
        fsutil::write_file(&repo.config_path(), CONFIG_TEMPLATE.as_bytes())?;
        repo.refs
            .set_head(&Head::Branch(DEFAULT_BRANCH.to_owned()))?;
        Ok(repo)
    }

    /// The working tree's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `.nexus` directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn odb(&self) -> &ObjectStore {
        &self.odb
    }

    pub fn refs(&self) -> &RefStore {
        &self.refs
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join("index")
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("lock")
    }

    pub fn oplog_path(&self) -> PathBuf {
        self.dir.join("OPLOG")
    }

    pub fn stash_path(&self) -> PathBuf {
        self.dir.join("stash")
    }

    pub fn load_index(&self) -> Result<Index> {
        Index::load(&self.index_path())
    }

    /// Converts an absolute path inside the working tree to a [`RepoPath`].
    pub fn repo_path(&self, path: &Path) -> Result<RepoPath> {
        let relative = match path.strip_prefix(&self.root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => self.relative_via_canonical(path).ok_or_else(|| {
                Error::Invalid(format!(
                    "{} is outside the repository at {}",
                    path.display(),
                    self.root.display()
                ))
            })?,
        };
        let mut names = Vec::new();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(Error::Invalid(format!(
                    "unexpected path component in {}",
                    path.display()
                )));
            };
            let name = name
                .to_str()
                .ok_or_else(|| Error::Invalid(format!("{} isn't valid UTF-8", path.display())))?;
            names.push(name);
        }
        if names.first().is_some_and(|first| *first == DIR_NAME) {
            return Err(Error::Invalid(format!(
                "{} is inside the repository's own {DIR_NAME} directory",
                path.display()
            )));
        }
        RepoPath::from_names(names).map_err(Error::Invalid)
    }

    /// The part of `path` below the root, for a path that reaches the
    /// repository by another route than its textual root: a symlinked
    /// directory (macOS's `/tmp` is one), a junction, different letter case on
    /// Windows, or a `\\?\` prefix. The deepest existing part of `path` is
    /// canonicalized and the rest re-attached, so not-yet-existing paths work.
    fn relative_via_canonical(&self, path: &Path) -> Option<PathBuf> {
        let root = fs::canonicalize(&self.root).ok()?;
        let mut existing = path.to_path_buf();
        let mut rest = Vec::new();
        loop {
            if let Ok(mut full) = fs::canonicalize(&existing) {
                full.extend(rest.iter().rev());
                return full.strip_prefix(&root).ok().map(Path::to_path_buf);
            }
            rest.push(existing.file_name()?.to_os_string());
            if !existing.pop() {
                return None;
            }
        }
    }
}

/// `path` made absolute against `base` with `.` and `..` resolved textually,
/// without touching the filesystem (so symlinks aren't followed).
pub fn absolutize(base: &Path, path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_resolves_dots_textually() {
        let base = if cfg!(windows) {
            Path::new(r"C:\work\repo")
        } else {
            Path::new("/work/repo")
        };
        assert_eq!(
            absolutize(base, Path::new("src/../a.txt")),
            base.join("a.txt")
        );
        assert_eq!(
            absolutize(base, Path::new("./src/./b")),
            base.join("src").join("b")
        );
        assert_eq!(absolutize(base, Path::new("..")), base.parent().unwrap());
    }

    #[test]
    fn discovers_from_subdirectories_and_maps_paths() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::create(dir.path()).unwrap();
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(Repo::discover(&nested).unwrap().root(), dir.path());
        assert_eq!(
            repo.repo_path(&nested.join("c.txt")).unwrap().as_str(),
            "a/b/c.txt"
        );
        assert!(
            repo.repo_path(&dir.path().join(DIR_NAME).join("HEAD"))
                .is_err()
        );
        assert!(repo.repo_path(dir.path().parent().unwrap()).is_err());
        assert!(repo.repo_path(dir.path()).unwrap().is_root());
    }
}
