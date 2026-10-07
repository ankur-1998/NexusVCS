//! `config.toml` files: the repository's own (`.nexus/config.toml`) and the
//! global one, which it overrides. Edits keep the user's comments and layout.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use crate::error::{Error, IoResultExt as _, Result};
use crate::fsutil;

/// The settings `nexus config` can read and write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    UserName,
    UserEmail,
}

impl Key {
    pub fn name(self) -> &'static str {
        match self {
            Self::UserName => "user.name",
            Self::UserEmail => "user.email",
        }
    }

    fn table_and_field(self) -> (&'static str, &'static str) {
        match self {
            Self::UserName => ("user", "name"),
            Self::UserEmail => ("user", "email"),
        }
    }

    /// Checks a value before it's stored. Names and emails end up inside
    /// `Name <email>` in commits, so they can't contain the delimiters.
    pub fn validate(self, value: &str) -> Result<()> {
        let problem = match self {
            Self::UserName if value.trim().is_empty() => Some("can't be empty"),
            Self::UserName if value.contains(['<', '>', '\n', '\r']) => {
                Some("can't contain `<`, `>`, or line breaks")
            }
            Self::UserEmail if value.is_empty() => Some("can't be empty"),
            Self::UserEmail
                if value.contains(['<', '>']) || value.chars().any(char::is_whitespace) =>
            {
                Some("can't contain `<`, `>`, or whitespace")
            }
            _ => None,
        };
        match problem {
            Some(problem) => Err(Error::Invalid(format!("{} {problem}", self.name()))),
            None => Ok(()),
        }
    }
}

/// The config files that apply, in order of precedence.
#[derive(Clone, Debug, Default)]
pub struct ConfigFiles {
    pub repo: Option<PathBuf>,
    pub global: Option<PathBuf>,
}

impl ConfigFiles {
    /// The value of `key` from the first file that sets it, and that file.
    pub fn lookup(&self, key: Key) -> Result<Option<(String, PathBuf)>> {
        for path in self.repo.iter().chain(&self.global) {
            if let Some(value) = get(path, key)? {
                return Ok(Some((value, path.clone())));
            }
        }
        Ok(None)
    }

    /// The author name and email for new commits.
    pub fn identity(&self) -> Result<(String, String)> {
        let name = self.lookup(Key::UserName)?;
        let email = self.lookup(Key::UserEmail)?;
        match (name, email) {
            (Some((name, name_file)), Some((email, email_file))) => {
                // A hand-edited file could hold values that would make an
                // unreadable commit, so check them as `nexus config` would.
                Key::UserName
                    .validate(&name)
                    .map_err(|err| in_file(&name_file, &err))?;
                Key::UserEmail
                    .validate(&email)
                    .map_err(|err| in_file(&email_file, &err))?;
                Ok((name, email))
            }
            _ => Err(Error::Invalid(
                "NexusVCS doesn't know who you are yet. Set your name and email once with:\n  \
                 nexus config --global user.name \"Your Name\"\n  \
                 nexus config --global user.email you@example.com"
                    .to_owned(),
            )),
        }
    }
}

fn load(path: &Path) -> Result<DocumentMut> {
    let text = match fsutil::read_optional(path)? {
        Some(bytes) => {
            String::from_utf8(bytes).map_err(|_| corrupt(path, "isn't valid UTF-8".to_owned()))?
        }
        None => String::new(),
    };
    text.parse()
        .map_err(|err: toml_edit::TomlError| corrupt(path, format!("isn't valid TOML: {err}")))
}

fn in_file(path: &Path, err: &Error) -> Error {
    Error::Invalid(format!("{err} (set in {})", path.display()))
}

fn corrupt(path: &Path, reason: String) -> Error {
    Error::Corrupt {
        path: path.to_path_buf(),
        reason,
    }
}

/// The value of `key` in one file, if that file sets it.
pub fn get(path: &Path, key: Key) -> Result<Option<String>> {
    let (table, field) = key.table_and_field();
    let doc = load(path)?;
    let Some(item) = lookup_item(&doc, table, field) else {
        return Ok(None);
    };
    item.as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| corrupt(path, format!("{} must be a string", key.name())))
}

/// Whether `[core] filemode` allows trusting the filesystem's executable
/// bit. `nexus init` sets it to false on filesystems where the bit is
/// meaningless; it defaults to true.
pub fn file_mode(path: &Path) -> Result<bool> {
    let doc = load(path)?;
    match lookup_item(&doc, "core", "filemode") {
        None => Ok(true),
        Some(item) => item
            .as_bool()
            .ok_or_else(|| corrupt(path, "core.filemode must be true or false".to_owned())),
    }
}

