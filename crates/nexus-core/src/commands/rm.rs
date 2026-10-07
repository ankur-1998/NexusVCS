//! `nexus rm [--cached] <path...>`

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::{Ctx, Output, Style};
use crate::error::{Error, Result};
use crate::oplog::Transaction;
use crate::repo::{self, Repo};
use crate::walk::{Lookup, OnDisk};
use crate::worktree::{self, Checker, Version, Working};

pub fn run(
    cached: bool,
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
    let mut index = tx.index().clone();

    let mut matched = BTreeSet::new();
    let mut unmatched = Vec::new();
    for scope in &scopes {
        let mut any = false;
        for path in index.within(scope) {
            any = true;
            matched.insert(path.clone());
        }
        if !any {
            unmatched.push(scope.to_string());
        }
    }
    if !unmatched.is_empty() {
        return Err(Error::Invalid(format!(
            "{} doesn't match any tracked files",
            unmatched.join(", ")
        )));
    }

    // Like Git, refuse to remove what exists nowhere else: a file with
    // changes that aren't committed, or (with --cached) staged content that
    // matches neither the file nor HEAD.
    let head = worktree::tree_files(repo.odb(), worktree::head_tree(&repo)?)?;
    let entries: Vec<_> = matched
        .iter()
        .filter_map(|path| index.get(path).map(|entry| (path.clone(), *entry)))
        .collect();
    let mut problems = Vec::new();
    let mut saved = Vec::new();
    for (path, working) in Checker::new(&repo)?.check_all(entries, BTreeMap::new())? {
        let Some(entry) = index.get(&path) else {
            continue;
        };
        let version = Version {
            kind: entry.kind,
            id: entry.id,
        };
        let staged = head.get(&path) != Some(&version);
        match working {
            Working::Clean { .. } if staged && !cached => {
                problems.push(format!("{path} (staged changes)"));
            }
            Working::Clean { .. } => saved.push((path, version)),
            Working::Modified(_) if !cached => {
                problems.push(format!("{path} (changes not staged)"));
            }
            Working::Modified(_) if staged => {
                problems.push(format!(
                    "{path} (staged content differs from both the file and HEAD)"
                ));
            }
            Working::Modified(_) | Working::Missing => {}
        }
    }
    if !problems.is_empty() {
        return Err(Error::Invalid(worktree::refusal(
            "removing",
            &problems,
            "Commit them first, or discard the changes with `nexus restore <path>` (and \
             `nexus restore --staged <path>`) and run nexus rm again.",
        )));
    }

    if !cached {
        // The files hold exactly what's staged, which is already stored.
        tx.save_working_files(&saved)?;
        tx.changing_worktree();
        let lookup = Lookup::default();
        let mut done = 0;
        for path in &matched {
            // Nothing in `.nexus` is deleted, whatever the index says.
            if worktree::in_repo_dir(path) {
                continue;
            }
            let removed = match lookup.find(repo.root(), path)? {
                OnDisk::File { fs_path, .. } => {
                    worktree::remove_working_file(repo.root(), &fs_path)
                }
                _ => continue,
            };
            if let Err(err) = removed {
                // Record an op only if a file had already been deleted.
                return Err(if done > 0 { tx.abandon(err) } else { err });
            }
            done += 1;
        }
    }
    for path in &matched {
        index.remove(path);
    }
    tx.set_index(index);
    tx.finish()?;

    for path in &matched {
        if cached {
            out.styled(Style::Removed, format!("untracked {path} (kept on disk)"));
        } else {
            out.styled(Style::Removed, format!("removed  {path}"));
        }
    }
    Ok(())
}
