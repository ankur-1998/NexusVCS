//! Paths inside a repository, as the index and trees store them: relative to
//! the repository root, `/`-separated, and Unicode NFC (spec §4).

use std::borrow::{Borrow, Cow};
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use unicode_normalization::{IsNormalized, UnicodeNormalization as _, is_nfc_quick};

#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(String);

impl RepoPath {
    /// The repository root itself. It names a scope, never a file.
    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|name| !name.is_empty())
    }

    /// Builds a path from file names as the filesystem reports them,
    /// normalizing each to NFC and rejecting names a tree can't hold.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut path = String::new();
        for name in names {
            validate_name(name)?;
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&nfc(name));
        }
        Ok(Self(path))
    }

    /// This path with one more name appended, normalized and validated like
    /// [`Self::from_names`] but without re-checking the existing part.
    pub fn child(&self, name: &str) -> Result<Self, String> {
        validate_name(name)?;
        let mut path = String::with_capacity(self.0.len() + 1 + name.len());
        path.push_str(&self.0);
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&nfc(name));
        Ok(Self(path))
    }

    /// Parses a stored path, accepting only the canonical form: non-empty,
    /// valid names, already NFC.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("empty path".to_owned());
        }
        for name in text.split('/') {
            validate_name(name)?;
            if nfc(name) != name {
                return Err(format!("`{text}` is not in Unicode NFC form"));
            }
        }
        Ok(Self(text.to_owned()))
    }

    /// Whether `other` is this path or lies inside it. The root contains
    /// everything.
    pub fn contains(&self, other: &RepoPath) -> bool {
        self.is_root() || other.0 == self.0 || other.is_strictly_inside(self)
    }

    /// Whether this path is a proper ancestor of `other`.
    pub fn is_ancestor_of(&self, other: &RepoPath) -> bool {
        self.is_root() && !other.is_root() || other.is_strictly_inside(self)
    }

    fn is_strictly_inside(&self, ancestor: &RepoPath) -> bool {
        self.0.len() > ancestor.0.len()
            && self.0.starts_with(&ancestor.0)
            && self.0.as_bytes()[ancestor.0.len()] == b'/'
    }

    /// The same path inside `root` on this machine's filesystem, or `None`
    /// if a name can't be used safely here. On Windows a name like `..\x`,
    /// `C:x`, or `CON` (valid in a tree written on Linux) would escape the
    /// root, open an alternate data stream, or open a device. A component
    /// that would name the repository's own `.nexus` directory is never
    /// usable, so no tree can write into it.
    pub fn to_fs_path(&self, root: &Path) -> Option<PathBuf> {
        let mut path = root.to_path_buf();
        for name in self.components() {
            if is_repo_dir_name(name) || (cfg!(windows) && windows_name_problem(name).is_some()) {
                return None;
            }
            path.push(name);
        }
        Some(path)
    }
}

/// Lets a `BTreeMap<RepoPath, _>` be queried with a `&str`.
impl Borrow<str> for RepoPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Shown the way Git shows paths: quoted, with escapes, if the name has
/// control characters, `"` or `\` (see [`quote`]), so a file name can't
/// smuggle terminal escape sequences into the output.
impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str(".")
        } else {
            f.write_str(&quote(&self.0))
        }
    }
}

/// `text` as Git prints a name it has to quote: in double quotes, with C
/// escapes for control characters, `"` and `\`. Anything else, non-ASCII
/// included, is returned unchanged.
pub fn quote(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\')
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => {
                let mut bytes = [0; 4];
                for byte in c.encode_utf8(&mut bytes).bytes() {
                    let _ = write!(out, "\\{byte:03o}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    Cow::Owned(out)
}

/// Whether `name` would open the repository's `.nexus` directory: in any
/// letter case, with the trailing dots and spaces Windows ignores, or as its
/// Windows 8.3 short name (`NEXUS~1`).
pub fn is_repo_dir_name(name: &str) -> bool {
    let trimmed = name.trim_end_matches(['.', ' ']);
    if trimmed.eq_ignore_ascii_case(crate::repo::DIR_NAME) {
        return true;
    }
    cfg!(windows)
        && trimmed.len() > 6
        && trimmed[..6].eq_ignore_ascii_case("NEXUS~")
        && trimmed[6..].bytes().all(|b| b.is_ascii_digit())
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({:?})", self.0)
    }
}

/// Checks that `name` can be one entry of a tree (spec §4).
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("`{name}` is not a valid file name"));
    }
    if let Some(c) = name.chars().find(|c| matches!(c, '/' | '\n' | '\0')) {
        return Err(format!("file names can't contain {c:?}: {name:?}"));
    }
    Ok(())
}

