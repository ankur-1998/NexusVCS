//! Line diffs (spec §7 Phase 2), on top of `imara-diff`: the histogram
//! algorithm by default, Myers on request, grouped into hunks with context.
//!
//! Lines are bytes, not text: files don't have to be UTF-8, and line endings
//! are part of each line, so a change from CRLF to LF is a real change.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::ops::Range;
use std::path::PathBuf;

use imara_diff::{Algorithm as Engine, Diff, InternedInput, SliderHeuristic, Token};

use crate::content::{self, CHUNKED_THRESHOLD};
use crate::error::{Error, IoResultExt as _, Result};
use crate::hash::ObjectId;
use crate::object::{self, Chunk, ChunkList, ObjectKind};
use crate::odb::ObjectStore;
use crate::path::RepoPath;
use crate::worktree::Version;

/// Files with a NUL byte in their first 8 KB are treated as binary. The
/// same 8,000 bytes Git looks at.
pub const BINARY_PROBE_LEN: usize = 8000;

/// Lines of unchanged context around each change.
pub const DEFAULT_CONTEXT: u32 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Algorithm {
    #[default]
    Histogram,
    Myers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Removed,
    Added,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// The line's bytes without its `\n`.
    pub text: Vec<u8>,
    /// False for a last line that doesn't end with `\n`.
    pub newline: bool,
}

/// A run of changes with context, as in a unified diff. Line numbers are
/// 1-based; an empty side's start is the line before it (0 at the top).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<DiffLine>,
    /// Git's default hunk-header context: the nearest line above the hunk,
    /// in the old file, that starts with a letter, `_` or `$`.
    pub function: Option<Vec<u8>>,
}

impl Hunk {
    /// The `@@ -a,b +c,d @@` header.
    pub fn header(&self) -> String {
        format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_len),
            range(self.new_start, self.new_len)
        )
    }
}

fn range(start: u32, len: u32) -> String {
    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

/// Whether `data` looks binary: a NUL byte in its first [`BINARY_PROBE_LEN`] bytes.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_PROBE_LEN)].contains(&0)
}

/// The hunks that turn `old` into `new`.
pub fn diff_lines(old: &[u8], new: &[u8], algorithm: Algorithm, context: u32) -> Vec<Hunk> {
    let input = InternedInput::new(old, new);
    let engine = match algorithm {
        Algorithm::Histogram => Engine::Histogram,
        Algorithm::Myers => Engine::Myers,
    };
    let mut diff = Diff::compute(engine, &input);
    diff.postprocess_with_heuristic(&input, GitIndentHeuristic::new(&input));
    let changes: Vec<imara_diff::Hunk> = diff.hunks().collect();
    let old_lines: Vec<&[u8]> = input.before.iter().map(|&t| input.interner[t]).collect();
    let new_lines: Vec<&[u8]> = input.after.iter().map(|&t| input.interner[t]).collect();
    let old_count = u32::try_from(old_lines.len()).unwrap_or(u32::MAX);

    // Changes whose context would touch or overlap share a hunk.
    let mut groups: Vec<Vec<imara_diff::Hunk>> = Vec::new();
    for change in changes {
        match groups.last_mut() {
            Some(group)
                if change.before.start - group.last().expect("groups aren't empty").before.end
                    <= context.saturating_mul(2) =>
            {
                group.push(change);
            }
            _ => groups.push(vec![change]),
        }
    }

    groups
        .into_iter()
        .map(|group| {
            let first = group.first().expect("groups aren't empty");
            let last = group.last().expect("groups aren't empty");
            let lead = first.before.start.min(context);
            let old_from = first.before.start - lead;
            let new_from = first.after.start - lead;
            let trail = (old_count - last.before.end).min(context);
            let old_to = last.before.end + trail;
            let new_to = last.after.end + trail;

            let mut lines = Vec::new();
            let mut old_pos = old_from;
            for change in &group {
                push_lines(
                    &mut lines,
                    LineKind::Context,
                    &old_lines,
                    old_pos,
                    change.before.start,
                );
                push_lines(
                    &mut lines,
                    LineKind::Removed,
                    &old_lines,
                    change.before.start,
                    change.before.end,
                );
                push_lines(
                    &mut lines,
                    LineKind::Added,
                    &new_lines,
                    change.after.start,
                    change.after.end,
                );
                old_pos = change.before.end;
            }
            push_lines(&mut lines, LineKind::Context, &old_lines, old_pos, old_to);

            let old_len = old_to - old_from;
            let new_len = new_to - new_from;
            Hunk {
                old_start: if old_len == 0 { old_from } else { old_from + 1 },
                old_len,
                new_start: if new_len == 0 { new_from } else { new_from + 1 },
                new_len,
                lines,
                function: function_line(&old_lines, old_from),
            }
        })
        .collect()
}