/// Sets `[core] filemode`.
pub fn set_file_mode(path: &Path, trusted: bool) -> Result<()> {
    set_item(path, "core", "filemode", toml_edit::value(trusted))
}

fn lookup_item<'a>(doc: &'a DocumentMut, table: &str, field: &str) -> Option<&'a toml_edit::Item> {
    doc.get(table)?.as_table_like()?.get(field)
}

/// Sets `key` in one file, creating the file and its directory if needed.
pub fn set(path: &Path, key: Key, value: &str) -> Result<()> {
    key.validate(value)?;
    let (table, field) = key.table_and_field();
    set_item(path, table, field, toml_edit::value(value))
}

fn set_item(path: &Path, table: &str, field: &str, value: toml_edit::Item) -> Result<()> {
    let mut doc = load(path)?;
    if let Some(existing) = doc.get(table)
        && existing.as_table_like().is_none()
    {
        return Err(corrupt(path, format!("`{table}` must be a table")));
    }
    if doc.get(table).is_none() {
        // Comments at the end of the file belong above the new table, not below it.
        let trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
        doc.set_trailing("");
        let mut new_table = toml_edit::Table::new();
        new_table.decor_mut().set_prefix(trailing);
        doc[table] = toml_edit::Item::Table(new_table);
    }
    // Works for `[user]` tables and inline `user = { ... }` tables alike.
    let table_like = doc[table].as_table_like_mut().expect("checked above");
    table_like.insert(field, value);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).at(dir)?;
    }
    fsutil::write_file(path, doc.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_keeps_comments_and_repo_overrides_global() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global").join("config.toml");
        let repo = dir.path().join("repo.toml");
        fs::write(&repo, "# my notes\n[ai]\nmodel = \"local\"\n").unwrap();

        set(&global, Key::UserName, "Global Name").unwrap();
        set(&global, Key::UserEmail, "global@example.com").unwrap();
        set(&repo, Key::UserName, "Repo Name").unwrap();

        let text = fs::read_to_string(&repo).unwrap();
        assert!(
            text.starts_with("# my notes\n[ai]\nmodel = \"local\"\n"),
            "{text}"
        );
        assert!(text.contains("[user]\nname = \"Repo Name\""), "{text}");

        let files = ConfigFiles {
            repo: Some(repo.clone()),
            global: Some(global),
        };
        assert_eq!(
            files.identity().unwrap(),
            ("Repo Name".to_owned(), "global@example.com".to_owned())
        );
        assert_eq!(files.lookup(Key::UserName).unwrap().unwrap().1, repo);
    }

    #[test]
    fn identity_is_required_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let files = ConfigFiles {
            repo: Some(dir.path().join("none.toml")),
            global: None,
        };
        assert!(
            files
                .identity()
                .unwrap_err()
                .to_string()
                .contains("nexus config --global user.name")
        );

        let path = dir.path().join("c.toml");
        assert!(set(&path, Key::UserName, "A <B>").is_err());
        assert!(set(&path, Key::UserEmail, "a b@example.com").is_err());
        assert!(set(&path, Key::UserName, "  ").is_err());

        // A hand-edited value that `nexus config` would refuse is caught when used.
        let edited = dir.path().join("edited.toml");
        fs::write(
            &edited,
            "[user]\nname = \"Ada\\nLovelace\"\nemail = \"ada@example.com\"\n",
        )
        .unwrap();
        let files = ConfigFiles {
            repo: Some(edited),
            global: None,
        };
        let err = files.identity().unwrap_err().to_string();
        assert!(
            err.contains("user.name can't contain") && err.contains("edited.toml"),
            "{err}"
        );
    }

    #[test]
    fn set_works_with_inline_tables_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        fs::write(
            &path,
            "user = { name = \"Inline\", email = \"inline@example.com\" }\n",
        )
        .unwrap();
        set(&path, Key::UserName, "New Name").unwrap();
        assert_eq!(
            get(&path, Key::UserName).unwrap().as_deref(),
            Some("New Name")
        );
        assert_eq!(
            get(&path, Key::UserEmail).unwrap().as_deref(),
            Some("inline@example.com")
        );

        fs::write(&path, "user = 5\n").unwrap();
        assert!(set(&path, Key::UserName, "x").is_err());
    }

    #[test]
    fn file_mode_defaults_to_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        assert!(file_mode(&path).unwrap());
        set_file_mode(&path, false).unwrap();
        assert!(!file_mode(&path).unwrap());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("[core]\nfilemode = false")
        );
    }
}