/// `name` in Unicode NFC, borrowing when it already is.
pub fn nfc(name: &str) -> Cow<'_, str> {
    if is_nfc_quick(name.chars()) == IsNormalized::Yes {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(name.nfc().collect())
    }
}

/// Why `path` couldn't be checked out on Windows, if it couldn't.
pub fn windows_problem(path: &RepoPath) -> Option<String> {
    path.components().find_map(windows_name_problem)
}

/// Why one file name couldn't be used on Windows, if it couldn't.
pub fn windows_name_problem(name: &str) -> Option<String> {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    {
        if let Some(c) = name
            .chars()
            .find(|&c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') || c < ' ')
        {
            return Some(format!("`{name}` contains {c:?}"));
        }
        if name.ends_with(['.', ' ']) {
            return Some(format!("`{name}` ends with a dot or a space"));
        }
        let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
        let numbered_device = stem.len() == 4
            && stem
                .get(..3)
                .is_some_and(|p| p.eq_ignore_ascii_case("COM") || p.eq_ignore_ascii_case("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9');
        let reserved = numbered_device || RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r));
        if reserved {
            return Some(format!("`{name}` is a reserved device name"));
        }
    }
    None
}

/// Pairs of paths, or path prefixes, that differ only in letter case. On
/// case-insensitive filesystems (the Windows and macOS defaults) each pair
/// would be the same file or directory.
pub fn case_collisions<'a>(paths: impl IntoIterator<Item = &'a RepoPath>) -> Vec<(String, String)> {
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut collisions = Vec::new();
    for path in paths {
        let mut prefix = String::new();
        for name in path.components() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(name);
            let folded = prefix.to_lowercase();
            match seen.get(&folded) {
                Some(existing) if *existing != prefix => {
                    let pair = (existing.clone(), prefix.clone());
                    if !collisions.contains(&pair) {
                        collisions.push(pair);
                    }
                }
                Some(_) => {}
                None => {
                    seen.insert(folded, prefix.clone());
                }
            }
        }
    }
    collisions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RepoPath {
        RepoPath::parse(text).unwrap()
    }

    #[test]
    fn builds_and_normalizes_paths() {
        let decomposed = "cafe\u{301}.txt";
        let built = RepoPath::from_names(["docs", decomposed]).unwrap();
        assert_eq!(built.as_str(), "docs/caf\u{e9}.txt");
        assert!(RepoPath::parse(&format!("docs/{decomposed}")).is_err());
        assert_eq!(
            built.to_fs_path(Path::new("root")),
            Some(Path::new("root").join("docs").join("caf\u{e9}.txt"))
        );
    }

    #[test]
    fn rejects_names_a_tree_cannot_hold() {
        for bad in ["", ".", "..", "a\nb", "a\0b"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(RepoPath::parse("a//b").is_err());
        assert!(RepoPath::parse("/a").is_err());
        assert!(RepoPath::parse("a/").is_err());
        assert!(RepoPath::parse("").is_err());
    }

    #[test]
    fn containment() {
        let root = RepoPath::root();
        assert!(root.contains(&path("a/b")));
        assert!(path("a").contains(&path("a")));
        assert!(path("a").contains(&path("a/b")));
        assert!(!path("a").contains(&path("ab")));
        assert!(path("a").is_ancestor_of(&path("a/b/c")));
        assert!(!path("a").is_ancestor_of(&path("a")));
        assert!(root.is_ancestor_of(&path("a")));
    }

    #[test]
    fn flags_windows_problems() {
        for bad in [
            "a:b",
            "what?",
            "trailing.",
            "trailing ",
            "CON",
            "con.txt",
            "Lpt3.log",
            "back\\slash",
            "tab\tname",
        ] {
            assert!(windows_problem(&path(bad)).is_some(), "{bad}");
        }
        for fine in ["console", "COM10", "LPT0", "a.b.c", "src/main.rs"] {
            assert_eq!(windows_problem(&path(fine)), None, "{fine}");
        }
    }

    #[test]
    fn finds_case_collisions_in_files_and_directories() {
        let paths = [
            path("README.md"),
            path("readme.md"),
            path("Src/a.rs"),
            path("src/b.rs"),
            path("ok.txt"),
        ];
        assert_eq!(
            case_collisions(&paths),
            [
                ("README.md".to_owned(), "readme.md".to_owned()),
                ("Src".to_owned(), "src".to_owned())
            ]
        );
    }
}
