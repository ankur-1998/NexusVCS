//! Output shared by several commands: unified diffs and commit headers.

use super::{Output, Style};
use crate::diff::{Content, FileDiff, LineKind};
use crate::hash::ObjectId;
use crate::index::FileKind;
use crate::object::Commit;

/// One file's diff in unified format, with the same headers as `git diff`,
/// names quoted the way Git quotes them, so `git apply` accepts a saved text
/// diff.
pub fn file_diff(out: &mut Output, diff: &FileDiff) {
    let path = &diff.path;
    out.styled(
        Style::Heading,
        format!(
            "diff --git {} {}",
            side_name("a", path, true),
            side_name("b", path, true)
        ),
    );
    let mut index_mode = "";
    match (diff.old, diff.new) {
        (None, Some(new)) => {
            out.styled(Style::Heading, format!("new file mode {}", mode(new.kind)));
        }
        (Some(old), None) => out.styled(
            Style::Heading,
            format!("deleted file mode {}", mode(old.kind)),
        ),
        (Some(old), Some(new)) if old.kind != new.kind => {
            out.styled(Style::Heading, format!("old mode {}", mode(old.kind)));
            out.styled(Style::Heading, format!("new mode {}", mode(new.kind)));
        }
        (Some(old), Some(_)) => index_mode = mode(old.kind),
        (None, None) => {}
    }
    if diff.content == Content::Same {
        return;
    }
    let short =
        |id: Option<ObjectId>| id.map_or_else(|| "0".repeat(ObjectId::SHORT_LEN), |id| id.short());
    let index_line = format!(
        "index {}..{}",
        short(diff.old.map(|v| v.id)),
        short(diff.new.map(|v| v.id))
    );
    if index_mode.is_empty() {
        out.styled(Style::Heading, index_line);
    } else {
        out.styled(Style::Heading, format!("{index_line} {index_mode}"));
    }
    match &diff.content {
        Content::Same => {}
        Content::Binary => out.line(format!(
            "Binary files {} and {} differ",
            side_name("a", path, diff.old.is_some()),
            side_name("b", path, diff.new.is_some())
        )),
        Content::Large {
            old_size,
            new_size,
            changed_chunks,
            total_chunks,
        } => out.line(format!(
            "Large file: {} -> {}, {changed_chunks} of {total_chunks} chunks new",
            size(*old_size),
            size(*new_size)
        )),
        Content::Text {
            hunks,
            line_endings_only,
        } => {
            // Like Git, no ---/+++ lines when there are no hunks (an empty
            // file added or deleted).
            if hunks.is_empty() {
                return;
            }
            // Git ends these with a tab when the name has a space, so
            // `patch` can tell where the name ends.
            let tab = |name: &str| if name.contains(' ') { "\t" } else { "" };
            let old_name = side_name("a", path, diff.old.is_some());
            let new_name = side_name("b", path, diff.new.is_some());
            out.styled(Style::Heading, format!("--- {old_name}{}", tab(&old_name)));
            out.styled(Style::Heading, format!("+++ {new_name}{}", tab(&new_name)));
            for (i, hunk) in hunks.iter().enumerate() {
                // After the closing `@@`, Git shows the nearest line above
                // that looks like a function. `git apply` ignores that text,
                // so a CRLF-only change is noted there too.
                let mut header = hunk.header().into_bytes();
                if let Some(function) = &hunk.function {
                    header.push(b' ');
                    header.extend_from_slice(function);
                }
                if i == 0 && *line_endings_only {
                    header.extend_from_slice(b" (only the line endings changed: CRLF and LF)");
                }
                out.content(Some(Style::Hash), visible(&header), header);
                for line in &hunk.lines {
                    let (marker, style) = match line.kind {
                        LineKind::Context => (b' ', None),
                        LineKind::Removed => (b'-', Some(Style::Removed)),
                        LineKind::Added => (b'+', Some(Style::Added)),
                    };
                    let mut bytes = Vec::with_capacity(line.text.len() + 1);
                    bytes.push(marker);
                    bytes.extend_from_slice(&line.text);
                    out.content(style, visible(&bytes), bytes);
                    if !line.newline {
                        out.styled(Style::Muted, "\\ No newline at end of file");
                    }
                }
            }
        }
    }
}

/// `a/<path>` or `b/<path>` (quoted as Git quotes it), or `/dev/null`.
fn side_name(prefix: &str, path: &crate::path::RepoPath, exists: bool) -> String {
    if exists {
        crate::path::quote(&format!("{prefix}/{}", path.as_str())).into_owned()
    } else {
        "/dev/null".to_owned()
    }
}

/// A line of file content made safe for a terminal: invalid UTF-8 replaced,
/// a final CR dropped (it would only move the cursor; a CRLF-only change is
/// noted in the hunk header), and other control characters shown in caret
/// notation (`^[` for ESC), so a file can't send escape sequences to the
/// terminal. Tabs stay.
fn visible(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    super::displayable(String::from_utf8_lossy(bytes).into_owned())
}

/// The mode Git would show for a file of this kind.
fn mode(kind: FileKind) -> &'static str {
    match kind {
        FileKind::File => "100644",
        FileKind::Exec => "100755",
    }
}

/// A byte count for people: `52.0 MiB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut whole = bytes;
    let mut tenths = 0;
    while whole >= 1024 && unit + 1 < UNITS.len() {
        tenths = (whole % 1024) * 10 / 1024;
        whole /= 1024;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{whole}.{tenths} {}", UNITS[unit])
    }
}

/// A commit as `nexus log` and `nexus show` print it.
pub fn commit_header(out: &mut Output, id: &ObjectId, commit: &Commit, label: &str) {
    out.styled(Style::Hash, format!("commit {id}{label}"));
    if commit.parents.len() > 1 {
        let parents: Vec<String> = commit.parents.iter().map(ObjectId::short).collect();
        out.line(format!("Merge: {}", parents.join(" ")));
    }
    out.line(format!(
        "Author: {} <{}>",
        commit.author.name, commit.author.email
    ));
    out.line(format!("Date:   {}", commit.author.time.display()));
    out.line("");
    for line in commit.message.lines() {
        out.line(format!("    {line}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_for_people() {
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size(1023), "1023 bytes");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(52 * 1024 * 1024), "52.0 MiB");
    }
}
