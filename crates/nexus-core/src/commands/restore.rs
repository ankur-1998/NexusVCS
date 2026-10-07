//! `nexus restore [--staged] [--source <commit>] <path...>`

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use super::{Ctx, Output, Style};
use crate::content::{self, Sink};
use crate::error::{Error, Result};
use crate::graph;
use crate::index::{FileKind, IndexEntry};
use crate::oplog::Transaction;
use crate::path::RepoPath;
use crate::platform;
use crate::repo::{self, Repo};
use crate::rev;
use crate::walk::OnDisk;
use crate::worktree::{self, Checker, Plan, Version};

pub fn run(
    staged: bool,
    source: Option<&str>,
    paths: &[PathBuf],
    ctx: &Ctx,
    command_line: &str,
    out: &mut Output,
) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let scopes = paths
        .iter()
        .map(|path| repo.repo_path(&repo::absolutize(&ctx.cwd, path)))
        .collect::<Result<Vec<_>>>()?;
    let mut tx = Transaction::begin(&repo, command_line, ctx.clock.now())?;
    let source_files = match source {
        Some(spec) => {
            let commit = rev::resolve_commit(&repo, spec)?;
            Some(worktree::tree_files(
                repo.odb(),
                Some(graph::read_commit(repo.odb(), &commit)?.tree),
            )?)
        }
        None => None,
    };
    let within = |files: &BTreeMap<RepoPath, Version>| -> BTreeMap<RepoPath, Version> {
        files
            .iter()
            .filter(|(path, _)| scopes.iter().any(|scope| scope.contains(path)))
            .map(|(path, version)| (path.clone(), *version))
            .collect()
    };
    if staged {
        let from = match source_files {
            Some(files) => files,
            None => worktree::tree_files(repo.odb(), worktree::head_tree(&repo)?)?,
        };
        unstage(&mut tx, &within(&from), &scopes, out)?;
    } else {
        let from = match source_files {
            Some(files) => within(&files),
            None => within(&worktree::index_files(tx.index())),
        };
        if let Some((err, changed)) =
            discard(&repo, &mut tx, &from, source.is_some(), &scopes, out)?
        {
            // Record an op only if a file had already changed.
            return Err(if changed { tx.abandon(err) } else { err });
        }
    }
    tx.finish()?;
    Ok(())
}

/// Resets the index entries in `scopes` to `from`, leaving working files alone.
fn unstage(
    tx: &mut Transaction,
    from: &BTreeMap<RepoPath, Version>,
    scopes: &[RepoPath],
    out: &mut Output,
) -> Result<()> {
    let mut index = tx.index().clone();
    let staged: Vec<RepoPath> = scopes
        .iter()
        .flat_map(|scope| index.within(scope).cloned().collect::<Vec<_>>())
        .collect();
    if staged.is_empty() && from.is_empty() {
        return Err(Error::Invalid(no_match(scopes)));
    }
    if let Some(path) = from.keys().find(|path| worktree::in_repo_dir(path)) {
        return Err(Error::Invalid(format!(
            "{path} can't be staged: it would be inside the repository's own .nexus directory"
        )));
    }
    let mut count = 0;
    for path in staged {
        if !from.contains_key(&path) && index.remove(&path).is_some() {
            count += 1;
        }
    }
    for (path, version) in from {
        let current = index.get(path).map(|entry| Version {
            kind: entry.kind,
            id: entry.id,
        });
        if current != Some(*version) {
            // No cached size or time: the next status hashes the file.
            index.insert(
                path.clone(),
                IndexEntry {
                    kind: version.kind,
                    id: version.id,
                    size: 0,
                    mtime_ns: 0,
                },
            );
            count += 1;
        }
    }
    tx.set_index(index);
    if count == 0 {
        out.styled(
            Style::Muted,
            "nothing to unstage: the index already matches",
        );
    } else {
        out.line(format!("Unstaged {count} {}", files(count)));
    }
    Ok(())
}

