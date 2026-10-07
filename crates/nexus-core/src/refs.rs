//! `HEAD` and refs (spec §4): one small text file each under `.nexus/`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, IoResultExt as _, Result};
use crate::fsutil;
use crate::hash::ObjectId;
use crate::path;

/// The branch a new repository starts on.
pub const DEFAULT_BRANCH: &str = "refs/heads/main";

pub const BRANCH_PREFIX: &str = "refs/heads/";
pub const TAG_PREFIX: &str = "refs/tags/";

/// What `HEAD` points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// A branch, by full ref name such as `refs/heads/main`. The branch may
    /// not exist yet ("unborn") before its first commit.
    Branch(String),
    Detached(ObjectId),
}

impl Head {
    /// The form stored in `HEAD` and in op objects, without a newline.
    pub fn encode(&self) -> String {
        match self {
            Self::Branch(name) => format!("ref: {name}"),
            Self::Detached(id) => id.to_string(),
        }
    }

    pub fn decode(text: &str) -> Option<Self> {
        match text.strip_prefix("ref: ") {
            Some(name) => is_valid_ref_name(name).then(|| Self::Branch(name.to_owned())),
            None => ObjectId::parse_hex(text).map(Self::Detached),
        }
    }

    /// The short branch name, such as `main`, unless HEAD is detached.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Self::Branch(name) => Some(name.strip_prefix(BRANCH_PREFIX).unwrap_or(name)),
            Self::Detached(_) => None,
        }
    }
}

/// Whether `name` is a full ref name this repository can store: `refs/`
/// followed by `/`-separated components that are plain file names on every
/// OS. Windows rules apply everywhere (no device names like `CON`, no
/// trailing dot, none of `< > " |`), so a repository made on Linux works on
/// Windows, and Windows can't quietly map one name onto another file.
pub fn is_valid_ref_name(name: &str) -> bool {
    name.strip_prefix("refs/").is_some_and(|rest| {
        !rest.is_empty()
            && rest.split('/').all(|part| {
                !part.is_empty()
                    && !part.starts_with('.')
                    && part.chars().all(|c| {
                        !c.is_control()
                            && !matches!(c, ' ' | '\\' | ':' | '?' | '*' | '[' | '~' | '^')
                    })
                    && path::windows_name_problem(part).is_none()
                    && path::nfc(part) == part
            })
    })
}

/// Checks a branch or tag name given by the user and returns the full ref name
/// (`prefix` is [`BRANCH_PREFIX`] or [`TAG_PREFIX`]). The rules follow Git's,
/// so names that would be confusing on a command line or as a file are refused.
pub fn full_ref_name(prefix: &str, name: &str, what: &str) -> Result<String> {
    // Stored NFC, like paths, so a name typed on macOS (often NFD) matches.
    let normalized = path::nfc(name);
    let name: &str = &normalized;
    let problem = if name.is_empty() {
        Some("can't be empty")
    } else if name == "HEAD" {
        Some("can't be HEAD")
    } else if name.starts_with('-') {
        Some("can't start with '-'")
    } else if name.contains("..") || name.contains("@{") || name.contains("//") {
        Some("can't contain '..', '@{', or '//'")
    } else if name.ends_with('/')
        || name.ends_with('.')
        || name.to_ascii_lowercase().ends_with(".lock")
    {
        Some("can't end with '/', '.', or '.lock'")
    } else if name.len() == ObjectId::HEX_LEN && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some("can't be a full object ID")
    } else if !is_valid_ref_name(&format!("{prefix}{name}")) {
        Some(
            "can't contain spaces, control characters, or any of \\ : ? * [ ~ ^ < > \" |, no part can \
             start with '.' or be a Windows device name such as CON or NUL",
        )
    } else {
        None
    };
    match problem {
        Some(problem) => Err(Error::Invalid(format!("{what} name `{name}` {problem}"))),
        None => Ok(format!("{prefix}{name}")),
    }
}

/// Reads and writes `HEAD` and the files under `.nexus/refs/`.
#[derive(Clone, Debug)]
pub struct RefStore {
    /// The `.nexus` directory.
    dir: PathBuf,
}

