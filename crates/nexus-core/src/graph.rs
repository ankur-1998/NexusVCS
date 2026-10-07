//! The commit graph: reading commits and walking their parents.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::{Commit, ObjectKind};
use crate::odb::ObjectStore;

pub fn read_commit(store: &ObjectStore, id: &ObjectId) -> Result<Commit> {
    let body = store.read_kind(id, ObjectKind::Commit)?;
    Commit::decode(&body).map_err(|reason| Error::CorruptObject { id: *id, reason })
}

/// Commits reachable from `from`, each once: only first parents (the history
/// of one branch, newest first) or all parents (breadth first).
pub fn walk_commits(store: &ObjectStore, from: ObjectId, first_parent: bool) -> Walk<'_> {
    walk_from(store, &[from], first_parent)
}

/// Like [`walk_commits`], from several starting commits at once: each
/// reachable commit is read once, however many of them reach it.
pub fn walk_from<'a>(store: &'a ObjectStore, tips: &[ObjectId], first_parent: bool) -> Walk<'a> {
    let mut seen = HashSet::new();
    let queue = tips
        .iter()
        .copied()
        .filter(|tip| seen.insert(*tip))
        .collect();
    Walk {
        store,
        first_parent,
        queue,
        seen,
    }
}

pub struct Walk<'a> {
    store: &'a ObjectStore,
    first_parent: bool,
    queue: VecDeque<ObjectId>,
    seen: HashSet<ObjectId>,
}

impl Iterator for Walk<'_> {
    type Item = Result<(ObjectId, Commit)>;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.queue.pop_front()?;
        let commit = match read_commit(self.store, &id) {
            Ok(commit) => commit,
            Err(err) => {
                self.queue.clear();
                return Some(Err(err));
            }
        };
        let parents = if self.first_parent {
            &commit.parents[..commit.parents.len().min(1)]
        } else {
            &commit.parents[..]
        };
        for parent in parents {
            if self.seen.insert(*parent) {
                self.queue.push_back(*parent);
            }
        }
        Some(Ok((id, commit)))
    }
}

/// Whether `ancestor` is `descendant` or reachable from it through parents.
pub fn is_ancestor(store: &ObjectStore, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
    for step in walk_commits(store, descendant, false) {
        if step?.0 == ancestor {
            return Ok(true);
        }
    }
    Ok(false)
}

/// For every commit reachable from `tips`, the commits that list it as a
/// parent (in the order they were found).
pub fn children(
    store: &ObjectStore,
    tips: &[ObjectId],
) -> Result<HashMap<ObjectId, Vec<ObjectId>>> {
    let mut children: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for step in walk_from(store, tips, false) {
        let (id, commit) = step?;
        for parent in commit.parents {
            let list = children.entry(parent).or_default();
            // A commit can name the same parent twice.
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Signature;
    use crate::time::Timestamp;

    fn commit(store: &ObjectStore, parents: &[ObjectId], message: &str) -> ObjectId {
        let commit = Commit {
            tree: ObjectId::from_bytes([0; 32]),
            parents: parents.to_vec(),
            author: Signature {
                name: "A".into(),
                email: "a@example.com".into(),
                time: Timestamp::parse("0 +0000").unwrap(),
            },
            message: format!("{message}\n"),
        };
        store.write(ObjectKind::Commit, &commit.encode()).unwrap()
    }

    #[test]
    fn walks_ancestry_and_children() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        //  a - b - d (merge of b and c)
        //   \- c -/
        let a = commit(&store, &[], "a");
        let b = commit(&store, &[a], "b");
        let c = commit(&store, &[a], "c");
        let d = commit(&store, &[b, c], "d");

        let first: Vec<_> = walk_commits(&store, d, true)
            .map(|s| s.unwrap().0)
            .collect();
        assert_eq!(first, [d, b, a]);
        let all: Vec<_> = walk_commits(&store, d, false)
            .map(|s| s.unwrap().0)
            .collect();
        assert_eq!(all, [d, b, c, a]);

        assert!(is_ancestor(&store, a, d).unwrap());
        assert!(is_ancestor(&store, c, d).unwrap());
        assert!(is_ancestor(&store, d, d).unwrap());
        assert!(!is_ancestor(&store, d, a).unwrap());
        assert!(!is_ancestor(&store, b, c).unwrap());

        let kids = children(&store, &[d]).unwrap();
        assert_eq!(kids[&a], [b, c]);
        assert_eq!(kids[&b], [d]);
        assert!(!kids.contains_key(&d));
    }
}
