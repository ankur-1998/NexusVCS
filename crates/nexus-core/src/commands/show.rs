//! `nexus show [<commit>]`: a commit and its changes against its first parent.

use std::collections::BTreeMap;

use super::{Ctx, Output, render};
use crate::cli::DiffOptions;
use crate::diff::Side;
use crate::error::Result;
use crate::graph;
use crate::path::RepoPath;
use crate::repo::Repo;
use crate::rev;
use crate::worktree;

pub fn run(spec: &str, options: DiffOptions, ctx: &Ctx, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let id = rev::resolve_commit(&repo, spec)?;
    let commit = graph::read_commit(repo.odb(), &id)?;
    let labels = super::log::labels(&repo, &repo.refs().head()?)?;
    let label = labels
        .get(&id)
        .map(|names| format!(" ({})", names.join(", ")))
        .unwrap_or_default();
    render::commit_header(out, &id, &commit, &label);

    let parent_tree = match commit.parents.first() {
        Some(parent) => Some(graph::read_commit(repo.odb(), parent)?.tree),
        None => None,
    };
    let stored = |tree| -> Result<BTreeMap<RepoPath, Side>> {
        Ok(worktree::tree_files(repo.odb(), tree)?
            .into_iter()
            .map(|(path, version)| (path, Side::Stored(version)))
            .collect())
    };
    let old = stored(parent_tree)?;
    let new = stored(Some(commit.tree))?;
    out.line("");
    super::diff::render_changes(&repo, &old, &new, &[RepoPath::root()], options, out)
}
