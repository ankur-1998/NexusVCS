//! `nexus branch`, `nexus branch <name> [<start>]`, `nexus branch -d|-D <name>`

use super::{Ctx, Output, Style};
use crate::error::{Error, Result};
use crate::graph;
use crate::oplog::Transaction;
use crate::refs::{self, BRANCH_PREFIX, Head};
use crate::repo::Repo;
use crate::rev;

pub struct Args<'a> {
    pub name: Option<&'a str>,
    pub start: Option<&'a str>,
    pub delete: bool,
    pub force_delete: bool,
}

pub fn run(args: &Args, ctx: &Ctx, command_line: &str, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    match (args.name, args.delete || args.force_delete) {
        (None, false) => list(&repo, out),
        (None, true) => Err(Error::Invalid(
            "say which branch to delete: nexus branch -d <name>".to_owned(),
        )),
        (Some(name), true) => {
            if args.start.is_some() {
                return Err(Error::Invalid(
                    "nexus branch -d takes one branch name".to_owned(),
                ));
            }
            delete(&repo, name, args.force_delete, ctx, command_line, out)
        }
        (Some(name), false) => create(&repo, name, args.start, ctx, command_line, out),
    }
}

fn list(repo: &Repo, out: &mut Output) -> Result<()> {
    let head = repo.refs().head()?;
    if let Head::Detached(id) = &head {
        out.styled(Style::Added, format!("* (HEAD detached at {})", id.short()));
    }
    let mut any = false;
    for name in repo.refs().list()?.keys() {
        let Some(branch) = name.strip_prefix(BRANCH_PREFIX) else {
            continue;
        };
        any = true;
        if head == Head::Branch(name.clone()) {
            out.styled(Style::Added, format!("* {branch}"));
        } else {
            out.line(format!("  {branch}"));
        }
    }
    if !any && let Head::Branch(_) = head {
        // An unborn branch has no ref file yet, but it's still the current branch.
        out.styled(
            Style::Added,
            format!("* {} (no commits yet)", head.branch().unwrap_or("?")),
        );
    }
    Ok(())
}

fn create(
    repo: &Repo,
    name: &str,
    start: Option<&str>,
    ctx: &Ctx,
    command_line: &str,
    out: &mut Output,
) -> Result<()> {
    let full = refs::full_ref_name(BRANCH_PREFIX, name, "branch")?;
    let mut tx = Transaction::begin(repo, command_line, ctx.clock.now())?;
    let target = rev::resolve_commit(repo, start.unwrap_or("HEAD"))?;
    if repo.refs().get(&full)?.is_some() {
        return Err(Error::Invalid(format!(
            "a branch named {name} already exists"
        )));
    }
    if let Some(existing) = repo.refs().conflict(&full)? {
        return Err(Error::Invalid(format!(
            "can't create {name}: it would clash with {existing}"
        )));
    }
    tx.set_ref(&full, target);
    tx.finish()?;
    out.line(format!("Created branch {name} at {}", target.short()));
    Ok(())
}

fn delete(
    repo: &Repo,
    name: &str,
    force: bool,
    ctx: &Ctx,
    command_line: &str,
    out: &mut Output,
) -> Result<()> {
    let full = refs::full_ref_name(BRANCH_PREFIX, name, "branch")?;
    let mut tx = Transaction::begin(repo, command_line, ctx.clock.now())?;
    let Some(tip) = repo.refs().get(&full)? else {
        return Err(Error::Invalid(format!("there's no branch named {name}")));
    };
    let head = repo.refs().head()?;
    if head == Head::Branch(full.clone()) {
        return Err(Error::Invalid(format!(
            "can't delete {name}: it's the current branch"
        )));
    }
    if !force {
        let merged = match repo.refs().resolve(&head)? {
            Some(current) => graph::is_ancestor(repo.odb(), tip, current)?,
            None => false,
        };
        if !merged {
            return Err(Error::Invalid(format!(
                "{name} isn't merged into the current branch, so deleting it would lose commits; \
                 use -D to delete it anyway"
            )));
        }
    }
    tx.delete_ref(&full);
    tx.finish()?;
    out.line(format!("Deleted branch {name} (was {})", tip.short()));
    Ok(())
}
