//! `nexus export <commit> <dir>`: a commit's files, written into an empty
//! directory with no repository around them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{Ctx, Output};
use crate::error::{Error, IoResultExt as _, Result};
use crate::graph;
use crate::path;
use crate::repo::{self, Repo};
use crate::rev;
use crate::worktree;

pub fn run(commit: &str, dir: &Path, ctx: &Ctx, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let id = rev::resolve_commit(&repo, commit)?;
    let target = repo::absolutize(&ctx.cwd, dir);
    if inside(&target, repo.dir()) {
        return Err(Error::Invalid(format!(
            "{} is inside the repository's own {} directory",
            target.display(),
            repo::DIR_NAME
        )));
    }
    match fs::read_dir(&target) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::Invalid(format!(
                    "{} isn't empty; export only writes into an empty or new directory",
                    target.display()
                )));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&target).at(&target)?;
        }
        Err(err) => return Err(err).at(&target),
    }

    let files = worktree::tree_files(repo.odb(), Some(graph::read_commit(repo.odb(), &id)?.tree))?;
    for (a, b) in path::case_collisions(files.keys()) {
        out.warning(format!(
            "{a} and {b} differ only in letter case; on a case-insensitive filesystem one replaces the other"
        ));
    }
    let mut written = 0;
    for (path, version) in &files {
        let Some(fs_path) = path.to_fs_path(&target) else {
            out.warning(format!(
                "skipped {path}: the name can't be used on this system"
            ));
            continue;
        };
        // The directory started empty, so anything already there is another
        // exported file under a name the filesystem treats as the same
        // (other letter case, or a Windows 8.3 short name).
        let in_the_way = fs_path
            .ancestors()
            .skip(1)
            .take_while(|dir| *dir != target)
            .any(|dir| fs::symlink_metadata(dir).is_ok_and(|meta| !meta.is_dir()));
        if in_the_way || fs::symlink_metadata(&fs_path).is_ok() {
            out.warning(format!(
                "skipped {path}: an exported file already has that name on this filesystem"
            ));
            continue;
        }
        worktree::write_working_file(repo.odb(), &fs_path, *version)?;
        written += 1;
    }
    let noun = if written == 1 { "file" } else { "files" };
    out.line(format!(
        "Exported {written} {noun} from {} to {}",
        id.short(),
        target.display()
    ));
    Ok(())
}

/// Whether `path` is `dir` or inside it, following symlinks and letter case
/// as the filesystem does. `path` needn't exist yet.
fn inside(path: &Path, dir: &Path) -> bool {
    let Ok(dir) = fs::canonicalize(dir) else {
        return false;
    };
    let mut existing = path.to_path_buf();
    let mut rest: Vec<PathBuf> = Vec::new();
    loop {
        if let Ok(mut full) = fs::canonicalize(&existing) {
            full.extend(rest.iter().rev());
            return full.starts_with(&dir);
        }
        let Some(name) = existing.file_name() else {
            return false;
        };
        rest.push(PathBuf::from(name));
        if !existing.pop() {
            return false;
        }
    }
}