/// Git's `def_ff`: the nearest line before `before` that starts with an
/// ASCII letter, `_` or `$`, cut to 80 bytes, without trailing whitespace.
fn function_line(lines: &[&[u8]], before: u32) -> Option<Vec<u8>> {
    let line = lines[..before as usize].iter().rev().find(|line| {
        line.first()
            .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
    })?;
    let mut line = &line[..line.len().min(80)];
    while let [rest @ .., last] = line
        && last.is_ascii_whitespace()
    {
        line = rest;
    }
    Some(line.to_vec())
}

fn push_lines(out: &mut Vec<DiffLine>, kind: LineKind, source: &[&[u8]], from: u32, to: u32) {
    for line in &source[from as usize..to as usize] {
        let (text, newline) = match line.strip_suffix(b"\n") {
            Some(text) => (text, true),
            None => (*line, false),
        };
        out.push(DiffLine {
            kind,
            text: text.to_vec(),
            newline,
        });
    }
}

/// Applies `hunks` to `old`, checking that every context and removed line
/// matches. Returns the new content. The inverse of [`diff_lines`].
pub fn apply(old: &[u8], hunks: &[Hunk]) -> Result<Vec<u8>, String> {
    let old_lines: Vec<&[u8]> = imara_diff::sources::byte_lines(old).collect();
    let mut out = Vec::with_capacity(old.len());
    let mut pos = 0usize;
    let emit = |out: &mut Vec<u8>, line: &DiffLine| {
        out.extend_from_slice(&line.text);
        if line.newline {
            out.push(b'\n');
        }
    };
    for hunk in hunks {
        let start = if hunk.old_len == 0 {
            hunk.old_start as usize
        } else {
            hunk.old_start as usize - 1
        };
        if start < pos || start > old_lines.len() {
            return Err(format!(
                "hunk {} is out of order or past the end",
                hunk.header()
            ));
        }
        for line in &old_lines[pos..start] {
            out.extend_from_slice(line);
        }
        pos = start;
        for line in &hunk.lines {
            match line.kind {
                LineKind::Context | LineKind::Removed => {
                    let expected = old_lines.get(pos).ok_or("a hunk runs past the end")?;
                    let mut actual = line.text.clone();
                    if line.newline {
                        actual.push(b'\n');
                    }
                    if actual != *expected {
                        return Err(format!(
                            "hunk {} doesn't match line {}",
                            hunk.header(),
                            pos + 1
                        ));
                    }
                    pos += 1;
                    if line.kind == LineKind::Context {
                        emit(&mut out, line);
                    }
                }
                LineKind::Added => emit(&mut out, line),
            }
        }
    }
    for line in &old_lines[pos..] {
        out.extend_from_slice(line);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// File diffs

/// One side of a file diff: stored content, or a file on disk.
#[derive(Clone, Debug)]
pub enum Side {
    Stored(Version),
    Working { fs_path: PathBuf, version: Version },
}

impl Side {
    pub fn version(&self) -> Version {
        match self {
            Self::Stored(version) | Self::Working { version, .. } => *version,
        }
    }
}

/// How a file's content changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text {
        hunks: Vec<Hunk>,
        /// The only differences are CRLF versus LF line endings.
        line_endings_only: bool,
    },
    Binary,
    /// One side is a chunked file: summarized rather than diffed line by line.
    Large {
        old_size: u64,
        new_size: u64,
        changed_chunks: usize,
        total_chunks: usize,
    },
    /// Same content; only the kind (executable bit) changed.
    Same,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: RepoPath,
    pub old: Option<Version>,
    pub new: Option<Version>,
    pub content: Content,
}