/// Overwrites working files in `scopes` with `from`. With a commit as the
/// source, tracked files in `scopes` that the commit doesn't have are deleted
/// too. What the files held is saved in the op first. Returns the error if
/// changing the files failed, and whether any had changed by then, for the
/// caller to record.
fn discard(
    repo: &Repo,
    tx: &mut Transaction,
    from: &BTreeMap<RepoPath, Version>,
    from_commit: bool,
    scopes: &[RepoPath],
    out: &mut Output,
) -> Result<Option<(Error, bool)>> {
    let index = tx.index().clone();
    let mut plan = Plan::default();
    for (path, version) in from {
        plan.writes.push((path.clone(), *version));
    }
    if from_commit {
        for scope in scopes {
            for path in index.within(scope) {
                if !from.contains_key(path) && !plan.deletes.contains(path) {
                    plan.deletes.push(path.clone());
                }
            }
        }
    }
    if plan.writes.is_empty() && plan.deletes.is_empty() {
        return Err(Error::Invalid(no_match(scopes)));
    }

    // Save what's on disk now, except files that already hold exactly what
    // would be written (which are left alone).
    let checker = Checker::new(repo)?;
    let targets: BTreeMap<&RepoPath, Option<Version>> = plan
        .writes
        .iter()
        .map(|(path, version)| (path, Some(*version)))
        .chain(plan.deletes.iter().map(|path| (path, None)))
        .collect();
    let deleting: HashSet<&RepoPath> = plan.deletes.iter().collect();
    let mut present = Vec::new();
    let mut absent = HashSet::new();
    let mut problems = Vec::new();
    if platform::ignores_case(repo.root()) {
        for (a, b) in crate::path::case_collisions(from.keys()) {
            problems.push(format!(
                "{b} (the source also has {a}, and this filesystem can't hold names that differ \
                 only in letter case)"
            ));
        }
    }
    for (path, target) in &targets {
        if worktree::in_repo_dir(path) {
            problems.push(format!("{path} (the name can't be used on this system)"));
            continue;
        }
        if let OnDisk::File { fs_path, meta } = checker.find(path)? {
            let recorded = target
                .map(|version| version.kind)
                .or_else(|| index.get(path).map(|entry| entry.kind))
                .unwrap_or(FileKind::File);
            let kind = checker.kind_of(&meta, recorded);
            present.push((((*path).clone(), kind), fs_path, meta.len()));
        } else if target.is_none() {
            // Already gone: nothing to delete.
            absent.insert((*path).clone());
        } else if path.to_fs_path(repo.root()).is_none() {
            problems.push(format!("{path} (the name can't be used on this system)"));
        } else if let Some(problem) = worktree::blocked(&checker, repo, path, &deleting)? {
            problems.push(problem);
        }
    }
    if !problems.is_empty() {
        return Err(Error::Invalid(worktree::refusal(
            "restoring",
            &problems,
            "Move those untracked files out of the way first.",
        )));
    }
    let mut saved = Vec::new();
    let mut identical = HashSet::new();
    for ((path, kind), file) in content::store_files(Sink::Store(repo.odb()), present) {
        let version = Version { kind, id: file?.id };
        if targets.get(&path).copied().flatten() == Some(version) {
            identical.insert(path);
        } else {
            saved.push((path, version));
        }
    }
    plan.writes.retain(|(path, _)| !identical.contains(path));
    plan.deletes.retain(|path| !absent.contains(path));
    let count = plan.writes.len() + plan.deletes.len();
    if count == 0 {
        out.styled(Style::Muted, "nothing to restore: the files already match");
        return Ok(None);
    }
    tx.save_working_files(&saved)?;

    let mut updated = index;
    tx.changing_worktree();
    let mut done = 0;
    if let Err(err) = worktree::apply(repo, &mut updated, &plan, &mut done) {
        return Ok(Some((err, done > 0)));
    }
    // Restoring from the index leaves the files matching it, so it records
    // their new size and time. Restoring from a commit changes only the
    // working tree: the index still says what's staged.
    if !from_commit {
        tx.set_index(updated);
    }
    out.line(format!("Restored {count} {}", files(count)));
    Ok(None)
}

fn no_match(scopes: &[RepoPath]) -> String {
    let named: Vec<String> = scopes.iter().map(ToString::to_string).collect();
    format!("{} doesn't match any tracked files", named.join(", "))
}

fn files(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}
