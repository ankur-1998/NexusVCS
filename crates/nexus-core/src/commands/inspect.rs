//! Commands for looking inside: `hash-object`, `cat-file -p`, `debug index`.

use std::path::Path;

use super::{Ctx, Output, Style};
use crate::content::{self, Sink};
use crate::error::{Error, Result};
use crate::index::FileKind;
use crate::repo::{self, Repo};
use crate::rev;

/// Works outside a repository too: it only reads the file.
pub fn hash_object(file: &Path, ctx: &Ctx, out: &mut Output) -> Result<()> {
    let path = repo::absolutize(&ctx.cwd, file);
    if path.is_dir() {
        return Err(Error::Invalid(format!(
            "{} is a directory, not a file",
            path.display()
        )));
    }
    let stored = content::store_file(Sink::HashOnly, &path)?;
    out.line(stored.id.to_string());
    Ok(())
}

pub fn cat_file(id: &str, ctx: &Ctx, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let id = rev::resolve_object(&repo, id)?;
    // Every body is printed exactly as stored: file content byte for byte (so
    // `nexus cat-file -p <blob> > file` reproduces the file), and for the
    // other types the stored text, which is already the readable form.
    let object = repo.odb().read(&id)?;
    out.raw(object.body);
    Ok(())
}

pub fn debug_index(ctx: &Ctx, out: &mut Output) -> Result<()> {
    let repo = Repo::discover(&ctx.cwd)?;
    let index = repo.load_index()?;
    out.styled(
        Style::Heading,
        format!("index version 1, {} entries", index.len()),
    );
    for (path, entry) in index.iter() {
        let kind = match entry.kind {
            FileKind::File => "file",
            FileKind::Exec => "exec",
        };
        out.line(format!(
            "{kind} {} {:>10} {:>20} {path}",
            entry.id, entry.size, entry.mtime_ns
        ));
    }
    Ok(())
}
