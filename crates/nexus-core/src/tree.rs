//! Converting between the index and tree objects. Both directions use
//! explicit stacks rather than recursion, so even absurdly deep paths can't
//! overflow the thread's stack.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

use rayon::prelude::*;

use crate::content::Sink;
use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::index::{FileKind, Index};
use crate::object::{self, EntryKind, ObjectKind, Tree, TreeEntry};
use crate::odb::ObjectStore;
use crate::path::RepoPath;

/// A directory whose entries are still being collected.
struct OpenDir {
    name: String,
    entries: Vec<TreeEntry>,
}

/// Builds the tree objects for the index, bottom-up, and returns the root
/// tree's ID. With [`Sink::HashOnly`] nothing is written.
pub fn write_index_tree(index: &Index, sink: Sink) -> Result<ObjectId> {
    write_index_tree_skipping(index, sink, &HashSet::new())
}

/// Like [`write_index_tree`], but trees in `known` are taken to be stored
/// already and aren't checked or written. With the previous index's trees as
/// `known`, staging one file writes only the few trees on its path instead of
/// checking every directory's tree. The rest are written in parallel.
pub fn write_index_tree_skipping<S: BuildHasher + Sync>(
    index: &Index,
    sink: Sink,
    known: &HashSet<ObjectId, S>,
) -> Result<ObjectId> {
    let trees = build_trees(index)?;
    let root = trees.last().expect("there is always a root tree").0;
    if let Sink::Store(store) = sink {
        trees
            .par_iter()
            .filter(|(id, _)| !known.contains(id))
            .try_for_each(|(_, body)| store.write(ObjectKind::Tree, body).map(drop))?;
    }
    Ok(root)
}

/// The ID of every tree the index describes. Nothing is written.
pub fn tree_ids(index: &Index) -> Result<HashSet<ObjectId>> {
    Ok(build_trees(index)?.into_iter().map(|(id, _)| id).collect())
}

/// Every tree the index describes, as (ID, encoded body), children before
/// their parents and the root last.
///
/// Index entries are sorted by whole path, so everything inside a directory
/// is contiguous: a directory is complete as soon as an entry outside it
/// appears. Its entries are then sorted by name, which can differ from
/// whole-path order (`src-old/x` sorts before `src/x`, but the tree `src`
/// sorts before `src-old`).
fn build_trees(index: &Index) -> Result<Vec<(ObjectId, Vec<u8>)>> {
    let mut trees = Vec::new();
    let mut stack = vec![OpenDir {
        name: String::new(),
        entries: Vec::new(),
    }];
    for (path, entry) in index.iter() {
        let names: Vec<&str> = path.components().collect();
        let (file_name, dirs) = names.split_last().expect("index paths aren't empty");
        // Close open directories that this path isn't inside.
        let shared = stack[1..]
            .iter()
            .zip(dirs)
            .take_while(|(open, name)| open.name == **name)
            .count();
        while stack.len() > shared + 1 {
            close(&mut stack, &mut trees)?;
        }
        for name in &dirs[shared..] {
            stack.push(OpenDir {
                name: (*name).to_owned(),
                entries: Vec::new(),
            });
        }
        let top = stack.last_mut().expect("the root is always open");
        top.entries.push(TreeEntry {
            name: (*file_name).to_owned(),
            kind: EntryKind::from(entry.kind),
            id: entry.id,
        });
    }
    while stack.len() > 1 {
        close(&mut stack, &mut trees)?;
    }
    let root = stack.pop().expect("the root is always open");
    finish_tree(root.entries, &mut trees)?;
    Ok(trees)
}

fn close(stack: &mut Vec<OpenDir>, trees: &mut Vec<(ObjectId, Vec<u8>)>) -> Result<()> {
    let dir = stack.pop().expect("only called with an open directory");
    let id = finish_tree(dir.entries, trees)?;
    let parent = stack.last_mut().expect("the root is always open");
    parent.entries.push(TreeEntry {
        name: dir.name,
        kind: EntryKind::Tree,
        id,
    });
    Ok(())
}

fn finish_tree(
    mut entries: Vec<TreeEntry>,
    trees: &mut Vec<(ObjectId, Vec<u8>)>,
) -> Result<ObjectId> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(pair) = entries.windows(2).find(|pair| pair[0].name == pair[1].name) {
        return Err(Error::Invalid(format!(
            "the index lists {} both as a file and as a directory. Delete .nexus/index and run \
             `nexus add .` to rebuild it",
            pair[0].name
        )));
    }
    let body = Tree { entries }.encode();
    let id = object::id_of(ObjectKind::Tree, &body);
    trees.push((id, body));
    Ok(id)
}

/// A file that differs between a tree and the index: its kind and content on
/// each side (`None`: not there).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: RepoPath,
    pub old: Option<(FileKind, ObjectId)>,
    pub new: Option<(FileKind, ObjectId)>,
}

