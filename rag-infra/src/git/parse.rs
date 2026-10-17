//! Parsers for `git` plumbing output. Pure functions over bytes.

use rag_core::application::ports::TreeListing;
use rag_core::domain::oid::BlobOid;
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::tree_entry::TreeEntry;

/// Parses `git ls-tree -r -z -l` output: records `<mode> <type> <oid> <size>\t<path>`
/// separated by NUL. Only regular files (modes 100644 and 100755) are listed;
/// symlinks and submodules are dropped silently. A path that is not UTF-8 or fails
/// `RepoPath::from_git` is counted in `skipped_unsafe`.
pub fn parse_ls_tree(output: &[u8]) -> Result<TreeListing, String> {
    let mut entries = Vec::new();
    let mut skipped_unsafe = 0;
    for record in output.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or("malformed ls-tree record")?;
        let (meta, path) = (&record[..tab], &record[tab + 1..]);
        let meta = std::str::from_utf8(meta).map_err(|_| "malformed ls-tree record")?;
        let mut fields = meta.split_whitespace();
        let (mode, kind, oid, size) = (fields.next(), fields.next(), fields.next(), fields.next());
        let (Some(mode), Some(kind), Some(oid), Some(size)) = (mode, kind, oid, size) else {
            return Err("malformed ls-tree record".to_owned());
        };
        if kind != "blob" || !matches!(mode, "100644" | "100755") {
            continue;
        }
        let blob = BlobOid::parse(oid).map_err(|_| "malformed object id in ls-tree")?;
        let size: u64 = size.parse().map_err(|_| "malformed size in ls-tree")?;
        let safe = std::str::from_utf8(path)
            .ok()
            .and_then(|path| RepoPath::from_git(path).ok());
        match safe {
            Some(path) => entries.push(TreeEntry { path, blob, size }),
            None => skipped_unsafe += 1,
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(TreeListing {
        entries,
        skipped_unsafe,
    })
}

/// What `git cat-file --batch` answered for one requested object.
#[derive(Debug, PartialEq, Eq)]
pub enum BatchEntry {
    /// A blob within the size limit, with its bytes.
    Blob(Vec<u8>),
    /// A blob larger than the limit; its bytes were skipped.
    TooLarge { size: u64 },
    /// An object of another type (tree, commit, tag).
    OtherType,
    /// No such object.
    Missing,
}

/// Splits `git cat-file --batch` output for `expected` objects, in request order.
/// An entry is `<oid> <type> <size>\n<bytes>\n`, or `<oid> missing\n`. Blobs larger
/// than `max_blob_bytes` are reported, not copied.
pub fn parse_batch(
    output: &[u8],
    expected: &[BlobOid],
    max_blob_bytes: u64,
) -> Result<Vec<BatchEntry>, String> {
    let mut rest = output;
    let mut entries = Vec::with_capacity(expected.len());
    for oid in expected {
        let newline = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or("truncated cat-file output")?;
        let header =
            std::str::from_utf8(&rest[..newline]).map_err(|_| "malformed cat-file header")?;
        let fields: Vec<&str> = header.split(' ').collect();
        if fields.first().is_none_or(|name| *name != oid.as_str()) {
            return Err("cat-file answered out of order".to_owned());
        }
        let after_header = &rest[newline + 1..];
        let (kind, size) = match fields.as_slice() {
            [_, "missing" | "ambiguous"] => {
                entries.push(BatchEntry::Missing);
                rest = after_header;
                continue;
            }
            [_, kind, size] => (
                *kind,
                size.parse::<usize>()
                    .map_err(|_| "malformed cat-file size")?,
            ),
            _ => return Err("unexpected cat-file header".to_owned()),
        };
        if after_header.len() < size + 1 {
            return Err("truncated cat-file output".to_owned());
        }
        entries.push(if kind != "blob" {
            BatchEntry::OtherType
        } else if size as u64 > max_blob_bytes {
            BatchEntry::TooLarge { size: size as u64 }
        } else {
            BatchEntry::Blob(after_header[..size].to_vec())
        });
        rest = &after_header[size + 1..];
    }
    Ok(entries)
}
/// Reads the branch out of `git ls-remote --symref <url> HEAD`, whose first line is
/// `ref: refs/heads/<branch>\tHEAD`.
pub fn parse_default_branch(output: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(output).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("ref: "))?
        .split('\t')
        .next()?;
    Some(
        target
            .strip_prefix("refs/heads/")
            .unwrap_or(target)
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    fn record(mode: &str, kind: &str, size: &str, path: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{mode} {kind} {OID} {size:>7}\t").into_bytes();
        bytes.extend_from_slice(path);
        bytes.push(0);
        bytes
    }

    #[test]
    fn ls_tree_lists_regular_files_sorted_by_path() {
        let mut output = record("100644", "blob", "12", b"src/b.rs");
        output.extend(record("100755", "blob", "3", b"a.sh"));
        let listing = parse_ls_tree(&output).unwrap();
        let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.sh", "src/b.rs"]);
        assert_eq!(listing.entries[1].size, 12);
        assert_eq!(listing.skipped_unsafe, 0);
    }

    #[test]
    fn ls_tree_drops_symlinks_and_submodules_without_counting_them() {
        let mut output = record("120000", "blob", "5", b"link");
        output.extend(record("160000", "commit", "-", b"vendor/sub"));
        let listing = parse_ls_tree(&output).unwrap();
        assert!(listing.entries.is_empty());
        assert_eq!(listing.skipped_unsafe, 0);
    }

    #[test]
    fn ls_tree_counts_non_utf8_and_unsafe_paths() {
        let mut output = record("100644", "blob", "1", b"ok.txt");
        output.extend(record("100644", "blob", "1", b"bad\xff\xfe.txt"));
        output.extend(record("100644", "blob", "1", b"dir/.git/config"));
        output.extend(record("100644", "blob", "1", b"evil\xe2\x80\xaename.txt"));
        output.extend(record("100644", "blob", "1", b"..\\up.txt"));
        let listing = parse_ls_tree(&output).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.skipped_unsafe, 4);
    }

    #[test]
    fn ls_tree_keeps_tabs_spaces_and_newlines_out_of_the_way_of_the_parser() {
        let output = record("100644", "blob", "1", b"has space.txt");
        let listing = parse_ls_tree(&output).unwrap();
        assert_eq!(listing.entries[0].path.as_str(), "has space.txt");
    }

    #[test]
    fn ls_tree_rejects_garbage() {
        assert!(parse_ls_tree(b"no tab here\0").is_err());
    }

    #[test]
    fn an_empty_listing_is_valid() {
        assert_eq!(parse_ls_tree(b"").unwrap(), TreeListing::default());
    }

    const LIMIT: u64 = 1000;

    fn entry(oid: &str, kind: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{oid} {kind} {}\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn batch_returns_exact_bytes_including_newlines_and_nul() {
        let oid = BlobOid::parse(OID).unwrap();
        let body = b"a\nb\0c\r\n";
        let output = entry(OID, "blob", body);
        let entries = parse_batch(&output, &[oid], LIMIT).unwrap();
        assert_eq!(entries, [BatchEntry::Blob(body.to_vec())]);
    }

    #[test]
    fn batch_handles_empty_blobs_and_detects_truncation() {
        let oid = BlobOid::parse(OID).unwrap();
        let output = format!("{OID} blob 0\n\n").into_bytes();
        assert_eq!(
            parse_batch(&output, std::slice::from_ref(&oid), LIMIT).unwrap(),
            [BatchEntry::Blob(Vec::new())]
        );
        let cut = format!("{OID} blob 10\nabc").into_bytes();
        assert!(parse_batch(&cut, &[oid], LIMIT).is_err());
    }

    #[test]
    fn batch_reports_missing_other_and_oversized_objects_and_keeps_parsing() {
        let other = "89abcdef0123456789abcdef0123456789abcdef";
        let third = "00112233445566778899aabbccddeeff00112233";
        let fourth = "ffeeddccbbaa99887766554433221100ffeeddcc";
        let expected: Vec<BlobOid> = [OID, other, third, fourth]
            .iter()
            .map(|oid| BlobOid::parse(oid).unwrap())
            .collect();
        let mut output = format!("{OID} missing\n").into_bytes();
        output.extend(entry(other, "tree", b"tree bytes"));
        output.extend(entry(third, "blob", &vec![b'x'; LIMIT as usize + 1]));
        output.extend(entry(fourth, "blob", b"fine"));
        assert_eq!(
            parse_batch(&output, &expected, LIMIT).unwrap(),
            [
                BatchEntry::Missing,
                BatchEntry::OtherType,
                BatchEntry::TooLarge { size: LIMIT + 1 },
                BatchEntry::Blob(b"fine".to_vec()),
            ]
        );
    }

    #[test]
    fn batch_rejects_answers_out_of_order() {
        let oid = BlobOid::parse(OID).unwrap();
        let output = entry("89abcdef0123456789abcdef0123456789abcdef", "blob", b"x");
        assert!(parse_batch(&output, &[oid], LIMIT).is_err());
    }
    #[test]
    fn default_branch_is_read_from_the_symref_line() {
        let output = format!("ref: refs/heads/trunk\tHEAD\n{OID}\tHEAD\n");
        assert_eq!(
            parse_default_branch(output.as_bytes()).as_deref(),
            Some("trunk")
        );
        assert_eq!(
            parse_default_branch(format!("{OID}\tHEAD\n").as_bytes()),
            None
        );
        assert_eq!(parse_default_branch(b""), None);
    }
}
