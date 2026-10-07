//! `nexus checkout <branch|commit>`

use super::{Ctx, Output, Style};
use crate::error::Result;
use crate::graph;
use crate::oplog::Transaction;
use crate::refs::{BRANCH_PREFIX, Head};
use crate::repo::Repo;
use crate::rev;
use crate::worktree;

pub fn run(target: &str, ctx: &Ctx, command_line: &str, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let mut tx = Transaction::begin(&repo, command_line, ctx.clock.now())?;
    let current_head = repo.refs().head()?;

    // A branch name switches to that branch; anything else detaches HEAD.
    let branch_ref = format!("{BRANCH_PREFIX}{}", crate::path::nfc(target));
    let branch_tip = if crate::refs::is_valid_ref_name(&branch_ref) {
        repo.refs().get(&branch_ref)?
    } else {
        None
    };
    let (new_head, commit) = if let Some(tip) = branch_tip {
        (Head::Branch(branch_ref.clone()), tip)
    } else {
        let commit = rev::resolve_commit(&repo, target)?;
        (Head::Detached(commit), commit)
    };
    if new_head == current_head {
        out.line(format!("Already on {}", describe(&new_head)));
        return Ok(());
    }

    let head_files = worktree::tree_files(repo.odb(), worktree::head_tree(&repo)?)?;
    let target_tree = graph::read_commit(repo.odb(), &commit)?.tree;
    let target_files = worktree::tree_files(repo.odb(), Some(target_tree))?;
    let mut index = tx.index().clone();
    let plan = worktree::plan_switch(&repo, &index, &head_files, &target_files)?;
    tx.save_working_files(&plan.saved)?;
    if !plan.writes.is_empty() || !plan.deletes.is_empty() {
        tx.changing_worktree();
        let mut done = 0;
        if let Err(err) = worktree::apply(&repo, &mut index, &plan, &mut done) {
            // Record an op only if a file had already changed.
            return Err(if done > 0 { tx.abandon(err) } else { err });
        }
        tx.set_index(index);
    }
    tx.set_head(new_head.clone());
    tx.finish()?;

    let changed = plan.writes.len() + plan.deletes.len();
    match &new_head {
        Head::Branch(_) => out.line(format!(
            "Switched to branch {} ({changed} files changed)",
            describe(&new_head)
        )),
        Head::Detached(id) => {
            let subject = graph::read_commit(repo.odb(), id)?.subject().to_owned();
            out.line(format!(
                "HEAD is now at {} {subject} ({changed} files changed)",
                id.short()
            ));
            out.styled(
                Style::Warning,
                "warning: HEAD is detached: new commits won't be on any branch. To keep them, create one \
                 with `nexus branch <name>`.",
            );
        }
    }
    Ok(())
}

fn describe(head: &Head) -> String {
    match head {
        Head::Branch(_) => format!("'{}'", head.branch().unwrap_or("?")),
        Head::Detached(id) => format!("detached HEAD at {}", id.short()),
    }
}
