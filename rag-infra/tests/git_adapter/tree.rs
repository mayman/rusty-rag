//! ls_tree and read_blobs.

use rag_core::application::error::GitError;
use rag_core::application::ports::GitClient;
use rag_core::domain::git_ref::GitRef;
use rag_core::domain::oid::{BlobOid, CommitId};

use super::support::{Sandbox, Upstream};

fn cloned(upstream: &Upstream) -> (Sandbox, CommitId) {
    let sandbox = Sandbox::new();
    let commit = sandbox
        .client
        .clone_shallow(
            &upstream.source_id(),
            &upstream.location(),
            &GitRef::parse("main").unwrap(),
        )
        .unwrap();
    (sandbox, commit)
}

#[test]
fn ls_tree_lists_files_sorted_with_blob_ids_and_sizes() {
    let upstream = Upstream::with_files(&[
        ("src/b.rs", b"12345"),
        ("a.txt", b""),
        ("docs/guide.md", b"# guide\n"),
    ]);
    let (sandbox, commit) = cloned(&upstream);

    let listing = sandbox
        .client
        .ls_tree(&upstream.source_id(), &commit)
        .unwrap();

    let summary: Vec<(&str, u64)> = listing
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.size))
        .collect();
    assert_eq!(
        summary,
        [("a.txt", 0), ("docs/guide.md", 8), ("src/b.rs", 5)]
    );
    assert_eq!(listing.skipped_unsafe, 0);
    let expected = upstream.git(&["rev-parse", "HEAD:src/b.rs"]);
    assert_eq!(listing.entries[2].blob.as_str(), expected);
}

#[test]
fn an_empty_commit_has_an_empty_listing() {
    let upstream = Upstream::new();
    upstream.commit("nothing");
    let (sandbox, commit) = cloned(&upstream);
    let listing = sandbox
        .client
        .ls_tree(&upstream.source_id(), &commit)
        .unwrap();
    assert!(listing.entries.is_empty());
    assert_eq!(listing.skipped_unsafe, 0);
}

#[test]
fn unsafe_and_non_utf8_paths_are_counted_not_listed() {
    let upstream = Upstream::new();
    upstream.commit_raw_tree(&[
        ("100644", b"ok.txt", b"fine"),
        ("100644", b"bad\xff\xfename.txt", b"not utf-8"),
        ("100644", "evil\u{202e}name.txt".as_bytes(), b"bidi"),
        ("100644", b".git", b"a file named .git"),
        ("100644", b"zero\xe2\x80\x8bwidth.txt", b"zero width"),
    ]);
    let (sandbox, commit) = cloned(&upstream);

    let listing = sandbox
        .client
        .ls_tree(&upstream.source_id(), &commit)
        .unwrap();

    let names: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, ["ok.txt"]);
    assert_eq!(listing.skipped_unsafe, 4);
}

#[test]
fn symlinks_and_submodules_are_not_listed() {
    let upstream = Upstream::new();
    upstream.commit_raw_tree(&[
        ("100644", b"real.txt", b"data"),
        ("120000", b"link", b"real.txt"),
        ("160000", b"vendor", b""),
        ("100755", b"run.sh", b"#!/bin/sh\n"),
    ]);
    let (sandbox, commit) = cloned(&upstream);

    let listing = sandbox
        .client
        .ls_tree(&upstream.source_id(), &commit)
        .unwrap();

    let names: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, ["real.txt", "run.sh"]);
    assert_eq!(listing.skipped_unsafe, 0);
}

#[test]
fn an_unknown_commit_is_not_found() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let (sandbox, _) = cloned(&upstream);
    let unknown = CommitId::parse(&"ab".repeat(20)).unwrap();
    let error = sandbox
        .client
        .ls_tree(&upstream.source_id(), &unknown)
        .unwrap_err();
    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
}

#[test]
fn ls_tree_of_a_source_that_was_never_cloned_is_not_found() {
    let upstream = Upstream::with_files(&[("a.txt", b"1")]);
    let sandbox = Sandbox::new();
    let commit = CommitId::parse(&upstream.head()).unwrap();
    let error = sandbox
        .client
        .ls_tree(&upstream.source_id(), &commit)
        .unwrap_err();
    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
}

