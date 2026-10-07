//! Finding working-tree files: walking the tree in parallel while honoring
//! `.nexusignore` files at every level (spec §4), and looking up a single
//! stored path on disk. `.nexus/` directories are always skipped, and so are
//! symlinks (and junctions on Windows).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use ignore::{WalkBuilder, WalkState};

use crate::error::{Error, IoResultExt as _, Result};
use crate::path::{RepoPath, nfc};
use crate::repo::{DIR_NAME, IGNORE_FILE};

pub struct FoundFile {
    /// The path as stored: NFC, `/`-separated.
    pub path: RepoPath,
    /// The file's actual location. Its name can differ from `path`: the
    /// filesystem may report a decomposed (NFD) name that the stored path
    /// normalizes.
    pub fs_path: PathBuf,
    pub meta: Metadata,
}

#[derive(Default)]
pub struct Walked {
    /// Regular files that aren't ignored, sorted by path.
    pub files: Vec<FoundFile>,
    /// Things that were skipped and why: symlinks, unusable names, bad ignore rules.
    pub warnings: Vec<String>,
}

/// The scopes a walk covers, preprocessed so that checking an entry costs a
/// few hash lookups however many scopes there are.
struct Scopes {
    everything: bool,
    /// Each scope's path text.
    named: HashSet<String>,
    /// Every proper ancestor of a scope: directories the walk must enter to
    /// reach one.
    ancestors: HashSet<String>,
}

impl Scopes {
    fn new(scopes: &[RepoPath]) -> Self {
        let mut prepared = Self {
            everything: scopes.iter().any(RepoPath::is_root),
            named: HashSet::new(),
            ancestors: HashSet::new(),
        };
        for scope in scopes {
            let text = scope.as_str();
            prepared.named.insert(text.to_owned());
            for (end, _) in text.match_indices('/') {
                prepared.ancestors.insert(text[..end].to_owned());
            }
        }
        prepared
    }

    /// Whether `path` is a scope or inside one.
    fn covers(&self, path: &str) -> bool {
        self.everything
            || self.named.contains(path)
            || path
                .match_indices('/')
                .any(|(end, _)| self.named.contains(&path[..end]))
    }

    /// Whether the walk must visit `path`: it's covered, or it leads to a scope.
    fn relevant(&self, path: &str) -> bool {
        self.covers(path) || self.ancestors.contains(path)
    }
}

/// Walks the working tree under `root`, visiting only `scopes` (and the
/// directories leading to them).
pub fn walk(root: &Path, scopes: &[RepoPath]) -> Result<Walked> {
    let scopes = Arc::new(Scopes::new(scopes));
    let root_owned = root.to_path_buf();
    let filter_scopes = Arc::clone(&scopes);
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .add_custom_ignore_filename(IGNORE_FILE)
        .follow_links(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) && entry.file_name() == DIR_NAME {
                return false;
            }
            match relative_path(&root_owned, entry.path()) {
                Some(path) => filter_scopes.relevant(&path),
                // A name that isn't UTF-8 can't be inside a named scope. Keep it
                // when walking everything, so it gets reported and skipped.
                None => filter_scopes.everything,
            }
        });

    let found = Mutex::new(Walked::default());
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    builder.build_parallel().run(|| {
        Box::new(|result| {
            match result {
                Ok(entry) => {
                    if entry.depth() == 0 {
                        return WalkState::Continue;
                    }
                    let Some(file_type) = entry.file_type() else {
                        return WalkState::Continue;
                    };
                    if file_type.is_dir() {
                        return WalkState::Continue;
                    }
                    let shown = entry.path().display().to_string();
                    let Some(text) = relative_path(root, entry.path()) else {
                        push_warning(
                            &found,
                            format!("skipping {shown}: its name isn't valid UTF-8"),
                        );
                        return WalkState::Continue;
                    };
                    // The walk also visits the entries leading to a scope; only
                    // what's inside one counts (`add a/b` must not stage a file `a`).
                    if !scopes.covers(&text) {
                        return WalkState::Continue;
                    }
                    if file_type.is_symlink() {
                        push_warning(
                            &found,
                            format!("skipping {shown}: symlinks aren't supported"),
                        );
                        return WalkState::Continue;
                    }
                    if !file_type.is_file() {
                        return WalkState::Continue;
                    }
                    let path = match RepoPath::from_names(text.split('/')) {
                        Ok(path) => path,
                        Err(reason) => {
                            push_warning(&found, format!("skipping {shown}: {reason}"));
                            return WalkState::Continue;
                        }
                    };
                    match entry.metadata() {
                        Ok(meta) => lock(&found).files.push(FoundFile {
                            path,
                            fs_path: entry.into_path(),
                            meta,
                        }),
                        Err(err) => return fail(&failure, err, &shown),
                    }
                }
                Err(err) => {
                    if err.io_error().is_some() {
                        return fail(&failure, err, "the working tree");
                    }
                    // Unparsable ignore rules are reported, and the rest still apply.
                    push_warning(&found, format!("ignore rules: {err}"));
                }
            }
            WalkState::Continue
        })
    });

    if let Some(err) = failure
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Err(err);
    }
    let mut walked = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    walked.files.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| preference(a).cmp(&preference(b)))
    });
    resolve_twins(&mut walked);
    walked.warnings.sort();
    Ok(walked)
}