/// The files that differ between the stored tree `tree` (`None`: no tree, as
/// on an unborn branch) and the index, sorted by path. The index's trees are
/// built in memory, and a stored tree is read only where its ID differs from
/// the index's tree at the same path, so when nothing is staged nothing is
/// read at all.
pub fn changed_files(
    store: &ObjectStore,
    tree: Option<ObjectId>,
    index: &Index,
) -> Result<Vec<FileChange>> {
    let built = build_trees(index)?;
    let index_root = built.last().expect("there is always a root tree").0;
    let index_trees: HashMap<ObjectId, Vec<u8>> = built.into_iter().collect();
    let entries = |id: Option<ObjectId>, stored: bool| -> Result<Vec<TreeEntry>> {
        let Some(id) = id else {
            return Ok(Vec::new());
        };
        let body = if stored {
            store.read_kind(&id, ObjectKind::Tree)?
        } else {
            index_trees[&id].clone()
        };
        Tree::decode(&body)
            .map(|tree| tree.entries)
            .map_err(|reason| Error::CorruptObject { id, reason })
    };
    let file = |entry: &TreeEntry| match entry.kind {
        EntryKind::File => Some((FileKind::File, entry.id)),
        EntryKind::Exec => Some((FileKind::Exec, entry.id)),
        EntryKind::Tree => None,
    };
    let subtree = |entry: Option<&TreeEntry>| {
        entry
            .filter(|entry| entry.kind == EntryKind::Tree)
            .map(|entry| entry.id)
    };

    let mut changes = Vec::new();
    let mut pending = vec![(RepoPath::root(), tree, Some(index_root))];
    while let Some((prefix, old, new)) = pending.pop() {
        if old == new {
            continue;
        }
        let old_entries = entries(old, true)?;
        let new_entries = entries(new, false)?;
        // Both lists are sorted by name, so walk them together.
        let (mut i, mut j) = (0, 0);
        while i < old_entries.len() || j < new_entries.len() {
            let order = match (old_entries.get(i), new_entries.get(j)) {
                (Some(old_entry), Some(new_entry)) => old_entry.name.cmp(&new_entry.name),
                (Some(_), None) => Ordering::Less,
                _ => Ordering::Greater,
            };
            let (old_entry, new_entry) = match order {
                Ordering::Less => (old_entries.get(i), None),
                Ordering::Greater => (None, new_entries.get(j)),
                Ordering::Equal => (old_entries.get(i), new_entries.get(j)),
            };
            if order != Ordering::Greater {
                i += 1;
            }
            if order != Ordering::Less {
                j += 1;
            }
            let name = old_entry
                .or(new_entry)
                .map_or("", |entry| entry.name.as_str());
            let path = prefix.child(name).map_err(Error::Invalid)?;
            let (old_tree, new_tree) = (subtree(old_entry), subtree(new_entry));
            if old_tree.is_some() || new_tree.is_some() {
                pending.push((path.clone(), old_tree, new_tree));
            }
            let (old_file, new_file) = (old_entry.and_then(file), new_entry.and_then(file));
            if old_file != new_file {
                changes.push(FileChange {
                    path,
                    old: old_file,
                    new: new_file,
                });
            }
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// One file in a tree, as [`flatten`] lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    pub path: RepoPath,
    pub kind: FileKind,
    pub id: ObjectId,
}

/// Every file under the tree `id`, sorted by path like the index.
pub fn flatten(store: &ObjectStore, id: &ObjectId) -> Result<Vec<TreeFile>> {
    let mut files = Vec::new();
    let mut pending = vec![(RepoPath::root(), *id)];
    while let Some((prefix, tree_id)) = pending.pop() {
        let body = store.read_kind(&tree_id, ObjectKind::Tree)?;
        let tree = Tree::decode(&body).map_err(|reason| Error::CorruptObject {
            id: tree_id,
            reason,
        })?;
        for entry in tree.entries {
            let path = prefix
                .child(&entry.name)
                .map_err(|reason| Error::CorruptObject {
                    id: tree_id,
                    reason,
                })?;
            match entry.kind {
                EntryKind::Tree => pending.push((path, entry.id)),
                EntryKind::File => files.push(TreeFile {
                    path,
                    kind: FileKind::File,
                    id: entry.id,
                }),
                EntryKind::Exec => files.push(TreeFile {
                    path,
                    kind: FileKind::Exec,
                    id: entry.id,
                }),
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;

    fn entry(store: &ObjectStore, content: &str, kind: FileKind) -> IndexEntry {
        let id = store.write(ObjectKind::Blob, content.as_bytes()).unwrap();
        IndexEntry {
            kind,
            id,
            size: 0,
            mtime_ns: 0,
        }
    }

    #[test]
    fn builds_nested_trees_and_flattens_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let mut index = Index::default();
        for (i, p) in [
            "README.md",
            "src/lib.rs",
            "src/cli/main.rs",
            "src-old/x",
            "src.rs",
        ]
        .iter()
        .enumerate()
        {
            let kind = if i == 2 {
                FileKind::Exec
            } else {
                FileKind::File
            };
            index.insert(RepoPath::parse(p).unwrap(), entry(&store, p, kind));
        }
        let root = write_index_tree(&index, Sink::Store(&store)).unwrap();
        assert_eq!(write_index_tree(&index, Sink::HashOnly).unwrap(), root);

        let root_tree = Tree::decode(&store.read_kind(&root, ObjectKind::Tree).unwrap()).unwrap();
        let names: Vec<_> = root_tree.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["README.md", "src", "src-old", "src.rs"]);

        let files = flatten(&store, &root).unwrap();
        let listed: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.kind)).collect();
        assert_eq!(
            listed,
            [
                ("README.md", FileKind::File),
                ("src-old/x", FileKind::File),
                ("src.rs", FileKind::File),
                ("src/cli/main.rs", FileKind::Exec),
                ("src/lib.rs", FileKind::File),
            ]
        );
    }

    #[test]
    fn changed_files_match_comparing_flattened_trees() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let mut before = Index::default();
        for p in [
            "a.txt",
            "dir/x",
            "dir/sub/y",
            "dir/sub/z",
            "keep/k",
            "file-then-dir",
            "exec",
        ] {
            before.insert(
                RepoPath::parse(p).unwrap(),
                entry(&store, p, FileKind::File),
            );
        }
        let tree = write_index_tree(&before, Sink::Store(&store)).unwrap();
        assert_eq!(changed_files(&store, Some(tree), &before).unwrap(), []);

        let mut after = before.clone();
        after.insert(
            RepoPath::parse("a.txt").unwrap(),
            entry(&store, "changed", FileKind::File),
        );
        after.remove(&RepoPath::parse("dir/sub/y").unwrap());
        after.remove(&RepoPath::parse("dir/sub/z").unwrap());
        after.insert(
            RepoPath::parse("dir/sub").unwrap(),
            entry(&store, "now a file", FileKind::File),
        );
        after.remove(&RepoPath::parse("file-then-dir").unwrap());
        after.insert(
            RepoPath::parse("file-then-dir/inside").unwrap(),
            entry(&store, "i", FileKind::File),
        );
        after.insert(
            RepoPath::parse("exec").unwrap(),
            entry(&store, "exec", FileKind::Exec),
        );
        after.insert(
            RepoPath::parse("new/deep/n").unwrap(),
            entry(&store, "n", FileKind::File),
        );

        let flat =
            |files: Vec<TreeFile>| -> std::collections::BTreeMap<RepoPath, (FileKind, ObjectId)> {
                files
                    .into_iter()
                    .map(|file| (file.path, (file.kind, file.id)))
                    .collect()
            };
        let old = flat(flatten(&store, &tree).unwrap());
        let new = flat(
            after
                .iter()
                .map(|(path, e)| TreeFile {
                    path: path.clone(),
                    kind: e.kind,
                    id: e.id,
                })
                .collect(),
        );
        let mut expected = Vec::new();
        let paths: std::collections::BTreeSet<&RepoPath> = old.keys().chain(new.keys()).collect();
        for path in paths {
            let (o, n) = (old.get(path).copied(), new.get(path).copied());
            if o != n {
                expected.push(FileChange {
                    path: path.clone(),
                    old: o,
                    new: n,
                });
            }
        }
        assert_eq!(changed_files(&store, Some(tree), &after).unwrap(), expected);
        assert_eq!(expected.len(), 8, "{expected:?}");
        // With no stored tree, everything is added.
        assert_eq!(
            changed_files(&store, None, &after).unwrap().len(),
            after.len()
        );
    }

    #[test]
    fn an_empty_index_is_the_empty_tree() {
        let id = write_index_tree(&Index::default(), Sink::HashOnly).unwrap();
        assert_eq!(
            id.to_string(),
            "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
        );
    }

    /// Deep paths are handled without recursion: this runs on a thread with
    /// a small stack, which would overflow if either direction recursed.
    #[test]
    fn deep_paths_dont_overflow_the_stack() {
        let worker = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let store = ObjectStore::new(dir.path());
                let deep = vec!["d"; 3000].join("/") + "/file";
                let mut index = Index::default();
                index.insert(
                    RepoPath::parse(&deep).unwrap(),
                    entry(&store, "x", FileKind::File),
                );
                let root = write_index_tree(&index, Sink::Store(&store)).unwrap();
                let files = flatten(&store, &root).unwrap();
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path.as_str(), deep);
            });
        worker.unwrap().join().unwrap();
    }
}