/// The diff of one file between two sides (`None`: the file doesn't exist there).
pub fn file_diff(
    store: &ObjectStore,
    path: &RepoPath,
    old: Option<&Side>,
    new: Option<&Side>,
    algorithm: Algorithm,
    context: u32,
) -> Result<FileDiff> {
    let old_version = old.map(Side::version);
    let new_version = new.map(Side::version);
    let compared = if old_version.map(|v| v.id) == new_version.map(|v| v.id) {
        Content::Same
    } else if [old, new]
        .into_iter()
        .flatten()
        .map(|side| is_large(store, side))
        .collect::<Result<Vec<_>>>()?
        .contains(&true)
    {
        let old_chunks = old
            .map(|side| chunks(store, side))
            .transpose()?
            .unwrap_or_default();
        let new_chunks = new
            .map(|side| chunks(store, side))
            .transpose()?
            .unwrap_or_default();
        let known: HashSet<ObjectId> = old_chunks.chunks.iter().map(|chunk| chunk.id).collect();
        Content::Large {
            old_size: old_chunks.size,
            new_size: new_chunks.size,
            changed_chunks: new_chunks
                .chunks
                .iter()
                .filter(|chunk| !known.contains(&chunk.id))
                .count(),
            total_chunks: new_chunks.chunks.len(),
        }
    } else {
        let old_bytes = old
            .map(|side| load(store, side))
            .transpose()?
            .unwrap_or_default();
        let new_bytes = new
            .map(|side| load(store, side))
            .transpose()?
            .unwrap_or_default();
        if is_binary(&old_bytes) || is_binary(&new_bytes) {
            Content::Binary
        } else {
            let line_endings_only =
                old_bytes != new_bytes && crlf_to_lf(&old_bytes) == crlf_to_lf(&new_bytes);
            Content::Text {
                hunks: diff_lines(&old_bytes, &new_bytes, algorithm, context),
                line_endings_only,
            }
        }
    };
    Ok(FileDiff {
        path: path.clone(),
        old: old_version,
        new: new_version,
        content: compared,
    })
}

fn is_large(store: &ObjectStore, side: &Side) -> Result<bool> {
    Ok(match side {
        Side::Stored(version) => store.read(&version.id)?.kind == ObjectKind::Chunked,
        Side::Working { fs_path, .. } => {
            fs::metadata(fs_path).at(fs_path)?.len() >= CHUNKED_THRESHOLD
        }
    })
}

/// A side's content as chunks. A small blob counts as one chunk.
fn chunks(store: &ObjectStore, side: &Side) -> Result<ChunkList> {
    match side {
        Side::Stored(version) => {
            let object = store.read(&version.id)?;
            match object.kind {
                ObjectKind::Chunked => {
                    ChunkList::decode(&object.body).map_err(|reason| Error::CorruptObject {
                        id: version.id,
                        reason,
                    })
                }
                _ => Ok(ChunkList {
                    size: object.body.len() as u64,
                    chunks: vec![Chunk {
                        id: version.id,
                        len: object.body.len() as u64,
                    }],
                }),
            }
        }
        Side::Working { fs_path, .. } => {
            if fs::metadata(fs_path).at(fs_path)?.len() >= CHUNKED_THRESHOLD {
                content::chunk_list_of_file(fs_path)
            } else {
                let data = fs::read(fs_path).at(fs_path)?;
                let id = object::id_of(ObjectKind::Blob, &data);
                Ok(ChunkList {
                    size: data.len() as u64,
                    chunks: vec![Chunk {
                        id,
                        len: data.len() as u64,
                    }],
                })
            }
        }
    }
}