impl RefStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn head_path(&self) -> PathBuf {
        self.dir.join("HEAD")
    }

    fn ref_path(&self, name: &str) -> PathBuf {
        let mut path = self.dir.clone();
        path.extend(name.split('/'));
        path
    }

    pub fn head(&self) -> Result<Head> {
        let path = self.head_path();
        let text = fs::read_to_string(&path).at(&path)?;
        strip_line_end(&text)
            .and_then(Head::decode)
            .ok_or_else(|| Error::Corrupt {
                path,
                reason: "HEAD must hold `ref: <name>` or a commit hash".to_owned(),
            })
    }

    pub fn set_head(&self, head: &Head) -> Result<()> {
        fsutil::write_file(&self.head_path(), format!("{}\n", head.encode()).as_bytes())
    }

    /// The commit a ref points at, or `None` if the ref doesn't exist.
    ///
    /// Every component must exist on disk spelled exactly as given (after
    /// NFC normalization). Otherwise a case-insensitive filesystem would open
    /// `main` for `MAIN`, and a lookup could succeed under a name that
    /// `nexus branch` doesn't list.
    pub fn get(&self, name: &str) -> Result<Option<ObjectId>> {
        if !is_valid_ref_name(name) {
            return Ok(None);
        }
        let mut path = self.dir.clone();
        for part in name.split('/') {
            let entries = match fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(None);
                }
                Err(err) => return Err(err).at(&path),
            };
            let mut found = None;
            for entry in entries {
                let entry = entry.at(&path)?;
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|on_disk| path::nfc(on_disk) == part)
                {
                    found = Some(entry.file_name());
                    break;
                }
            }
            let Some(on_disk) = found else {
                return Ok(None);
            };
            path.push(on_disk);
        }
        if path.is_dir() {
            // `refs/heads/a` when only `refs/heads/a/b` exists.
            return Ok(None);
        }
        Self::read_ref(path)
    }

    fn read_ref(path: PathBuf) -> Result<Option<ObjectId>> {
        match fsutil::read_optional(&path)? {
            None => Ok(None),
            Some(bytes) => parse_ref(&bytes).map(Some).ok_or_else(|| Error::Corrupt {
                path,
                reason: "a ref must hold one commit hash".to_owned(),
            }),
        }
    }

    pub fn set(&self, name: &str, id: ObjectId) -> Result<()> {
        debug_assert!(is_valid_ref_name(name), "{name}");
        let path = self.ref_path(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        fsutil::write_file(&path, format!("{id}\n").as_bytes())
    }

    /// Deletes a ref, then any directories under `refs/` it leaves empty.
    pub fn delete(&self, name: &str) -> Result<()> {
        let path = self.ref_path(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).at(&path),
        }
        let refs_root = self.dir.join("refs");
        let mut dir = path.parent();
        while let Some(current) = dir {
            if current == refs_root
                || !current.starts_with(&refs_root)
                || fs::remove_dir(current).is_err()
            {
                break;
            }
            dir = current.parent();
        }
        Ok(())
    }

    /// Why `name` can't be created: an existing ref is a prefix of it (`a`,
    /// when creating `a/b`), lies under it (`a/b`, when creating `a`), or
    /// differs from it only in letter case. Case is ignored on every OS,
    /// because on Windows and macOS such names are the same file.
    pub fn conflict(&self, name: &str) -> Result<Option<String>> {
        let refs = self.list()?;
        let new: Vec<&str> = name.split('/').collect();
        Ok(refs
            .keys()
            .find(|existing| {
                let old: Vec<&str> = existing.split('/').collect();
                // Walk the components the two names share, ignoring case.
                for (i, (a, b)) in old.iter().zip(&new).enumerate() {
                    if a.to_lowercase() != b.to_lowercase() {
                        return false;
                    }
                    let last = i + 1 == old.len() || i + 1 == new.len();
                    // Spelled differently (`Feature/y` and `feature/x` share
                    // one directory on Windows), or one ends where the other
                    // needs a directory.
                    if a != b || last {
                        return true;
                    }
                }
                false
            })
            .cloned())
    }

    /// The commit HEAD resolves to, or `None` on an unborn branch.
    pub fn resolve(&self, head: &Head) -> Result<Option<ObjectId>> {
        match head {
            Head::Branch(name) => self.get(name),
            Head::Detached(id) => Ok(Some(*id)),
        }
    }

    /// Every ref, by full name.
    pub fn list(&self) -> Result<BTreeMap<String, ObjectId>> {
        let mut refs = BTreeMap::new();
        Self::collect(&self.dir.join("refs"), "refs", &mut refs)?;
        Ok(refs)
    }

    fn collect(dir: &Path, prefix: &str, refs: &mut BTreeMap<String, ObjectId>) -> Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).at(dir),
        };
        for entry in entries {
            let entry = entry.at(dir)?;
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(Error::Corrupt {
                    path,
                    reason: "ref names must be valid UTF-8".to_owned(),
                });
            };
            if name.starts_with('.') {
                // Temp files from interrupted writes.
                continue;
            }
            let full = format!("{prefix}/{}", path::nfc(&name));
            if entry.file_type().at(&path)?.is_dir() {
                Self::collect(&path, &full, refs)?;
            } else if !is_valid_ref_name(&full) {
                // Not something nexus could have written, such as an editor's
                // backup file (`main~`) or a copy (`main - Copy`). Ignore it.
            } else if let Some(id) = Self::read_ref(path)? {
                refs.insert(full, id);
            }
        }
        Ok(())
    }
}

