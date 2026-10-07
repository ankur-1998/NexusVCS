//! `nexus config [--global] <key> [value]`

use std::path::PathBuf;

use super::{Ctx, Output, Style};
use crate::config::{self, ConfigFiles, Key};
use crate::error::{Error, Result};
use crate::oplog::Lock;
use crate::repo::Repo;

pub fn run(
    global: bool,
    key: Key,
    value: Option<&str>,
    ctx: &Ctx,
    command_line: &str,
    out: &mut Output,
) -> Result<()> {
    let global_path = || {
        ctx.global_config.clone().ok_or_else(|| {
            Error::Invalid(
                "there's no global config location on this machine (set NEXUS_CONFIG_GLOBAL)"
                    .to_owned(),
            )
        })
    };
    if let Some(value) = value {
        // Config isn't part of the op log, but its read-modify-write still
        // needs a lock, or two concurrent writes could lose one.
        let (path, lock_path) = if global {
            let path = global_path()?;
            let lock_path = PathBuf::from(format!("{}.lock", path.display()));
            (path, lock_path)
        } else {
            let repo = Repo::discover(&ctx.cwd)?;
            (repo.config_path(), repo.lock_path())
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let _lock = Lock::acquire(&lock_path, command_line)?;
        config::set(&path, key, value)?;
        out.styled(
            Style::Muted,
            format!("{} set in {}", key.name(), path.display()),
        );
        return Ok(());
    }
    let files = if global {
        ConfigFiles {
            repo: None,
            global: Some(global_path()?),
        }
    } else {
        // Outside a repository, only the global config applies.
        let repo = Repo::discover(&ctx.cwd).ok().map(|repo| repo.config_path());
        ConfigFiles {
            repo,
            global: ctx.global_config.clone(),
        }
    };
    let (value, _) = files
        .lookup(key)?
        .ok_or_else(|| Error::Invalid(format!("{} isn't set", key.name())))?;
    out.line(value);
    Ok(())
}