/// Orders files that normalize to the same path: the one whose on-disk name
/// is already NFC first, then by raw name, so the choice never depends on the
/// order the parallel walk happened to find them in.
fn preference(file: &FoundFile) -> (bool, OsString) {
    let raw = file
        .fs_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_default();
    let already_nfc = raw.to_str().is_some_and(|name| nfc(name) == name);
    (!already_nfc, raw)
}

/// Keeps one of each set of files whose names differ on disk but normalize to
/// the same stored path (possible on Linux and Windows), with a warning.
fn resolve_twins(walked: &mut Walked) {
    let mut kept: Vec<FoundFile> = Vec::with_capacity(walked.files.len());
    for file in walked.files.drain(..) {
        match kept.last() {
            Some(previous) if previous.path == file.path => walked.warnings.push(format!(
                "{} and {} both normalize to {}; staging only the first",
                previous.fs_path.display(),
                file.fs_path.display(),
                file.path
            )),
            _ => kept.push(file),
        }
    }
    walked.files = kept;
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn push_warning(walked: &Mutex<Walked>, warning: String) {
    lock(walked).warnings.push(warning);
}

/// Stops the walk on an error that would make the result incomplete. A
/// directory that couldn't be read must not look like its files were deleted.
fn fail(failure: &Mutex<Option<Error>>, err: impl std::fmt::Display, what: &str) -> WalkState {
    lock(failure).get_or_insert_with(|| Error::Invalid(format!("couldn't read {what}: {err}")));
    WalkState::Quit
}

/// `path` below `root` as `/`-separated NFC text, or `None` if a name isn't UTF-8.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let mut text = String::new();
    for component in path.strip_prefix(root).ok()?.components() {
        if !text.is_empty() {
            text.push('/');
        }
        text.push_str(&nfc(component.as_os_str().to_str()?));
    }
    Some(text)
}

/// What a stored path refers to on disk.
pub enum OnDisk {
    /// A regular file, reached through real directories.
    File {
        fs_path: PathBuf,
        meta: Metadata,
    },
    Dir,
    /// The path itself is a symlink (or junction), which isn't supported.
    Symlink,
    /// Something else, such as a device or socket.
    Other,
    /// Nothing is there. A directory on the way may be missing, be a file, or
    /// be a symlink. `case_match` is the path as it's spelled on disk when it
    /// exists with different letter case.
    Missing {
        case_match: Option<String>,
    },
}

/// What [`Lookup::find_ignoring_case`] found.
pub enum CaseFound {
    /// No directory listing has an entry for name number `missing` (from 0)
    /// of the path, so nothing listed is there. (Windows 8.3 short names
    /// aren't listed; check the filesystem itself for those.) `spelled` is
    /// how the directories before it are spelled on disk.
    Free {
        missing: usize,
        spelled: Vec<String>,
    },
    /// An entry answers, spelled as on disk: the whole path (`whole`), or a
    /// part of it that isn't a real directory although one is needed.
    Found {
        spelled: RepoPath,
        whole: bool,
        what: OnDisk,
    },
}

struct Listed {
    raw: OsString,
    /// The NFC name, if the raw name is valid UTF-8.
    name: Option<String>,
    file_type: FileType,
}

/// Looks up stored paths on disk the same way the walk finds files: every name
/// must match exactly after NFC normalization (so a case-only rename doesn't
/// count as the old name on a case-insensitive filesystem), and no directory on
/// the way may be a symlink or junction (so nothing outside the repository is
/// read through one). Directory listings are cached.
#[derive(Default)]
pub struct Lookup {
    listings: RefCell<HashMap<PathBuf, Option<Rc<Vec<Listed>>>>>,
}

