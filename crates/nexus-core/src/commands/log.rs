//! `nexus log [-n N] [--oneline]`: first-parent history from HEAD.

use std::collections::HashMap;

use super::{Ctx, Output, Style, render};
use crate::error::Result;
use crate::graph;
use crate::hash::ObjectId;
use crate::refs::{BRANCH_PREFIX, Head, TAG_PREFIX};
use crate::repo::Repo;

pub fn run(max_count: Option<usize>, oneline: bool, ctx: &Ctx, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let head = repo.refs().head()?;
    let Some(tip) = repo.refs().resolve(&head)? else {
        let branch = head.branch().unwrap_or("HEAD");
        out.styled(Style::Muted, format!("no commits yet on {branch}"));
        return Ok(());
    };
    let labels = labels(&repo, &head)?;
    for (shown, step) in graph::walk_commits(repo.odb(), tip, true).enumerate() {
        if max_count.is_some_and(|max| shown >= max) {
            break;
        }
        let (id, commit) = step?;
        let label = labels
            .get(&id)
            .map(|names| format!(" ({})", names.join(", ")))
            .unwrap_or_default();
        if oneline {
            out.line(format!("{}{label} {}", id.short(), commit.subject()));
        } else {
            if shown > 0 {
                out.line("");
            }
            render::commit_header(out, &id, &commit, &label);
        }
    }
    Ok(())
}

/// Names to show next to the commits they point at: HEAD (with its branch)
/// first, then other branches, then tags.
pub fn labels(repo: &Repo, head: &Head) -> Result<HashMap<ObjectId, Vec<String>>> {
    let mut labels: HashMap<ObjectId, Vec<String>> = HashMap::new();
    if let Head::Detached(id) = head {
        labels.entry(*id).or_default().push("HEAD".to_owned());
    }
    let mut tags = Vec::new();
    for (name, id) in repo.refs().list()? {
        if let Some(branch) = name.strip_prefix(BRANCH_PREFIX) {
            let names = labels.entry(id).or_default();
            if head == &Head::Branch(name.clone()) {
                names.insert(0, format!("HEAD -> {branch}"));
            } else {
                names.push(branch.to_owned());
            }
        } else if let Some(tag) = name.strip_prefix(TAG_PREFIX) {
            tags.push((id, format!("tag: {tag}")));
        }
    }
    for (id, tag) in tags {
        labels.entry(id).or_default().push(tag);
    }
    Ok(labels)
}
