//! Argument definitions, shared by the CLI and the web terminal so both parse
//! commands identically (spec §5.2).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config;

/// NexusVCS: a local-first version control system.
#[derive(Debug, Parser)]
#[command(
    name = "nexus",
    bin_name = "nexus",
    version,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a repository in the current directory, or in DIR
    Init { dir: Option<PathBuf> },

    /// Stage files: record their current content in the index for the next commit
    #[command(
        after_help = "Directories are staged recursively, skipping paths matched by .nexusignore. \
                            A path that no longer exists is removed from the index."
    )]
    Add {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Mark the files executable
        #[arg(long)]
        exec: bool,
    },

    /// Record the staged files as a new commit on the current branch
    Commit {
        /// The commit message. Repeat -m to add paragraphs.
        #[arg(
            short,
            long = "message",
            value_name = "MESSAGE",
            required = true,
            allow_hyphen_values = true
        )]
        message: Vec<String>,
        /// Commit even if nothing changed since the parent commit
        #[arg(long)]
        allow_empty: bool,
    },

    /// Show the current branch's history, newest first
    Log {
        /// Show at most N commits
        #[arg(short = 'n', long = "max-count", value_name = "N")]
        max_count: Option<usize>,
        /// One line per commit
        #[arg(long)]
        oneline: bool,
    },

    /// Read or set a configuration value
    Config {
        /// Use the global config instead of this repository's
        #[arg(long)]
        global: bool,
        key: ConfigKey,
        /// The new value. Without one, prints the current value.
        value: Option<String>,
    },

    /// Show staged changes, unstaged changes, and untracked files
    Status,

    /// Show changes: working tree against the index, or as the options say
    #[command(after_help = "With no commits: the working tree against the index.\n\
                            With --staged: the index against HEAD (or COMMIT).\n\
                            With one commit: the working tree against it.\n\
                            With two commits: one against the other.\n\
                            Put paths after `--` to limit the diff to them.")]
    Diff {
        /// Compare the index instead of the working tree
        #[arg(long, alias = "cached")]
        staged: bool,
        #[command(flatten)]
        options: DiffOptions,
        /// Zero, one, or two commits (or paths, when they aren't commits)
        #[arg(value_name = "COMMIT")]
        revs: Vec<String>,
        /// Only these paths
        #[arg(last = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },

    /// Show a commit and its changes against its first parent
    Show {
        #[arg(default_value = "HEAD")]
        commit: String,
        #[command(flatten)]
        options: DiffOptions,
    },

    /// List, create, or delete branches
    #[command(
        after_help = "With no arguments, lists branches and marks the current one.\n\
                            `nexus branch NAME [START]` creates a branch at START (default HEAD)."
    )]
    Branch {
        name: Option<String>,
        start: Option<String>,
        /// Delete the branch, if it's merged into the current one
        #[arg(short = 'd', long = "delete", conflicts_with = "force_delete")]
        delete: bool,
        /// Delete the branch even if it isn't merged
        #[arg(short = 'D')]
        force_delete: bool,
    },

    /// List, create, or delete tags
    Tag {
        name: Option<String>,
        /// The commit to tag (default HEAD)
        commit: Option<String>,
        /// Delete the tag
        #[arg(short = 'd', long = "delete")]
        delete: bool,
    },

    /// Switch to a branch, or to a commit (detached HEAD)
    Checkout { target: String },

    /// Discard changes to files, or unstage them with --staged
    #[command(
        after_help = "Without --staged, overwrites the named working files with their staged \
                            version (or COMMIT's, with --source). With --staged, resets their index \
                            entries to HEAD (or COMMIT) and leaves the files alone."
    )]
    Restore {
        /// Reset the index entries instead of the working files
        #[arg(long)]
        staged: bool,
        /// Where to restore from
        #[arg(short = 's', long, value_name = "COMMIT")]
        source: Option<String>,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },

    /// Stop tracking files, deleting them unless --cached is given
    Rm {
        /// Keep the files on disk
        #[arg(long)]
        cached: bool,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },

    /// Write a commit's files into an empty directory
    Export { commit: String, dir: PathBuf },

    /// Print the ID a file would get as an object, without storing it
    HashObject { file: PathBuf },

    /// Print an object's content
    CatFile {
        /// Pretty-print the object
        #[arg(short = 'p', required = true)]
        pretty: bool,
        /// The object's ID, or a unique prefix of at least 4 characters
        id: String,
    },

    /// Inspect internal data structures
    Debug {
        #[command(subcommand)]
        what: DebugCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    /// Print every index entry: kind, ID, size, modification time, path
    Index,
}

#[derive(Clone, Copy, Debug, clap::Args)]
pub struct DiffOptions {
    /// The line-diff algorithm
    #[arg(long, value_enum, default_value_t = DiffAlgorithm::Histogram)]
    pub diff_algorithm: DiffAlgorithm,
    /// Lines of context around each change
    #[arg(short = 'U', long = "unified", value_name = "N", default_value_t = crate::diff::DEFAULT_CONTEXT)]
    pub context: u32,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum DiffAlgorithm {
    Histogram,
    Myers,
}

impl From<DiffAlgorithm> for crate::diff::Algorithm {
    fn from(algorithm: DiffAlgorithm) -> Self {
        match algorithm {
            DiffAlgorithm::Histogram => Self::Histogram,
            DiffAlgorithm::Myers => Self::Myers,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ConfigKey {
    #[value(name = "user.name")]
    UserName,
    #[value(name = "user.email")]
    UserEmail,
}

impl From<ConfigKey> for config::Key {
    fn from(key: ConfigKey) -> Self {
        match key {
            ConfigKey::UserName => Self::UserName,
            ConfigKey::UserEmail => Self::UserEmail,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::*;

    #[test]
    fn definitions_are_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_is_recognised() {
        let err = Cli::try_parse_from(["nexus", "--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn no_arguments_asks_for_help() {
        let err = Cli::try_parse_from(["nexus"]).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn parses_phase_one_commands() {
        let cli =
            Cli::try_parse_from(["nexus", "commit", "-m", "one", "-m", "two", "--allow-empty"])
                .unwrap();
        assert!(
            matches!(cli.command, Command::Commit { ref message, allow_empty: true } if message == &["one", "two"])
        );
        let cli =
            Cli::try_parse_from(["nexus", "config", "--global", "user.email", "a@b.c"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Config {
                global: true,
                key: ConfigKey::UserEmail,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["nexus", "cat-file", "abc"]).is_err());
        assert!(Cli::try_parse_from(["nexus", "add"]).is_err());
    }
}
