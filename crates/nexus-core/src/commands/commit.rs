//! `nexus commit -m <message> [--allow-empty]`

use super::{Ctx, Output};
use crate::config::ConfigFiles;
use crate::error::{Error, Result};
use crate::graph;
use crate::object::{Commit, ObjectKind, Signature};
use crate::oplog::Transaction;
use crate::refs::Head;
use crate::repo::Repo;

pub fn run(
    messages: &[String],
    allow_empty: bool,
    ctx: &Ctx,
    command_line: &str,
    out: &mut Output,
) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let message = clean_message(messages)?;
    let files = ConfigFiles {
        repo: Some(repo.config_path()),
        global: ctx.global_config.clone(),
    };
    let (name, email) = files.identity()?;
    let time = ctx.clock.now();

    let mut tx = Transaction::begin(&repo, command_line, time)?;
    let tree = tx.index_tree()?;
    let head = repo.refs().head()?;
    let parent = repo.refs().resolve(&head)?;
    if !allow_empty {
        match parent {
            Some(parent) if graph::read_commit(repo.odb(), &parent)?.tree == tree => {
                return Err(Error::Invalid(format!(
                    "nothing to commit: the staged files are the same as in {}. Stage changes with \
                     `nexus add`, or pass --allow-empty",
                    parent.short()
                )));
            }
            None if tx.index().is_empty() => {
                return Err(Error::Invalid(
                    "nothing to commit: stage files with `nexus add` first".to_owned(),
                ));
            }
            _ => {}
        }
    }

    let commit = Commit {
        tree,
        parents: parent.into_iter().collect(),
        author: Signature { name, email, time },
        message,
    };
    let id = repo.odb().write(ObjectKind::Commit, &commit.encode())?;
    let place = match &head {
        Head::Branch(name) => {
            tx.set_ref(name, id);
            head.branch().unwrap_or(name).to_owned()
        }
        Head::Detached(_) => {
            tx.set_head(Head::Detached(id));
            "detached HEAD".to_owned()
        }
    };
    tx.finish()?;

    let root = if parent.is_none() {
        " (root commit)"
    } else {
        ""
    };
    out.line(format!(
        "[{place}{root} {}] {}",
        id.short(),
        commit.subject()
    ));
    Ok(())
}

/// Joins `-m` paragraphs with blank lines, strips trailing whitespace from each
/// line, drops leading and trailing blank lines, and collapses runs of blank
/// lines. The result ends with one newline.
fn clean_message(messages: &[String]) -> Result<String> {
    let joined = messages.join("\n\n");
    let mut lines: Vec<&str> = Vec::new();
    for line in joined.lines().map(str::trim_end) {
        let blank = line.is_empty();
        if blank && lines.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return Err(Error::Invalid(
            "aborting the commit: the message is empty".to_owned(),
        ));
    }
    Ok(lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(parts: &[&str]) -> String {
        clean_message(&parts.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn cleans_messages() {
        assert_eq!(clean(&["first"]), "first\n");
        assert_eq!(clean(&["subject", "body"]), "subject\n\nbody\n");
        assert_eq!(
            clean(&["\n\nsubject  \n\n\n\nbody\t\n\n"]),
            "subject\n\nbody\n"
        );
        assert!(clean_message(&["  \n ".to_owned()]).is_err());
    }
}
