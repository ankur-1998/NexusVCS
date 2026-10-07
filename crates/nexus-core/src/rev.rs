//! Turning what the user typed into an object: `HEAD`, a branch or tag name,
//! a full ref name, or an object ID, which may be abbreviated to any unique
//! prefix of at least 4 hex characters.

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::refs::{BRANCH_PREFIX, TAG_PREFIX, is_valid_ref_name};
use crate::repo::Repo;

/// The shortest abbreviation accepted.
pub const MIN_PREFIX_LEN: usize = 4;

/// The commit `spec` names.
pub fn resolve_commit(repo: &Repo, spec: &str) -> Result<ObjectId> {
    let id = resolve(repo, spec, Some(ObjectKind::Commit))?;
    let kind = repo.odb().read(&id)?.kind;
    if kind != ObjectKind::Commit {
        return Err(Error::Invalid(format!("{spec} is a {kind}, not a commit")));
    }
    Ok(id)
}

/// The object of any kind `spec` names.
pub fn resolve_object(repo: &Repo, spec: &str) -> Result<ObjectId> {
    resolve(repo, spec, None)
}

/// Looks `spec` up as a ref first, then as an object ID or prefix. When a
/// prefix matches several objects, `prefer` breaks the tie if exactly one of
/// them is of that kind (as Git does for commits).
fn resolve(repo: &Repo, spec: &str, prefer: Option<ObjectKind>) -> Result<ObjectId> {
    // Ref names are stored in NFC; a name typed on macOS is often NFD.
    let normalized = crate::path::nfc(spec);
    let spec: &str = &normalized;
    let refs = repo.refs();
    if spec == "HEAD" {
        let head = refs.head()?;
        return refs
            .resolve(&head)?
            .ok_or_else(|| Error::Invalid("HEAD has no commits yet".to_owned()));
    }
    // A complete ID names its object even if a ref has the same name, as in Git.
    if spec.len() == ObjectId::HEX_LEN
        && let Some(id) = ObjectId::parse_hex(&spec.to_ascii_lowercase())
        && repo.odb().contains(&id)
    {
        return Ok(id);
    }
    if is_valid_ref_name(spec)
        && let Some(id) = refs.get(spec)?
    {
        return Ok(id);
    }
    let branch = valid_get(repo, &format!("{BRANCH_PREFIX}{spec}"))?;
    let tag = valid_get(repo, &format!("{TAG_PREFIX}{spec}"))?;
    match (branch, tag) {
        (Some(_), Some(_)) => {
            return Err(Error::Invalid(format!(
                "{spec} is both a branch and a tag; say {BRANCH_PREFIX}{spec} or {TAG_PREFIX}{spec}"
            )));
        }
        (Some(id), None) | (None, Some(id)) => return Ok(id),
        (None, None) => {}
    }

    let hex = spec.to_ascii_lowercase();
    let is_hex = hex.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex || hex.len() < MIN_PREFIX_LEN || hex.len() > ObjectId::HEX_LEN {
        return Err(Error::Invalid(format!(
            "{spec} isn't a branch, a tag, or an object ID (IDs can be shortened to {MIN_PREFIX_LEN} or more characters)"
        )));
    }
    let candidates = repo.odb().find_by_prefix(&hex)?;
    match candidates.as_slice() {
        [] => Err(Error::Invalid(format!("no object ID starts with {hex}"))),
        [only] => Ok(*only),
        many => {
            let mut described = Vec::new();
            let mut preferred = Vec::new();
            for id in many {
                let kind = repo.odb().read(id)?.kind;
                if Some(kind) == prefer {
                    preferred.push(*id);
                }
                described.push(format!("  {id} ({kind})"));
            }
            if let [only] = preferred.as_slice() {
                return Ok(*only);
            }
            Err(Error::Invalid(format!(
                "{hex} is ambiguous; it starts {} object IDs:\n{}",
                many.len(),
                described.join("\n")
            )))
        }
    }
}

fn valid_get(repo: &Repo, name: &str) -> Result<Option<ObjectId>> {
    if is_valid_ref_name(name) {
        repo.refs().get(name)
    } else {
        Ok(None)
    }
}
