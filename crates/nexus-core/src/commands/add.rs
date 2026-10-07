//! `nexus add <path...> [--exec]`

use std::path::PathBuf;

use super::{Ctx, Output, Style};
use crate::config;
use crate::error::Result;
use crate::oplog::Transaction;
use crate::platform;
use crate::repo::{self, Repo};
use crate::stage::{self, StageOptions};

pub fn run(
    paths: &[PathBuf],
    exec: bool,
    ctx: &Ctx,
    command_line: &str,
    out: &mut Output,
) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let scopes = paths
        .iter()
        .map(|path| repo.repo_path(&repo::absolutize(&ctx.cwd, path)))
        .collect::<Result<Vec<_>>>()?;
    let options = StageOptions {
        exec,
        trust_exec_bit: platform::executable_bits_exist()
            && config::file_mode(&repo.config_path())?,
    };

    let mut tx = Transaction::begin(&repo, command_line, ctx.clock.now())?;
    let mut index = tx.index().clone();
    let report = stage::stage(&repo, &mut index, &scopes, options)?;
    if report.index_updated {
        tx.set_index(index);
    }
    tx.finish()?;

    for warning in &report.warnings {
        out.warning(warning);
    }
    for path in &report.staged {
        out.styled(Style::Added, format!("staged   {path}"));
    }
    for path in &report.removed {
        out.styled(Style::Removed, format!("removed  {path}"));
    }
    if !report.changed() {
        out.styled(
            Style::Muted,
            "nothing to stage: the index already matches these files",
        );
    }
    Ok(())
}
