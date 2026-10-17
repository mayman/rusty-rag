//! Manifest: the files of one commit as a map from `RepoPath` to `BlobOid`.
//! Comparing the stored manifest with the manifest of a new commit says which
//! files to (re)index and which to drop.

use std::collections::BTreeMap;

use crate::domain::oid::BlobOid;
use crate::domain::repo_path::RepoPath;
use crate::domain::tree_entry::TreeEntry;

/// Path to blob id, ordered by path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest(BTreeMap<RepoPath, BlobOid>);

/// What changed between two manifests. Each list is ordered by path. A renamed
/// file is a removal plus an addition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManifestDiff {
    pub added: Vec<RepoPath>,
    pub changed: Vec<RepoPath>,
    pub removed: Vec<RepoPath>,
}

impl ManifestDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

impl Manifest {
    pub fn new() -> Self {
        Self::default()
    }

    /// The manifest of a commit's tree listing.
    pub fn from_entries(entries: &[TreeEntry]) -> Self {
        entries
            .iter()
            .map(|entry| (entry.path.clone(), entry.blob.clone()))
            .collect()
    }

    /// Adds or replaces the blob for `path`.
    pub fn insert(&mut self, path: RepoPath, blob: BlobOid) {
        self.0.insert(path, blob);
    }

    pub fn remove(&mut self, path: &RepoPath) -> Option<BlobOid> {
        self.0.remove(path)
    }

    pub fn get(&self, path: &RepoPath) -> Option<&BlobOid> {
        self.0.get(path)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Entries in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&RepoPath, &BlobOid)> {
        self.0.iter()
    }

    /// Changes needed to go from `old` to `new`.
    pub fn diff(old: &Manifest, new: &Manifest) -> ManifestDiff {
        let mut diff = ManifestDiff::default();
        for (path, blob) in &new.0 {
            match old.0.get(path) {
                None => diff.added.push(path.clone()),
                Some(previous) if previous != blob => diff.changed.push(path.clone()),
                Some(_) => {}
            }
        }
        diff.removed = old
            .0
            .keys()
            .filter(|path| !new.0.contains_key(*path))
            .cloned()
            .collect();
        diff
    }
}

impl FromIterator<(RepoPath, BlobOid)> for Manifest {
    fn from_iter<I: IntoIterator<Item = (RepoPath, BlobOid)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{Manifest, ManifestDiff};
    use crate::domain::oid::BlobOid;
    use crate::domain::repo_path::RepoPath;

    fn path(text: &str) -> RepoPath {
        RepoPath::parse(text).unwrap()
    }

    fn blob(n: u8) -> BlobOid {
        BlobOid::parse(&format!("{n:02x}").repeat(20)).unwrap()
    }

    fn manifest(entries: &[(&str, u8)]) -> Manifest {
        entries.iter().map(|(p, n)| (path(p), blob(*n))).collect()
    }

    #[test]
    fn identical_manifests_have_an_empty_diff() {
        let m = manifest(&[("a", 1), ("b", 2)]);
        assert!(Manifest::diff(&m, &m.clone()).is_empty());
    }

    #[test]
    fn two_empty_manifests_have_an_empty_diff() {
        assert!(Manifest::diff(&Manifest::new(), &Manifest::new()).is_empty());
    }

    #[test]
    fn new_paths_are_added() {
        let diff = Manifest::diff(&Manifest::new(), &manifest(&[("b", 2), ("a", 1)]));
        assert_eq!(diff.added, [path("a"), path("b")]);
        assert!(diff.changed.is_empty() && diff.removed.is_empty());
    }

    #[test]
    fn missing_paths_are_removed() {
        let diff = Manifest::diff(&manifest(&[("a", 1), ("b", 2)]), &manifest(&[("a", 1)]));
        assert_eq!(diff.removed, [path("b")]);
        assert!(diff.added.is_empty() && diff.changed.is_empty());
    }

    #[test]
    fn paths_with_another_blob_are_changed() {
        let diff = Manifest::diff(
            &manifest(&[("a", 1), ("b", 2)]),
            &manifest(&[("a", 1), ("b", 3)]),
        );
        assert_eq!(diff.changed, [path("b")]);
        assert!(diff.added.is_empty() && diff.removed.is_empty());
    }

    #[test]
    fn a_rename_is_a_removal_plus_an_addition() {
        let diff = Manifest::diff(&manifest(&[("old.rs", 1)]), &manifest(&[("new.rs", 1)]));
        assert_eq!(
            diff,
            ManifestDiff {
                added: vec![path("new.rs")],
                changed: vec![],
                removed: vec![path("old.rs")],
            }
        );
    }

    #[test]
    fn insert_replaces_and_remove_returns_the_old_blob() {
        let mut m = manifest(&[("a", 1)]);
        m.insert(path("a"), blob(2));
        assert_eq!(m.get(&path("a")), Some(&blob(2)));
        assert_eq!(m.remove(&path("a")), Some(blob(2)));
        assert!(m.is_empty());
    }
}
