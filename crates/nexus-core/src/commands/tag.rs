//! `nexus tag`, `nexus tag <name> [<commit>]`, `nexus tag -d <name>`
//!
//! Lightweight tags: a ref under `refs/tags/` naming a commit.

use super::{Ctx, Output};
use crate::error::{Error, Result};
use crate::oplog::Transaction;
use crate::refs::{self, TAG_PREFIX};
use crate::repo::Repo;
use crate::rev;

pub fn run(
    name: Option<&str>,
    commit: Option<&str>,
    delete: bool,
    ctx: &Ctx,
    command_line: &str,
    out: &mut Output,
) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let Some(name) = name else {
        if delete {
            return Err(Error::Invalid(
                "say which tag to delete: nexus tag -d <name>".to_owned(),
            ));
        }
        for full in repo.refs().list()?.keys() {
            if let Some(tag) = full.strip_prefix(TAG_PREFIX) {
                out.line(tag);
            }
        }
        return Ok(());
    };
    let full = refs::full_ref_name(TAG_PREFIX, name, "tag")?;
    if delete {
        if commit.is_some() {
            return Err(Error::Invalid("nexus tag -d takes one tag name".to_owned()));
        }
        let mut tx = Transaction::begin(&repo, command_line, ctx.clock.now())?;
        let Some(id) = repo.refs().get(&full)? else {
            return Err(Error::Invalid(format!("there's no tag named {name}")));
        };
        tx.delete_ref(&full);
        tx.finish()?;
        out.line(format!("Deleted tag {name} (was {})", id.short()));
        return Ok(());
    }
    let mut tx = Transaction::begin(&repo, command_line, ctx.clock.now())?;
    let target = rev::resolve_commit(&repo, commit.unwrap_or("HEAD"))?;
    if repo.refs().get(&full)?.is_some() {
        return Err(Error::Invalid(format!("a tag named {name} already exists")));
    }
    if let Some(existing) = repo.refs().conflict(&full)? {
        return Err(Error::Invalid(format!(
            "can't create {name}: it would clash with {existing}"
        )));
    }
    tx.set_ref(&full, target);
    tx.finish()?;
    out.line(format!("Tagged {} as {name}", target.short()));
    Ok(())
}
