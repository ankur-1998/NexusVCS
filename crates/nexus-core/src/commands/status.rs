//! `nexus status`

use super::{Ctx, Output, Style};
use crate::error::Result;
use crate::oplog::Transaction;
use crate::path::RepoPath;
use crate::refs::Head;
use crate::repo::Repo;
use crate::worktree::{self, Change, ChangeKind};

pub fn run(ctx: &Ctx, command_line: &str, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let index = repo.load_index()?;
    let status = worktree::status(&repo, &index)?;
    refresh_index(&repo, ctx, command_line, &status.refreshed);
    // Paths are shown relative to the current directory, as Git shows them,
    // so they can be pasted into the commands the hints suggest.
    let here = repo
        .repo_path(&ctx.cwd)
        .unwrap_or_else(|_| RepoPath::root());

    // Git's layout: the head line, then each section with a blank line
    // before it (none before the first, unless "No commits yet" is shown).
    let head = repo.refs().head()?;
    let unborn = repo.refs().resolve(&head)?.is_none();
    match &head {
        Head::Branch(_) => out.line(format!("On branch {}", head.branch().unwrap_or("?"))),
        // On stdout, in red, as Git shows it.
        Head::Detached(id) => {
            out.styled(Style::Removed, format!("HEAD detached at {}", id.short()));
        }
    }
    let mut gap = false;
    if unborn {
        out.line("");
        out.line("No commits yet");
        gap = true;
    }
    for warning in &status.warnings {
        out.warning(warning);
    }
    let mut section = |out: &mut Output, title: &str| {
        if gap {
            out.line("");
        }
        gap = true;
        out.line(title);
    };

    if !status.staged.is_empty() {
        section(out, "Changes to be committed:");
        out.styled(
            Style::Muted,
            "  (use \"nexus restore --staged <file>...\" to unstage)",
        );
        list(out, &status.staged, &here, Style::Added);
    }
    if !status.unstaged.is_empty() {
        section(out, "Changes not staged for commit:");
        out.styled(
            Style::Muted,
            "  (use \"nexus add <file>...\" to update what will be committed)",
        );
        out.styled(
            Style::Muted,
            "  (use \"nexus restore <file>...\" to discard changes in the working tree)",
        );
        list(out, &status.unstaged, &here, Style::Removed);
    }
    if !status.untracked.is_empty() {
        section(out, "Untracked files:");
        out.styled(
            Style::Muted,
            "  (use \"nexus add <file>...\" to include in what will be committed)",
        );
        for path in &status.untracked {
            // A collapsed directory keeps its trailing `/`.
            let (path, dir) = match path.strip_suffix('/') {
                Some(dir) => (dir, "/"),
                None => (path.as_str(), ""),
            };
            let shown = match RepoPath::parse(path) {
                Ok(path) => relative(&path, &here),
                Err(_) => crate::path::quote(path).into_owned(),
            };
            out.styled(Style::Removed, format!("\t{shown}{dir}"));
        }
    }
    if gap {
        out.line("");
    }
    if status.staged.is_empty() && status.unstaged.is_empty() {
        if !status.untracked.is_empty() {
            out.line(
                "nothing added to commit but untracked files present (use \"nexus add\" to track)",
            );
        } else if unborn {
            out.line("nothing to commit (create/copy files and use \"nexus add\" to track)");
        } else {
            out.line("nothing to commit, working tree clean");
        }
    } else if status.staged.is_empty() {
        out.line("no changes added to commit (use \"nexus add\")");
    }
    Ok(())
}

fn list(out: &mut Output, changes: &[Change], here: &RepoPath, style: Style) {
    for change in changes {
        let label = match change.kind {
            ChangeKind::Added => "new file:",
            ChangeKind::Modified => "modified:",
            ChangeKind::Deleted => "deleted: ",
        };
        let shown = relative(&change.path, here);
        out.styled(style, format!("\t{label}   {shown}"));
    }
}

/// `path` as seen from the directory `here`, with a `../` for each level up
/// (`.` for `here` itself), quoted the way Git quotes names.
fn relative(path: &RepoPath, here: &RepoPath) -> String {
    let names: Vec<&str> = path.components().collect();
    let base: Vec<&str> = here.components().collect();
    let shared = names.iter().zip(&base).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; base.len() - shared];
    parts.extend(&names[shared..]);
    if parts.is_empty() {
        return ".".to_owned();
    }
    crate::path::quote(&parts.join("/")).into_owned()
}

/// Saves the size and time of files that were hashed and found unchanged, so
/// the next `status` can skip them. Best effort: if another command holds the
/// lock, the cache simply isn't updated this time.
fn refresh_index(
    repo: &Repo,
    ctx: &Ctx,
    command_line: &str,
    refreshed: &[(crate::path::RepoPath, crate::index::IndexEntry)],
) {
    if refreshed.is_empty() {
        return;
    }
    let attempt = || -> Result<()> {
        let mut tx = Transaction::begin(repo, command_line, ctx.clock.now())?;
        let mut index = tx.index().clone();
        let mut changed = false;
        for (path, entry) in refreshed {
            // Only if the entry is still what was compared (nothing staged since).
            if index
                .get(path)
                .is_some_and(|current| current.id == entry.id && current.kind == entry.kind)
            {
                index.insert(path.clone(), *entry);
                changed = true;
            }
        }
        if changed {
            tx.set_index(index);
        }
        tx.finish()?;
        Ok(())
    };
    // Refreshing is only an optimization, so any failure (usually the lock
    // being held by another command) is ignored.
    let _ = attempt();
}
