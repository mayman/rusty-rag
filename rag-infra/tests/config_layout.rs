//! Data directory resolution and layout (`RUSTY_RAG_HOME`, then OS defaults).

use std::ffi::OsString;
use std::path::PathBuf;

use rag_infra::config::{ConfigError, DataLayout, HOME_ENV};

fn env_with(value: Option<&str>) -> impl Fn(&str) -> Option<OsString> {
    let value = value.map(OsString::from);
    move |name| {
        assert_eq!(name, HOME_ENV, "only the documented variable is read");
        value.clone()
    }
}

fn absolute(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

#[test]
fn the_environment_variable_overrides_the_os_default() {
    let home = absolute("rag-home");
    let default = Some(absolute("os-default"));
    let layout = DataLayout::resolve(env_with(Some(home.to_str().unwrap())), default).unwrap();
    assert_eq!(layout.root(), home);
}

#[test]
fn without_the_variable_the_os_default_directory_is_used() {
    let default = absolute("os-default");
    let layout = DataLayout::resolve(env_with(None), Some(default.clone())).unwrap();
    assert_eq!(layout.root(), default.join("rag"));
}

#[test]
fn an_empty_variable_counts_as_unset() {
    let default = absolute("os-default");
    let layout = DataLayout::resolve(env_with(Some("")), Some(default.clone())).unwrap();
    assert_eq!(layout.root(), default.join("rag"));
}

#[test]
fn no_variable_and_no_os_default_is_an_error() {
    let error = DataLayout::resolve(env_with(None), None).unwrap_err();
    assert_eq!(error, ConfigError::NoDataDirectory);
}

#[test]
fn a_relative_override_is_rejected_because_it_would_depend_on_the_cwd() {
    let error = DataLayout::resolve(env_with(Some("relative/dir")), None).unwrap_err();
    assert_eq!(error, ConfigError::RelativeHome);
}

#[test]
fn the_layout_names_index_repos_locks_and_tmp_under_the_root() {
    let root = absolute("layout-root");
    let layout = DataLayout::new(root.clone());
    assert_eq!(layout.index_db(), root.join("index.db"));
    assert_eq!(layout.repos_dir(), root.join("repos"));
    assert_eq!(layout.locks_dir(), root.join("locks"));
    assert_eq!(layout.tmp_dir(), root.join("tmp"));
}

#[test]
fn ensure_dirs_creates_the_directories_and_is_repeatable() {
    let temp = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(temp.path().join("fresh"));
    layout.ensure_dirs().unwrap();
    layout.ensure_dirs().unwrap();
    for dir in [layout.repos_dir(), layout.locks_dir(), layout.tmp_dir()] {
        assert!(dir.is_dir(), "{}", dir.display());
    }
}

#[test]
fn the_os_default_is_the_local_data_directory_not_the_roaming_one() {
    assert_eq!(DataLayout::os_default_dir(), dirs::data_local_dir());
    #[cfg(windows)]
    {
        let default = DataLayout::os_default_dir().expect("a local app data directory");
        assert!(
            !default.to_string_lossy().contains("Roaming"),
            "an index must not roam between machines: {}",
            default.display()
        );
        assert_ne!(Some(default), dirs::data_dir());
    }
}

#[test]
fn repos_and_tmp_are_siblings_so_clones_are_renames_within_one_volume() {
    let layout = DataLayout::new(absolute("sibling-root"));
    assert_eq!(layout.repos_dir().parent(), layout.tmp_dir().parent());
}

#[test]
fn a_rename_from_tmp_into_repos_is_verified_to_work() {
    let temp = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(temp.path().join("data"));
    layout.ensure_dirs().unwrap();
    layout.verify_same_volume().unwrap();
    for dir in [layout.repos_dir(), layout.tmp_dir()] {
        assert_eq!(
            std::fs::read_dir(dir).unwrap().count(),
            0,
            "the probe cleans up after itself"
        );
    }

    std::fs::remove_dir(layout.repos_dir()).unwrap();
    let error = layout.verify_same_volume().unwrap_err();
    assert!(
        matches!(error, ConfigError::CannotMoveBetweenDirs { .. }),
        "{error:?}"
    );
}

#[test]
fn many_callers_verifying_at_once_do_not_trip_over_each_other() {
    let temp = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(temp.path().join("data"));
    layout.ensure_dirs().unwrap();
    for _ in 0..20 {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| layout.verify_same_volume()))
                .collect();
            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        });
    }
}

#[test]
fn a_missing_repos_directory_is_still_reported() {
    let temp = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(temp.path().join("data"));
    layout.ensure_dirs().unwrap();
    std::fs::remove_dir(layout.repos_dir()).unwrap();
    let error = layout.verify_same_volume().unwrap_err();
    assert!(
        matches!(error, ConfigError::CannotMoveBetweenDirs { .. }),
        "{error:?}"
    );
}