fn load(store: &ObjectStore, side: &Side) -> Result<Vec<u8>> {
    match side {
        Side::Stored(version) => store.read_kind(&version.id, ObjectKind::Blob),
        Side::Working { fs_path, .. } => fs::read(fs_path).at(fs_path),
    }
}

fn crlf_to_lf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut bytes = data.iter().peekable();
    while let Some(&byte) = bytes.next() {
        if byte == b'\r' && bytes.peek() == Some(&&b'\n') {
            continue;
        }
        out.push(byte);
    }
    out
}

// ---------------------------------------------------------------------------
// Where a slidable change goes

/// Git's indent heuristic (`diff.indentHeuristic`, on by default since Git
/// 2.14), so that a change that could sit at several positions (an inserted
/// paragraph next to blank lines, say) lands where Git puts it. imara-diff
/// has its own version, but 0.2.0 miscounts the blank lines that end a file,
/// so this one follows Git's `measure_split` and `score_add_split` directly.
struct GitIndentHeuristic {
    /// Each interned line's indent, by token: -1 for a blank line.
    indents: Vec<i32>,
}

/// How far a change may be slid looking for a better place.
const MAX_SLIDING: u32 = 100;
/// Blank lines counted around a split, at most.
const MAX_BLANKS: i32 = 20;
/// Indents are counted up to this many columns.
const MAX_INDENT: i32 = 200;

const START_OF_FILE_PENALTY: i32 = 1;
const END_OF_FILE_PENALTY: i32 = 21;
const TOTAL_BLANK_WEIGHT: i32 = -30;
const POST_BLANK_WEIGHT: i32 = 6;
const RELATIVE_INDENT_PENALTY: i32 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i32 = 10;
const RELATIVE_OUTDENT_PENALTY: i32 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i32 = 17;
const RELATIVE_DEDENT_PENALTY: i32 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i32 = 17;
const INDENT_WEIGHT: i32 = 60;

impl GitIndentHeuristic {
    fn new(input: &InternedInput<&[u8]>) -> Self {
        let count = input.interner.num_tokens();
        let indents = (0..count)
            .map(|token| indent_of(input.interner[Token::from(token)]))
            .collect();
        Self { indents }
    }

    fn indent(&self, tokens: &[Token], line: usize) -> i32 {
        self.indents[u32::from(tokens[line]) as usize]
    }

    /// The score of splitting `tokens` just before line `split`.
    fn split_score(&self, tokens: &[Token], split: usize) -> SplitScore {
        let end_of_file = split >= tokens.len();
        let indent = if end_of_file {
            -1
        } else {
            self.indent(tokens, split)
        };

        let mut pre_blank = 0;
        let mut pre_indent = -1;
        for line in (0..split.min(tokens.len())).rev() {
            let indent = self.indent(tokens, line);
            if indent != -1 {
                pre_indent = indent;
                break;
            }
            pre_blank += 1;
            if pre_blank == MAX_BLANKS {
                pre_indent = 0;
                break;
            }
        }
        let mut post_blank = 0;
        let mut post_indent = -1;
        for line in split.saturating_add(1)..tokens.len() {
            let indent = self.indent(tokens, line);
            if indent != -1 {
                post_indent = indent;
                break;
            }
            post_blank += 1;
            if post_blank == MAX_BLANKS {
                post_indent = 0;
                break;
            }
        }

        let mut penalty = 0;
        if pre_indent == -1 && pre_blank == 0 {
            penalty += START_OF_FILE_PENALTY;
        }
        if end_of_file {
            penalty += END_OF_FILE_PENALTY;
        }
        let post_blank = if indent == -1 { 1 + post_blank } else { 0 };
        let total_blank = pre_blank + post_blank;
        penalty += TOTAL_BLANK_WEIGHT * total_blank + POST_BLANK_WEIGHT * post_blank;
        let indent = if indent == -1 { post_indent } else { indent };
        let any_blanks = total_blank != 0;
        if indent != -1 && pre_indent != -1 {
            penalty += match indent.cmp(&pre_indent) {
                Ordering::Greater if any_blanks => RELATIVE_INDENT_WITH_BLANK_PENALTY,
                Ordering::Greater => RELATIVE_INDENT_PENALTY,
                Ordering::Equal => 0,
                Ordering::Less if post_indent != -1 && post_indent > indent => {
                    if any_blanks {
                        RELATIVE_OUTDENT_WITH_BLANK_PENALTY
                    } else {
                        RELATIVE_OUTDENT_PENALTY
                    }
                }
                Ordering::Less if any_blanks => RELATIVE_DEDENT_WITH_BLANK_PENALTY,
                Ordering::Less => RELATIVE_DEDENT_PENALTY,
            };
        }
        SplitScore {
            effective_indent: indent,
            penalty,
        }
    }
}

