//! File content in the object store: exact round trips, chunking, compression,
//! and corruption.

mod common;

use std::collections::HashSet;
use std::fs;

use common::{TestRepo, pseudo_random};
use nexus_core::content::{self, Sink};
use nexus_core::object::{ChunkList, ObjectKind};
use nexus_core::path::RepoPath;

#[test]
fn byte_exact_round_trips() {
    let repo = TestRepo::new();
    let binary: Vec<u8> = (0..=255_u8).cycle().take(4096).collect();
    let crlf = b"line one\r\nline two\r\n\r\nno newline at the end".to_vec();
    let large = pseudo_random(50 * 1024 * 1024, 1);
    let decomposed_name = "cafe\u{301}.txt";
    repo.write("binary.bin", &binary);
    repo.write("windows.txt", &crlf);
    repo.write(decomposed_name, "accented name\n");
    repo.write("media/large.bin", &large);
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "round trips"]);

    let files = repo.files_at(&repo.head_commit());
    assert_eq!(files["binary.bin"], binary);
    assert_eq!(files["windows.txt"], crlf);
    assert!(
        files["media/large.bin"] == large,
        "the 50 MiB file didn't round-trip"
    );
    // The name is stored in NFC whatever form the filesystem reported.
    assert_eq!(files["caf\u{e9}.txt"], b"accented name\n");

    // The large file is stored as chunks.
    let r = repo.repo();
    let index = r.load_index().unwrap();
    let entry = index
        .get(&RepoPath::parse("media/large.bin").unwrap())
        .unwrap();
    assert_eq!(r.odb().read(&entry.id).unwrap().kind, ObjectKind::Chunked);
    assert_eq!(entry.size, large.len() as u64);
}

#[test]
fn editing_the_middle_of_a_large_file_stores_only_a_few_new_chunks() {
    let repo = TestRepo::new();
    let mut data = pseudo_random(64 * 1024 * 1024, 2);
    repo.write("big.bin", &data);
    repo.ok(&["add", "big.bin"]);
    let chunks = |repo: &TestRepo| -> Vec<_> {
        let r = repo.repo();
        let id = r
            .load_index()
            .unwrap()
            .get(&RepoPath::parse("big.bin").unwrap())
            .unwrap()
            .id;
        let list =
            ChunkList::decode(&r.odb().read_kind(&id, ObjectKind::Chunked).unwrap()).unwrap();
        list.chunks.into_iter().map(|chunk| chunk.id).collect()
    };
    let before = chunks(&repo);

    let middle = 32 * 1024 * 1024;
    data[middle..middle + 1024 * 1024].copy_from_slice(&pseudo_random(1024 * 1024, 3));
    repo.write("big.bin", &data);
    repo.ok(&["add", "big.bin"]);
    let after = chunks(&repo);

    let old: HashSet<_> = before.iter().collect();
    let new_chunks = after.iter().filter(|id| !old.contains(id)).count();
    assert!(
        (1..=4).contains(&new_chunks),
        "{new_chunks} new chunks out of {}",
        after.len()
    );
}

#[test]
fn incompressible_data_is_stored_raw() {
    let repo = TestRepo::new();
    repo.write("random.bin", pseudo_random(100_000, 4));
    repo.write("text.txt", "compressible text ".repeat(1000));
    repo.ok(&["add", "."]);
    let r = repo.repo();
    let index = r.load_index().unwrap();
    let encoding = |path: &str| {
        let id = index.get(&RepoPath::parse(path).unwrap()).unwrap().id;
        fs::read(r.odb().path(&id)).unwrap()[0]
    };
    assert_eq!(encoding("random.bin"), 0x00);
    assert_eq!(encoding("text.txt"), 0x01);
}

#[test]
fn detects_corrupt_objects() {
    let repo = TestRepo::new();
    repo.write("a.txt", "some content worth protecting\n".repeat(10));
    repo.ok(&["add", "."]);
    let r = repo.repo();
    let id = r
        .load_index()
        .unwrap()
        .get(&RepoPath::parse("a.txt").unwrap())
        .unwrap()
        .id;
    let path = r.odb().path(&id);
    let mut stored = fs::read(&path).unwrap();
    let middle = stored.len() / 2;
    stored[middle] ^= 0x55;
    fs::write(&path, stored).unwrap();
    let output = repo.fails(&["cat-file", "-p", &id.to_string()]);
    assert!(output.contains("is corrupt"), "{output}");
}

#[test]
fn hash_object_matches_what_add_stores() {
    let repo = TestRepo::new();
    repo.write("small.txt", "hello\n");
    repo.write("large.bin", pseudo_random(9 * 1024 * 1024, 5));
    repo.ok(&["add", "."]);
    let index = repo.repo().load_index().unwrap();
    for name in ["small.txt", "large.bin"] {
        let printed = repo.ok(&["hash-object", name]);
        let staged = index.get(&RepoPath::parse(name).unwrap()).unwrap().id;
        assert_eq!(printed.trim(), staged.to_string(), "{name}");
    }
    assert_eq!(
        repo.ok(&["hash-object", "small.txt"]).trim(),
        "2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"
    );
}

#[test]
fn cat_file_shows_text_and_binary_blobs() {
    let repo = TestRepo::new();
    repo.write("text.txt", "line 1\nline 2\n");
    repo.write("bin.dat", [1_u8, 0, 2]);
    repo.ok(&["add", "."]);
    let index = repo.repo().load_index().unwrap();
    let id = |name: &str| {
        index
            .get(&RepoPath::parse(name).unwrap())
            .unwrap()
            .id
            .to_string()
    };
    assert_eq!(
        repo.ok(&["cat-file", "-p", &id("text.txt")]),
        "line 1\nline 2\n"
    );
    let binary = repo.run(&["cat-file", "-p", &id("bin.dat")]);
    assert_eq!(binary.raw.as_deref(), Some(&[1_u8, 0, 2][..]));
}

#[test]
fn cat_file_prints_blobs_byte_for_byte() {
    let repo = TestRepo::new();
    let tricky: &[u8] = b"crlf\r\nansi \x1b[31mred\x1b[0m\nlatin1 \xe9\nno newline at the end";
    repo.write("tricky.bin", tricky);
    repo.ok(&["add", "."]);
    let id = repo
        .repo()
        .load_index()
        .unwrap()
        .get(&RepoPath::parse("tricky.bin").unwrap())
        .unwrap()
        .id;
    let result = repo.run(&["cat-file", "-p", &id.to_string()]);
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.raw.as_deref(), Some(tricky));
    assert_eq!(result.lines.len(), 0);
}

#[test]
fn store_files_chunks_a_file_that_grew_past_the_threshold() {
    // The expected size says "small", as when a file grows between the walk
    // and the read. It must still come out as one chunked object, the same
    // as store_file makes.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grew.bin");
    fs::write(&path, pseudo_random(9 * 1024 * 1024, 5)).unwrap();
    let expected = content::store_file(Sink::HashOnly, &path).unwrap();
    let stored = content::store_files(Sink::HashOnly, vec![((), path, 100)]);
    assert_eq!(stored.len(), 1);
    assert_eq!(*stored[0].1.as_ref().unwrap(), expected);
}