fn blob_of(upstream: &Upstream, path: &str) -> BlobOid {
    BlobOid::parse(&upstream.git(&["rev-parse", &format!("HEAD:{path}")])).unwrap()
}

#[test]
fn read_blobs_returns_exact_bytes_including_binary_and_crlf() {
    let every_byte: Vec<u8> = (0..=255u8).cycle().take(2048).collect();
    let crlf = b"line one\r\nline two\r\n\r\n".to_vec();
    let upstream = Upstream::with_files(&[
        ("bin.dat", &every_byte),
        ("crlf.txt", &crlf),
        ("empty", b""),
    ]);
    let (sandbox, _) = cloned(&upstream);
    let wanted = [
        blob_of(&upstream, "bin.dat"),
        blob_of(&upstream, "crlf.txt"),
        blob_of(&upstream, "empty"),
    ];

    let blobs = sandbox
        .client
        .read_blobs(&upstream.source_id(), &wanted)
        .unwrap();

    assert_eq!(blobs[&wanted[0]], every_byte);
    assert_eq!(blobs[&wanted[1]], crlf);
    assert_eq!(blobs[&wanted[2]], Vec::<u8>::new());
}

#[test]
fn duplicate_requests_collapse_and_no_request_is_a_no_op() {
    let upstream = Upstream::with_files(&[("a.txt", b"hello")]);
    let (sandbox, _) = cloned(&upstream);
    let blob = blob_of(&upstream, "a.txt");

    let blobs = sandbox
        .client
        .read_blobs(&upstream.source_id(), &[blob.clone(), blob.clone()])
        .unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[&blob], b"hello");

    let none = sandbox
        .client
        .read_blobs(&upstream.source_id(), &[])
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn a_missing_blob_fails_the_whole_read_with_not_found() {
    let upstream = Upstream::with_files(&[("a.txt", b"hello")]);
    let (sandbox, _) = cloned(&upstream);
    let present = blob_of(&upstream, "a.txt");
    let absent = BlobOid::parse(&"cd".repeat(20)).unwrap();

    let error = sandbox
        .client
        .read_blobs(&upstream.source_id(), &[present, absent])
        .unwrap_err();

    assert!(matches!(error, GitError::NotFound(_)), "{error:?}");
}

#[test]
fn a_tree_id_is_not_a_blob() {
    let upstream = Upstream::with_files(&[("dir/a.txt", b"hello")]);
    let (sandbox, _) = cloned(&upstream);
    let tree = BlobOid::parse(&upstream.git(&["rev-parse", "HEAD:dir"])).unwrap();
    let error = sandbox
        .client
        .read_blobs(&upstream.source_id(), &[tree])
        .unwrap_err();
    assert!(matches!(error, GitError::Failed(_)), "{error:?}");
}

#[test]
fn many_blobs_are_read_in_batches_and_a_large_one_is_intact() {
    let names: Vec<String> = (0..700).map(|i| format!("files/f{i:04}.txt")).collect();
    let contents: Vec<Vec<u8>> = (0..700)
        .map(|i| format!("content {i}\n").into_bytes())
        .collect();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let mut files: Vec<(&str, &[u8])> = names
        .iter()
        .zip(&contents)
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    files.push(("big.bin", &big));
    let upstream = Upstream::with_files(&files);
    let (sandbox, commit) = cloned(&upstream);
    let listing = sandbox
        .client
        .ls_tree(&upstream.source_id(), &commit)
        .unwrap();
    let wanted: Vec<BlobOid> = listing.entries.iter().map(|e| e.blob.clone()).collect();

    let blobs = sandbox
        .client
        .read_blobs(&upstream.source_id(), &wanted)
        .unwrap();

    assert_eq!(blobs.len(), 701);
    let big_entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "big.bin")
        .unwrap();
    assert_eq!(blobs[&big_entry.blob], big);
    let small = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "files/f0123.txt")
        .unwrap();
    assert_eq!(blobs[&small.blob], b"content 123\n");
}
