//! `nexus init [dir]`

use std::fs;
use std::path::Path;

use super::{Ctx, Output};
use crate::config;
use crate::error::{Error, IoResultExt as _, Result};
use crate::oplog::Transaction;
use crate::platform;
use crate::repo::{self, DEFAULT_IGNORE, IGNORE_FILE, Repo};

pub fn run(dir: Option<&Path>, ctx: &Ctx, command_line: &str, out: &mut Output) -> Result<()> {
    let root = match dir {
        Some(dir) => repo::absolutize(&ctx.cwd, dir),
        None => ctx.cwd.clone(),
    };
    if let Some(existing) = Repo::find_root(&root) {
        return Err(Error::AlreadyARepository(existing));
    }
    fs::create_dir_all(&root).at(&root)?;
    let repo = Repo::create(&root)?;
    // On filesystems that report every file as executable, the executable
    // bit can't be trusted; remember that, as git's core.fileMode does.
    if platform::executable_bits_exist() && !platform::exec_bit_is_reliable(repo.dir()) {
        config::set_file_mode(&repo.config_path(), false)?;
    }
    let ignore = root.join(IGNORE_FILE);
    if !ignore.exists() {
        fs::write(&ignore, DEFAULT_IGNORE).at(&ignore)?;
    }
    // The first op records the empty starting state.
    Transaction::begin(&repo, command_line, ctx.clock.now())?.finish()?;
    out.line(format!(
        "Initialized empty NexusVCS repository in {}",
        repo.dir().display()
    ));
    Ok(())
}
