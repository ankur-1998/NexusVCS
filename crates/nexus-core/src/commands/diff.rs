//! `nexus diff [--staged] [<commit> [<commit>]] [-- <path>...]`

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use super::{Ctx, Output, render};
use crate::cli::DiffOptions;
use crate::content::{self, Sink};
use crate::diff::{self, Side};
use crate::error::{Error, Result};
use crate::graph;
use crate::hash::ObjectId;
use crate::path::RepoPath;
use crate::repo::{self, Repo};
use crate::rev;
use crate::worktree::{self, Checker, Version, Working};

pub fn run(
    staged: bool,
    options: DiffOptions,
    revs: &[String],
    paths: &[PathBuf],
    ctx: &Ctx,
    out: &mut Output,
) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let index = repo.load_index()?;
    // Positional arguments are commits until one isn't; that one and the
    // rest are paths, as with `git diff`.
    let mut commits = Vec::new();
    let mut scopes = Vec::new();
    for arg in revs {
        let path = repo::absolutize(&ctx.cwd, std::path::Path::new(arg));
        // Like Git, only a file on disk makes a commit name ambiguous; a
        // tracked path that's been deleted can still be named as a path.
        let on_disk = fs::symlink_metadata(&path).is_ok();
        let is_path = on_disk
            || repo
                .repo_path(&path)
                .is_ok_and(|scope| index.within(&scope).next().is_some());
        if scopes.is_empty() {
            match rev::resolve_commit(&repo, arg) {
                Ok(_) if on_disk => {
                    return Err(Error::Invalid(format!(
                        "{arg} is both a commit and a path; put paths after `--`"
                    )));
                }
                Ok(id) => {
                    commits.push(id);
                    continue;
                }
                // Why it isn't a commit matters (an ambiguous prefix lists
                // its candidates), unless it's a path.
                Err(Error::Invalid(message)) if !is_path => {
                    return Err(Error::Invalid(format!(
                        "{message}\nTo name a path instead, put it after `--`."
                    )));
                }
                Err(err) if !is_path => return Err(err),
                Err(_) => {}
            }
        } else if !is_path {
            return Err(Error::Invalid(format!(
                "{arg} isn't a path in the working tree (commits come before paths)"
            )));
        }
        scopes.push(repo.repo_path(&path)?);
    }
    for path in paths {
        scopes.push(repo.repo_path(&repo::absolutize(&ctx.cwd, path))?);
    }
    if scopes.is_empty() {
        scopes.push(RepoPath::root());
    }

    let tree_of = |commit: &ObjectId| -> Result<BTreeMap<RepoPath, Version>> {
        let tree = graph::read_commit(repo.odb(), commit)?.tree;
        worktree::tree_files(repo.odb(), Some(tree))
    };
    let stored = |files: BTreeMap<RepoPath, Version>| -> BTreeMap<RepoPath, Side> {
        files
            .into_iter()
            .map(|(path, version)| (path, Side::Stored(version)))
            .collect()
    };
    let (old, new) = match (commits.as_slice(), staged) {
        ([], false) => {
            let old = worktree::index_files(&index);
            (stored(old), working_versions(&repo, &index, &scopes)?)
        }
        ([], true) => {
            let head = worktree::tree_files(repo.odb(), worktree::head_tree(&repo)?)?;
            (stored(head), stored(worktree::index_files(&index)))
        }
        ([commit], false) => (
            stored(tree_of(commit)?),
            working_versions(&repo, &index, &scopes)?,
        ),
        ([commit], true) => (
            stored(tree_of(commit)?),
            stored(worktree::index_files(&index)),
        ),
        ([a, b], false) => (stored(tree_of(a)?), stored(tree_of(b)?)),
        ([_, _], true) => {
            return Err(Error::Invalid(
                "--staged compares with the index, so it takes at most one commit".to_owned(),
            ));
        }
        _ => {
            return Err(Error::Invalid(
                "nexus diff takes at most two commits".to_owned(),
            ));
        }
    };
    render_changes(&repo, &old, &new, &scopes, options, out)
}

/// What the working tree holds for each tracked (indexed) path inside
/// `scopes`, leaving out missing files. Clean files take their version from
/// the index, without hashing. Untracked files never count, as in
/// `git diff`, even where a commit being compared has them.
pub fn working_versions(
    repo: &Repo,
    index: &crate::index::Index,
    scopes: &[RepoPath],
) -> Result<BTreeMap<RepoPath, Side>> {
    let checker = Checker::new(repo)?;
    let entries = index
        .iter()
        .filter(|(path, _)| scopes.iter().any(|scope| scope.contains(path)))
        .map(|(path, entry)| (path.clone(), *entry))
        .collect();
    let mut sides = BTreeMap::new();
    let mut unhashed = Vec::new();
    for (path, working) in checker.check_all(entries, BTreeMap::new())? {
        let side = match working {
            // Unchanged since staging: the content is the stored version.
            Working::Clean { .. } => match index.get(&path) {
                Some(entry) => Side::Stored(Version {
                    kind: entry.kind,
                    id: entry.id,
                }),
                None => continue,
            },
            Working::Modified(file) => {
                let Some(id) = file.id else {
                    // Its size showed it changed, or it couldn't be read:
                    // hash it below, so an error names the file.
                    let size = file.meta.len();
                    unhashed.push(((path, file.kind, file.fs_path.clone()), file.fs_path, size));
                    continue;
                };
                Side::Working {
                    fs_path: file.fs_path,
                    version: Version {
                        kind: file.kind,
                        id,
                    },
                }
            }
            Working::Missing => continue,
        };
        sides.insert(path, side);
    }
    for ((path, kind, fs_path), stored) in content::store_files(Sink::HashOnly, unhashed) {
        let id = match stored {
            Ok(stored) => stored.id,
            // Deleted since it was checked.
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(err) => return Err(err),
        };
        sides.insert(
            path,
            Side::Working {
                fs_path,
                version: Version { kind, id },
            },
        );
    }
    Ok(sides)
}

/// Prints the diff of every path that differs between `old` and `new`
/// inside `scopes`.
pub fn render_changes(
    repo: &Repo,
    old: &BTreeMap<RepoPath, Side>,
    new: &BTreeMap<RepoPath, Side>,
    scopes: &[RepoPath],
    options: DiffOptions,
    out: &mut Output,
) -> Result<()> {
    let paths: BTreeSet<&RepoPath> = old.keys().chain(new.keys()).collect();
    for path in paths {
        if !scopes.iter().any(|scope| scope.contains(path)) {
            continue;
        }
        let (old_side, new_side) = (old.get(path), new.get(path));
        if old_side.map(Side::version) == new_side.map(Side::version) {
            continue;
        }
        let file = diff::file_diff(
            repo.odb(),
            path,
            old_side,
            new_side,
            options.diff_algorithm.into(),
            options.context,
        )?;
        render::file_diff(out, &file);
    }
    Ok(())
}