impl SliderHeuristic for GitIndentHeuristic {
    fn best_slider_end(&mut self, tokens: &[Token], hunk: Range<u32>, earliest_end: u32) -> u32 {
        let size = hunk.end - hunk.start;
        let mut shift = earliest_end;
        if hunk.start >= 1 && hunk.start - 1 > shift {
            shift = hunk.start - 1;
        }
        if hunk.end > MAX_SLIDING && hunk.end - MAX_SLIDING > shift {
            shift = hunk.end - MAX_SLIDING;
        }
        let mut best: Option<(SplitScore, u32)> = None;
        for end in shift..=hunk.end {
            let score = self
                .split_score(tokens, end as usize)
                .plus(self.split_score(tokens, end.saturating_sub(size) as usize));
            // On a tie the lower position wins, as in Git.
            if best.is_none_or(|(best, _)| score.compare(best) <= 0) {
                best = Some((score, end));
            }
        }
        best.map_or(hunk.end, |(_, end)| end)
    }
}

#[derive(Clone, Copy)]
struct SplitScore {
    effective_indent: i32,
    penalty: i32,
}

impl SplitScore {
    fn plus(self, other: Self) -> Self {
        Self {
            effective_indent: self.effective_indent + other.effective_indent,
            penalty: self.penalty + other.penalty,
        }
    }

    /// Negative if `self` is the better split, as Git's `score_cmp`.
    fn compare(self, other: Self) -> i32 {
        let indents = match self.effective_indent.cmp(&other.effective_indent) {
            Ordering::Greater => 1,
            Ordering::Equal => 0,
            Ordering::Less => -1,
        };
        INDENT_WEIGHT * indents + (self.penalty - other.penalty)
    }
}