impl Lookup {
    pub fn find(&self, root: &Path, path: &RepoPath) -> Result<OnDisk> {
        let names: Vec<&str> = path.components().collect();
        let mut dir = root.to_path_buf();
        for (i, name) in names.iter().enumerate() {
            let Some(listing) = self.listing(&dir)? else {
                return Ok(OnDisk::Missing { case_match: None });
            };
            let Some(entry) = best_match(&listing, |listed| listed == *name) else {
                let lower = name.to_lowercase();
                let case_match =
                    best_match(&listing, |listed| listed.to_lowercase() == lower).map(|entry| {
                        let mut spelled: Vec<&str> = names[..i].to_vec();
                        let found = entry.name.as_deref().unwrap_or(name);
                        spelled.push(found);
                        spelled.extend(&names[i + 1..]);
                        spelled.join("/")
                    });
                return Ok(OnDisk::Missing { case_match });
            };
            let fs_path = dir.join(&entry.raw);
            let last = i + 1 == names.len();
            if !last {
                if !entry.file_type.is_dir() {
                    return Ok(OnDisk::Missing { case_match: None });
                }
                dir = fs_path;
                continue;
            }
            let file_type = entry.file_type;
            return Ok(if file_type.is_symlink() {
                OnDisk::Symlink
            } else if file_type.is_dir() {
                OnDisk::Dir
            } else if file_type.is_file() {
                let meta = fs::symlink_metadata(&fs_path).at(&fs_path)?;
                OnDisk::File { fs_path, meta }
            } else {
                OnDisk::Other
            });
        }
        Ok(OnDisk::Dir)
    }

    /// What creating `path` would reach: each name matched exactly if
    /// possible, otherwise (with `ignore_case`, for a filesystem that ignores
    /// letter case) case-insensitively, through real directories. Used before
    /// creating a file, because on Windows and macOS writing `D/a.txt` lands
    /// in an existing `d/A.txt`.
    pub fn find_ignoring_case(
        &self,
        root: &Path,
        path: &RepoPath,
        ignore_case: bool,
    ) -> Result<CaseFound> {
        let names: Vec<&str> = path.components().collect();
        let mut dir = root.to_path_buf();
        let mut spelled: Vec<String> = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let Some(listing) = self.listing(&dir)? else {
                return Ok(CaseFound::Free {
                    missing: i,
                    spelled,
                });
            };
            let lower = name.to_lowercase();
            let entry = best_match(&listing, |listed| listed == *name).or_else(|| {
                ignore_case
                    .then(|| best_match(&listing, |listed| listed.to_lowercase() == lower))
                    .flatten()
            });
            let Some(entry) = entry else {
                return Ok(CaseFound::Free {
                    missing: i,
                    spelled,
                });
            };
            spelled.push(entry.name.clone().unwrap_or_else(|| (*name).to_owned()));
            let fs_path = dir.join(&entry.raw);
            let file_type = entry.file_type;
            if i + 1 < names.len() && file_type.is_dir() {
                dir = fs_path;
                continue;
            }
            let what = if file_type.is_symlink() {
                OnDisk::Symlink
            } else if file_type.is_dir() {
                OnDisk::Dir
            } else if file_type.is_file() {
                let meta = fs::symlink_metadata(&fs_path).at(&fs_path)?;
                OnDisk::File { fs_path, meta }
            } else {
                OnDisk::Other
            };
            return Ok(CaseFound::Found {
                spelled: RepoPath::from_names(spelled.iter().map(String::as_str))
                    .map_err(Error::Invalid)?,
                whole: i + 1 == names.len(),
                what,
            });
        }
        Ok(CaseFound::Free {
            missing: 0,
            spelled,
        })
    }

    fn listing(&self, dir: &Path) -> Result<Option<Rc<Vec<Listed>>>> {
        if let Some(cached) = self.listings.borrow().get(dir) {
            return Ok(cached.clone());
        }
        let listing = match fs::read_dir(dir) {
            Ok(entries) => {
                let mut listed = Vec::new();
                for entry in entries {
                    let entry = entry.at(dir)?;
                    let raw = entry.file_name();
                    listed.push(Listed {
                        name: raw.to_str().map(|name| nfc(name).into_owned()),
                        raw,
                        file_type: entry.file_type().at(dir)?,
                    });
                }
                Some(Rc::new(listed))
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                None
            }
            Err(err) => return Err(err).at(dir),
        };
        self.listings
            .borrow_mut()
            .insert(dir.to_path_buf(), listing.clone());
        Ok(listing)
    }
}

