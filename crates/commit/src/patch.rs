//! Byte-level splitting of a single-file unified diff into its header and
//! hunks, and reassembly of a hunk subset. Operates on raw bytes — the patch
//! is produced by `git diff --output=<file>` and fed back to `git apply`, so
//! it must never round-trip through a lossy string conversion (CRLF and
//! non-UTF-8 content stay byte-exact).
//!
//! Dropping whole hunks from a patch is safe for `git apply`: each kept hunk's
//! own `@@` header still matches its body, and apply locates hunks by their
//! context lines with an offset search, so the stale line numbers of later
//! hunks don't matter.

/// Split a single-file patch into `(header, hunks)`. The header is everything
/// up to the first line starting with `@@` (the `diff --git`/`---`/`+++`
/// block); each hunk runs from its `@@` line to the next one (or EOF).
pub fn split(patch: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut header_end = patch.len();
    let mut starts: Vec<usize> = Vec::new();
    for pos in line_starts(patch) {
        if patch[pos..].starts_with(b"@@") {
            if starts.is_empty() {
                header_end = pos;
            }
            starts.push(pos);
        }
    }
    let header = patch[..header_end].to_vec();
    let mut hunks = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(patch.len());
        hunks.push(patch[start..end].to_vec());
    }
    (header, hunks)
}

/// Concatenate the header plus the hunks whose index is in `selected`
/// (out-of-range indices are ignored; order follows the original patch).
pub fn assemble(header: &[u8], hunks: &[Vec<u8>], selected: &[usize]) -> Vec<u8> {
    let mut out = header.to_vec();
    for (i, hunk) in hunks.iter().enumerate() {
        if selected.contains(&i) {
            out.extend_from_slice(hunk);
        }
    }
    out
}

/// Byte offsets where lines start (0, and after every `\n`).
fn line_starts(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        bytes
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i + 1)
            .filter(move |&i| i < bytes.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &[u8] = b"diff --git a/f.txt b/f.txt\n\
index 111..222 100644\n\
--- a/f.txt\n\
+++ b/f.txt\n\
@@ -1,3 +1,3 @@ first\n\
 ctx\n\
-old1\n\
+new1\n\
@@ -10,2 +10,3 @@ second\n\
 ctx2\n\
+added\n";

    #[test]
    fn split_separates_header_and_hunks() {
        let (header, hunks) = split(PATCH);
        assert!(header.starts_with(b"diff --git"));
        assert!(header.ends_with(b"+++ b/f.txt\n"));
        assert_eq!(hunks.len(), 2);
        assert!(hunks[0].starts_with(b"@@ -1,3 +1,3 @@ first\n"));
        assert!(hunks[0].ends_with(b"+new1\n"));
        assert!(hunks[1].starts_with(b"@@ -10,2 +10,3 @@ second\n"));
    }

    #[test]
    fn assemble_full_selection_is_byte_identical() {
        let (header, hunks) = split(PATCH);
        assert_eq!(assemble(&header, &hunks, &[0, 1]), PATCH);
    }

    #[test]
    fn assemble_subset_keeps_only_selected_hunks() {
        let (header, hunks) = split(PATCH);
        let only_second = assemble(&header, &hunks, &[1]);
        let text = String::from_utf8_lossy(&only_second);
        assert!(text.contains("second"));
        assert!(!text.contains("first"));
        assert!(text.starts_with("diff --git"));
        // Out-of-range indices are ignored.
        assert_eq!(assemble(&header, &hunks, &[1, 7]), only_second);
    }

    #[test]
    fn split_is_byte_exact_for_crlf_and_high_bytes() {
        // CRLF line endings and non-UTF-8 bytes must survive untouched.
        let patch: Vec<u8> = [
            &b"--- a/f\n+++ b/f\n"[..],
            b"@@ -1 +1 @@\n-a\xFF\r\n+b\xFE\r\n",
            b"@@ -5 +5 @@\n-c\r\n+d\r\n",
        ]
        .concat();
        let (header, hunks) = split(&patch);
        assert_eq!(hunks.len(), 2);
        assert_eq!(assemble(&header, &hunks, &[0, 1]), patch);
        let first_only = assemble(&header, &hunks, &[0]);
        assert!(first_only.ends_with(b"+b\xFE\r\n"));
    }

    #[test]
    fn patch_without_hunks_is_all_header() {
        let (header, hunks) = split(b"diff --git a/x b/x\nBinary files differ\n");
        assert!(hunks.is_empty());
        assert!(header.ends_with(b"differ\n"));
    }

    #[test]
    fn at_signs_inside_hunk_bodies_do_not_split() {
        // A context/added line beginning with `@@` can't occur (bodies are
        // prefixed with ' '/'+'/'-'), but a header-ish line mid-hunk must not
        // start a new hunk unless it starts the line with `@@`.
        let patch = b"--- a/f\n+++ b/f\n@@ -1 +1 @@\n- x @@ y\n+ z @@ w\n";
        let (_, hunks) = split(patch);
        assert_eq!(hunks.len(), 1);
    }
}
