//! Integration tests of `SystemGitClient` against real `git` and throwaway
//! upstream repositories created by the tests (local paths and `file://` URLs).

#[path = "git_adapter/clone.rs"]
mod clone;
#[path = "git_adapter/deadline.rs"]
mod deadline;
#[path = "git_adapter/remove.rs"]
mod remove;
#[path = "git_adapter/support.rs"]
mod support;
#[path = "git_adapter/tree.rs"]
mod tree;
#[path = "git_adapter/update.rs"]
mod update;