/// The listed entry whose NFC name satisfies `matches`, preferring one whose
/// raw name is already NFC, then the smallest raw name.
fn best_match(listing: &[Listed], matches: impl Fn(&str) -> bool) -> Option<&Listed> {
    listing
        .iter()
        .filter(|listed| listed.name.as_deref().is_some_and(&matches))
        .min_by(|a, b| {
            let rank = |listed: &Listed| {
                (
                    listed.raw.to_str() != listed.name.as_deref(),
                    listed.raw.clone(),
                )
            };
            rank(a).cmp(&rank(b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(walked: &Walked) -> Vec<&str> {
        walked.files.iter().map(|f| f.path.as_str()).collect()
    }

    fn write(root: &Path, file: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn honors_nested_ignore_files_and_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for file in [
            "a.txt",
            "debug.log",
            "src/main.rs",
            "src/gen/out.rs",
            "docs/guide.md",
            ".nexus/HEAD",
            "sub/.nexus/x",
        ] {
            write(root, file);
        }
        fs::write(root.join(".nexusignore"), "*.log\n").unwrap();
        fs::write(root.join("src/.nexusignore"), "gen/\n").unwrap();

        let all = walk(root, &[RepoPath::root()]).unwrap();
        assert_eq!(
            paths(&all),
            [
                ".nexusignore",
                "a.txt",
                "docs/guide.md",
                "src/.nexusignore",
                "src/main.rs"
            ]
        );

        let src = walk(root, &[RepoPath::parse("src").unwrap()]).unwrap();
        assert_eq!(paths(&src), ["src/.nexusignore", "src/main.rs"]);

        let one = walk(
            root,
            &[
                RepoPath::parse("docs/guide.md").unwrap(),
                RepoPath::parse("a.txt").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(paths(&one), ["a.txt", "docs/guide.md"]);
    }

    #[test]
    fn files_on_the_way_to_a_scope_are_not_included() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "thing");
        let walked = walk(dir.path(), &[RepoPath::parse("thing/inside.txt").unwrap()]).unwrap();
        assert_eq!(paths(&walked), Vec::<&str>::new());
    }

    #[test]
    fn lookup_requires_exact_names_through_real_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Docs/Guide.md");
        let lookup = Lookup::default();
        let find = |p: &str| {
            lookup
                .find(dir.path(), &RepoPath::parse(p).unwrap())
                .unwrap()
        };
        assert!(matches!(find("Docs/Guide.md"), OnDisk::File { .. }));
        assert!(matches!(find("Docs"), OnDisk::Dir));
        match find("docs/guide.md") {
            OnDisk::Missing { case_match } => {
                assert_eq!(case_match.as_deref(), Some("Docs/guide.md"));
            }
            _ => panic!("a different-case name must not count as the file"),
        }
        assert!(matches!(
            find("Docs/Guide.md/x"),
            OnDisk::Missing { case_match: None }
        ));
        assert!(matches!(
            find("nope/x"),
            OnDisk::Missing { case_match: None }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn lookup_and_walk_skip_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.txt");
        write(dir.path(), "real");
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked-dir")).unwrap();

        let walked = walk(dir.path(), &[RepoPath::root()]).unwrap();
        assert_eq!(paths(&walked), ["real"]);
        assert!(
            walked
                .warnings
                .iter()
                .any(|w| w.contains("symlinks aren't supported")),
            "{:?}",
            walked.warnings
        );

        let lookup = Lookup::default();
        let secret = RepoPath::parse("linked-dir/secret.txt").unwrap();
        assert!(matches!(
            lookup.find(dir.path(), &secret).unwrap(),
            OnDisk::Missing { .. }
        ));
        assert!(matches!(
            lookup
                .find(dir.path(), &RepoPath::parse("link").unwrap())
                .unwrap(),
            OnDisk::Symlink
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn nfc_twins_resolve_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("caf\u{e9}.txt"), "composed").unwrap();
        fs::write(dir.path().join("cafe\u{301}.txt"), "decomposed").unwrap();
        let walked = walk(dir.path(), &[RepoPath::root()]).unwrap();
        assert_eq!(paths(&walked), ["caf\u{e9}.txt"]);
        assert_eq!(fs::read(&walked.files[0].fs_path).unwrap(), b"composed");
        assert!(
            walked
                .warnings
                .iter()
                .any(|w| w.contains("both normalize to")),
            "{:?}",
            walked.warnings
        );
    }
}
