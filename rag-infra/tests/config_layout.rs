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