fn parse_ref(bytes: &[u8]) -> Option<ObjectId> {
    let text = std::str::from_utf8(bytes).ok()?;
    ObjectId::parse_hex(strip_line_end(text)?)
}

/// One line's content without its terminator. Files nexus writes end in
/// `\n`; hand edits on Windows often end in `\r\n`, which is accepted too.
pub(crate) fn strip_line_end(text: &str) -> Option<&str> {
    let line = text.strip_suffix('\n')?;
    let line = line.strip_suffix('\r').unwrap_or(line);
    (!line.contains(['\n', '\r'])).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_round_trip() {
        let branch = Head::Branch(DEFAULT_BRANCH.to_owned());
        assert_eq!(branch.encode(), "ref: refs/heads/main");
        assert_eq!(Head::decode("ref: refs/heads/main"), Some(branch.clone()));
        assert_eq!(branch.branch(), Some("main"));

        let id = ObjectId::from_bytes([7; 32]);
        assert_eq!(Head::decode(&id.to_string()), Some(Head::Detached(id)));
        assert_eq!(Head::decode("ref: heads/main"), None);
        assert_eq!(Head::decode("ref: refs/heads/"), None);
    }

    #[test]
    fn ref_names() {
        for good in [
            "refs/heads/main",
            "refs/heads/feature/login",
            "refs/meta/issues",
        ] {
            assert!(is_valid_ref_name(good), "{good}");
        }
        for bad in [
            "heads/main",
            "refs/",
            "refs//x",
            "refs/heads/.hidden",
            "refs/heads/a b",
            "refs/heads/a\\b",
        ] {
            assert!(!is_valid_ref_name(bad), "{bad}");
        }
    }

    #[test]
    fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::new(dir.path());
        let id = ObjectId::from_bytes([1; 32]);
        assert_eq!(refs.get("refs/heads/main").unwrap(), None);
        refs.set("refs/heads/main", id).unwrap();
        refs.set("refs/heads/feature/x", id).unwrap();
        assert_eq!(refs.get("refs/heads/main").unwrap(), Some(id));
        let names: Vec<_> = refs.list().unwrap().into_keys().collect();
        assert_eq!(names, ["refs/heads/feature/x", "refs/heads/main"]);
    }

    #[test]
    fn tolerates_hand_edits() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::new(dir.path());
        let id = ObjectId::from_bytes([2; 32]);
        let heads = dir.path().join("refs").join("heads");
        fs::create_dir_all(&heads).unwrap();
        // Saved by a Windows editor: CRLF.
        fs::write(heads.join("main"), format!("{id}\r\n")).unwrap();
        // Backup and copy files that aren't valid ref names are ignored.
        fs::write(heads.join("main~"), format!("{id}\n")).unwrap();
        fs::write(heads.join("main - Copy"), format!("{id}\n")).unwrap();
        assert_eq!(refs.get("refs/heads/main").unwrap(), Some(id));
        let names: Vec<_> = refs.list().unwrap().into_keys().collect();
        assert_eq!(names, ["refs/heads/main"]);
        fs::write(dir.path().join("HEAD"), "ref: refs/heads/main\r\n").unwrap();
        assert_eq!(
            refs.head().unwrap(),
            Head::Branch(DEFAULT_BRANCH.to_owned())
        );
        // Anything more than one line is still rejected.
        fs::write(heads.join("main"), format!("{id}\n{id}\n")).unwrap();
        assert!(refs.get("refs/heads/main").is_err());
    }
}
