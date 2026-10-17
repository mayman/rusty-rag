//! What `rag add <url|path>` was given: a remote git address, or a local
//! repository path that must be made absolute before it becomes a `GitLocation`.

use std::path::Path;

use rag_core::application::error::{AppError, Scrubbed};

/// True for text the user meant as a remote address: a URL (`scheme://`) or an
/// scp-like `user@host:path`. Anything else is a local path.
pub fn looks_remote(target: &str) -> bool {
    if target.contains("://") {
        return true;
    }
    match (target.find('@'), target.find(':')) {
        (Some(at), Some(colon)) => at < colon && !target[..at].contains(['/', '\\']),
        _ => false,
    }
}

/// The location text to register for `target`: remote addresses unchanged, local
/// paths as absolute paths of existing git repositories.
pub fn resolve_target(target: &str) -> Result<String, AppError> {
    if looks_remote(target) {
        return Ok(target.to_owned());
    }
    let usage = |message: String| AppError::Usage(Scrubbed::from(message));
    let absolute = std::fs::canonicalize(target).map_err(|_| {
        usage(format!(
            "{target:?} is neither a URL nor an existing directory"
        ))
    })?;
    if !absolute.is_dir() {
        return Err(usage(format!("{target:?} is not a directory")));
    }
    let text = absolute
        .to_str()
        .ok_or_else(|| usage(format!("{target:?} is not a valid UTF-8 path")))?;
    let text = strip_verbatim_prefix(text);
    if !is_git_repository(Path::new(&text)) {
        return Err(usage(format!(
            "{target:?} is not a git repository (no .git found); run `git init` and commit first"
        )));
    }
    Ok(text)
}

/// A work tree (`.git` directory or file) or a bare repository.
fn is_git_repository(path: &Path) -> bool {
    path.join(".git").exists() || (path.join("HEAD").is_file() && path.join("objects").is_dir())
}

/// Removes the `\\?\` prefix that `canonicalize` adds on Windows: `\\?\C:\x` becomes
/// `C:\x` and `\\?\UNC\server\share` becomes `\\server\share`. Other text is returned
/// unchanged.
pub fn strip_verbatim_prefix(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) if is_drive_path(rest) => rest.to_owned(),
        _ => path.to_owned(),
    }
}

fn is_drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::{looks_remote, resolve_target, strip_verbatim_prefix};

    #[test]
    fn urls_and_scp_addresses_are_remote() {
        for target in [
            "https://github.com/o/r",
            "ssh://git@host/o/r.git",
            "file:///srv/repo",
            "git@github.com:o/r.git",
        ] {
            assert!(looks_remote(target), "{target}");
        }
    }

    #[test]
    fn paths_are_not_remote() {
        for target in [".", "../repo", r"C:\work\repo", "/srv/repo", "C:/work/repo"] {
            assert!(!looks_remote(target), "{target}");
        }
    }

    #[test]
    fn the_verbatim_prefix_is_stripped_from_drive_and_unc_paths() {
        assert_eq!(strip_verbatim_prefix(r"\\?\C:\work\repo"), r"C:\work\repo");
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\repo"),
            r"\\server\share\repo"
        );
    }

    #[test]
    fn other_paths_are_left_alone() {
        for path in [
            r"C:\work",
            "/srv/repo",
            r"\\server\share",
            r"\\?\Volume{1}\x",
        ] {
            assert_eq!(strip_verbatim_prefix(path), path);
        }
    }

    #[test]
    fn a_remote_target_is_passed_through() {
        assert_eq!(
            resolve_target("https://github.com/o/r").unwrap(),
            "https://github.com/o/r"
        );
    }

    #[test]
    fn a_missing_path_is_a_usage_error() {
        let error = resolve_target("definitely/not/here").unwrap_err();
        assert_eq!(error.code().exit_code(), 2);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_a_usage_error_naming_git() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_target(dir.path().to_str().unwrap()).unwrap_err();
        assert_eq!(error.code().exit_code(), 2);
        assert!(
            error.to_string().contains("not a git repository"),
            "{error}"
        );
    }

    #[test]
    fn a_repository_directory_resolves_to_an_absolute_path_without_the_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let resolved = resolve_target(dir.path().to_str().unwrap()).unwrap();
        assert!(std::path::Path::new(&resolved).is_absolute(), "{resolved}");
        assert!(!resolved.starts_with(r"\\?\"), "{resolved}");
    }
}
