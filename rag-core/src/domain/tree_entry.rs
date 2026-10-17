//! TreeEntry: one regular file of a commit, as listed by `git ls-tree`.

use crate::domain::oid::BlobOid;
use crate::domain::repo_path::RepoPath;

/// A file at a commit: where it is, its content identity, and its size in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: RepoPath,
    pub blob: BlobOid,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::TreeEntry;
    use crate::domain::manifest::Manifest;
    use crate::domain::oid::BlobOid;
    use crate::domain::repo_path::RepoPath;

    #[test]
    fn entries_collect_into_a_manifest_of_path_to_blob() {
        let blob = BlobOid::parse(&"cd".repeat(20)).unwrap();
        let entries = [TreeEntry {
            path: RepoPath::from_git("src/a.rs").unwrap(),
            blob: blob.clone(),
            size: 3,
        }];
        let manifest = Manifest::from_entries(&entries);
        assert_eq!(manifest.get(&entries[0].path), Some(&blob));
        assert_eq!(manifest.len(), 1);
    }
}
