//! Helpers shared by the integration tests: a throwaway repository with a
//! fixed clock and identity, driven through the same command layer the CLI
//! uses.

#![allow(dead_code, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use nexus_core::commands::{self, CommandResult, Ctx};
use nexus_core::content;
use nexus_core::hash::ObjectId;
use nexus_core::object::{Commit, ObjectKind, Op};
use nexus_core::oplog;
use nexus_core::repo::Repo;
use nexus_core::time::{Clock, Timestamp};
use nexus_core::tree;
use tempfile::TempDir;

pub const TIME: &str = "1759737600 +0530";

pub struct TestRepo {
    _dir: TempDir,
    pub root: PathBuf,
    pub ctx: Ctx,
}

impl TestRepo {
    /// An initialized repository whose global config sets an identity.
    pub fn new() -> Self {
        let repo = Self::uninitialized();
        repo.ok(&["init"]);
        repo
    }

    /// A working directory with a global config but no repository yet.
    pub fn uninitialized() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let global = dir.path().join("global.toml");
        fs::write(
            &global,
            "[user]\nname = \"Ada Lovelace\"\nemail = \"ada@example.com\"\n",
        )
        .unwrap();
        let ctx = Ctx {
            cwd: root.clone(),
            global_config: Some(global),
            clock: Clock::Fixed(Timestamp::parse(TIME).unwrap()),
        };
        Self {
            _dir: dir,
            root,
            ctx,
        }
    }

    pub fn run(&self, args: &[&str]) -> CommandResult {
        self.run_in(&self.root, args)
    }

    pub fn run_in(&self, cwd: &Path, args: &[&str]) -> CommandResult {
        let ctx = Ctx {
            cwd: cwd.to_path_buf(),
            ..self.ctx.clone()
        };
        commands::run(std::iter::once("nexus").chain(args.iter().copied()), &ctx)
    }

    /// Runs a command that must succeed and returns its output.
    pub fn ok(&self, args: &[&str]) -> String {
        let result = self.run(args);
        assert_eq!(
            result.exit_code,
            0,
            "`nexus {}` failed:\n{}",
            args.join(" "),
            result.text()
        );
        result.text()
    }

    /// Runs a command that must fail and returns its output.
    pub fn fails(&self, args: &[&str]) -> String {
        let result = self.run(args);
        assert_ne!(
            result.exit_code,
            0,
            "`nexus {}` should have failed:\n{}",
            args.join(" "),
            result.text()
        );
        result.text()
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(relative.split('/'));
        path
    }

    pub fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn remove(&self, relative: &str) {
        fs::remove_file(self.path(relative)).unwrap();
    }

    pub fn repo(&self) -> Repo {
        Repo::discover(&self.root).unwrap()
    }

    pub fn head_commit(&self) -> ObjectId {
        let repo = self.repo();
        let head = repo.refs().head().unwrap();
        repo.refs()
            .resolve(&head)
            .unwrap()
            .expect("HEAD has a commit")
    }

    pub fn commit(&self, id: &ObjectId) -> Commit {
        let body = self.repo().odb().read_kind(id, ObjectKind::Commit).unwrap();
        Commit::decode(&body).unwrap()
    }

    /// Every file in a commit and its exact content, rebuilt from the object store.
    pub fn files_at(&self, commit: &ObjectId) -> BTreeMap<String, Vec<u8>> {
        let repo = self.repo();
        let tree = self.commit(commit).tree;
        tree::flatten(repo.odb(), &tree)
            .unwrap()
            .into_iter()
            .map(|file| {
                let mut bytes = Vec::new();
                content::write_content(repo.odb(), &file.id, &mut bytes, Path::new("test"))
                    .unwrap();
                (file.path.as_str().to_owned(), bytes)
            })
            .collect()
    }

    pub fn latest_op(&self) -> (ObjectId, Op) {
        oplog::latest(&self.repo()).unwrap().expect("an op exists")
    }

    /// How many ops the log holds.
    pub fn op_count(&self) -> usize {
        let repo = self.repo();
        let mut count = 0;
        let mut next = oplog::latest(&repo).unwrap().map(|(id, _)| id);
        while let Some(id) = next {
            count += 1;
            next = oplog::read_op(&repo, &id).unwrap().parent;
        }
        count
    }

    /// The files in the newest op's `saved` tree and their content.
    pub fn saved_files(&self) -> BTreeMap<String, Vec<u8>> {
        let saved = self.latest_op().1.saved.expect("the op saved files");
        self.tree_contents(&saved)
    }

    /// Every file in a tree and its exact content.
    pub fn tree_contents(&self, tree: &ObjectId) -> BTreeMap<String, Vec<u8>> {
        let repo = self.repo();
        tree::flatten(repo.odb(), tree)
            .unwrap()
            .into_iter()
            .map(|file| {
                let mut bytes = Vec::new();
                content::write_content(repo.odb(), &file.id, &mut bytes, Path::new("test"))
                    .unwrap();
                (file.path.as_str().to_owned(), bytes)
            })
            .collect()
    }

    /// Every file in the working tree (outside `.nexus`) and its content.
    pub fn disk_files(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect(&self.root, "", false, &mut files);
        files
    }

    /// Every file under the root, `.nexus` included, for checking that a
    /// refused command changed nothing at all.
    pub fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect(&self.root, "", true, &mut files);
        files
    }

    pub fn read(&self, relative: &str) -> Vec<u8> {
        fs::read(self.path(relative)).unwrap()
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.path(relative).exists()
    }

    /// How many object files the store holds.
    pub fn object_count(&self) -> usize {
        let objects = self.root.join(".nexus").join("objects");
        fs::read_dir(objects)
            .unwrap()
            .map(|dir| fs::read_dir(dir.unwrap().path()).unwrap().count())
            .sum()
    }
}

fn collect(dir: &Path, prefix: &str, with_repo_dir: bool, files: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if name == ".nexus" && prefix.is_empty() && !with_repo_dir {
            continue;
        }
        let path = format!("{prefix}{name}");
        if entry.file_type().unwrap().is_dir() {
            collect(&entry.path(), &format!("{path}/"), with_repo_dir, files);
        } else {
            files.insert(path, fs::read(entry.path()).unwrap());
        }
    }
}

/// Deterministic pseudo-random bytes (xorshift64*). Incompressible, like
/// already-compressed media.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.max(1);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.extend_from_slice(&state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(len);
    out
}