/// A line's indent in columns (tabs to multiples of 8), or -1 if it's blank.
fn indent_of(line: &[u8]) -> i32 {
    let mut indent = 0;
    for &byte in line {
        match byte {
            b' ' => indent += 1,
            b'\t' => indent += 8 - indent % 8,
            b'\n' | b'\r' | b'\x0b' | b'\x0c' => {}
            _ => return indent,
        }
        if indent >= MAX_INDENT {
            return MAX_INDENT;
        }
    }
    -1
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn render(hunks: &[Hunk]) -> String {
        let mut out = String::new();
        for hunk in hunks {
            out.push_str(&hunk.header());
            out.push('\n');
            for line in &hunk.lines {
                out.push(match line.kind {
                    LineKind::Context => ' ',
                    LineKind::Removed => '-',
                    LineKind::Added => '+',
                });
                out.push_str(&String::from_utf8_lossy(&line.text));
                out.push('\n');
                if !line.newline {
                    out.push_str("\\ No newline at end of file\n");
                }
            }
        }
        out
    }

    fn both(old: &str, new: &str) -> String {
        let histogram = render(&diff_lines(
            old.as_bytes(),
            new.as_bytes(),
            Algorithm::Histogram,
            3,
        ));
        let myers = render(&diff_lines(
            old.as_bytes(),
            new.as_bytes(),
            Algorithm::Myers,
            3,
        ));
        assert_eq!(
            histogram, myers,
            "the algorithms should agree on simple cases"
        );
        histogram
    }

    #[test]
    fn identical_and_empty_inputs() {
        assert_eq!(both("", ""), "");
        assert_eq!(both("a\nb\n", "a\nb\n"), "");
        assert_eq!(both("", "a\nb\n"), "@@ -0,0 +1,2 @@\n+a\n+b\n");
        assert_eq!(both("a\nb\n", ""), "@@ -1,2 +0,0 @@\n-a\n-b\n");
    }

    #[test]
    fn completely_different() {
        assert_eq!(
            both("a\nb\n", "x\ny\n"),
            "@@ -1,2 +1,2 @@\n-a\n-b\n+x\n+y\n"
        );
    }

    #[test]
    fn insert_at_start_and_end_with_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        assert_eq!(
            both(old, &format!("0\n{old}")),
            "@@ -1,3 +1,4 @@\n+0\n 1\n 2\n 3\n"
        );
        assert_eq!(
            both(old, &format!("{old}9\n")),
            "@@ -6,3 +6,4 @@\n 6\n 7\n 8\n+9\n"
        );
    }

    #[test]
    fn slidable_changes_land_where_git_puts_them() {
        // An inserted paragraph between blank lines at the end of a file.
        assert_eq!(
            both(
                "# Title\n\nIntro.\n\n",
                "# Title\n\nIntro.\n\nMore text.\n\n"
            ),
            "@@ -2,3 +2,5 @@\n \n Intro.\n \n+More text.\n+\n"
        );
        // A new function after an existing one goes after its closing brace.
        let old = "fn a() {\n    one();\n}\n";
        let new = "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n";
        assert_eq!(
            both(old, new),
            "@@ -1,3 +1,7 @@\n fn a() {\n     one();\n }\n+\n+fn b() {\n+    two();\n+}\n"
        );
    }

    #[test]
    fn distant_changes_get_separate_hunks_and_near_ones_merge() {
        let old = (1..=20)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let far = old.replace("2\n", "two\n").replace("19\n", "nineteen\n");
        assert_eq!(both(&old, &far).matches("@@ -").count(), 2);
        let near = old.replace("2\n", "two\n").replace("8\n", "eight\n");
        assert_eq!(both(&old, &near).matches("@@ -").count(), 1);
    }

    #[test]
    fn repeated_lines_and_missing_final_newline() {
        let diff = both("a\na\na\n", "a\na\na\na");
        let hunks = diff_lines(b"a\na\na\n", b"a\na\na\na", Algorithm::Histogram, 3);
        assert_eq!(apply(b"a\na\na\n", &hunks).unwrap(), b"a\na\na\na");
        assert!(diff.contains("\\ No newline at end of file"), "{diff}");
    }

    #[test]
    fn line_endings_are_part_of_the_line() {
        let hunks = diff_lines(b"a\r\nb\r\n", b"a\nb\n", Algorithm::Histogram, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0]
                .lines
                .iter()
                .filter(|l| l.kind == LineKind::Removed)
                .count(),
            2
        );
    }

    #[test]
    fn detects_binary() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text\n"));
        let mut late_nul = vec![b'a'; BINARY_PROBE_LEN];
        late_nul.push(0);
        assert!(!is_binary(&late_nul));
    }

    proptest! {
        /// Applying the diff of `a` to `b` back onto `a` reproduces `b`.
        #[test]
        fn applying_the_diff_reproduces_the_new_side(
            old in proptest::collection::vec("[abc]{0,2}", 0..30),
            new in proptest::collection::vec("[abc]{0,2}", 0..30),
            old_newline in any::<bool>(),
            new_newline in any::<bool>(),
            myers in any::<bool>(),
            context in 0u32..5,
        ) {
            let join = |lines: &[String], newline: bool| {
                let mut text = lines.join("\n");
                if newline && !lines.is_empty() {
                    text.push('\n');
                }
                text.into_bytes()
            };
            let (old, new) = (join(&old, old_newline), join(&new, new_newline));
            let algorithm = if myers { Algorithm::Myers } else { Algorithm::Histogram };
            let hunks = diff_lines(&old, &new, algorithm, context);
            prop_assert_eq!(apply(&old, &hunks).unwrap(), new);
        }
    }
}
