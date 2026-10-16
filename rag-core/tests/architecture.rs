//! Dependency-rule test (ddd-onion-rust).
//!
//! (a) Cargo metadata: crate-level dependencies match the onion layering.
//! (b) A small text check: `domain/` and `application/` sources stay free of
//!     forbidden paths. Comments are stripped; there is no real parser, so this
//!     is a tripwire, not a proof. Cargo and `clippy.toml` do the heavy lifting.

// This test legitimately spawns `cargo` and reads source files, which
// `clippy.toml` forbids for production code in this crate.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Normal (non-dev, non-build) dependency names of `package` in `cargo metadata` output.
fn normal_dependencies(metadata: &Value, package: &str) -> BTreeSet<String> {
    metadata["packages"]
        .as_array()
        .expect("packages array")
        .iter()
        .filter(|p| p["name"] == package)
        .flat_map(|p| p["dependencies"].as_array().expect("dependencies array"))
        .filter(|d| d["kind"].is_null())
        .map(|d| d["name"].as_str().expect("dependency name").to_owned())
        .collect()
}

fn cargo_metadata() -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata should start");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON")
}

#[derive(Clone, Copy, Debug)]
enum Layer {
    Domain,
    Application,
}

const DOMAIN_FORBIDDEN: &[&str] = &[
    "crate::application",
    "super::application",
    "serde",
    "std::fs",
    "std::process",
    "std::env",
    "std::net",
    "std::thread",
    "SystemTime",
    "Instant",
];

const APPLICATION_FORBIDDEN: &[&str] = &["rusqlite", "std::fs", "std::process"];

/// Drops `//` comments (including `///` and `//!`) line by line.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One message per forbidden needle found in `source`.
fn violations(layer: Layer, source: &str) -> Vec<String> {
    let forbidden = match layer {
        Layer::Domain => DOMAIN_FORBIDDEN,
        Layer::Application => APPLICATION_FORBIDDEN,
    };
    let code = strip_line_comments(source);
    forbidden
        .iter()
        .filter(|needle| code.contains(*needle))
        .map(|needle| format!("{layer:?} must not reference `{needle}`"))
        .collect()
}

#[test]
fn normal_dependencies_ignores_dev_and_build_dependencies() {
    let metadata = serde_json::json!({"packages": [
        {"name": "a", "dependencies": [
            {"name": "serde", "kind": null},
            {"name": "serde_json", "kind": "dev"},
            {"name": "cc", "kind": "build"}
        ]},
        {"name": "b", "dependencies": [{"name": "a", "kind": null}]}
    ]});
    assert_eq!(
        normal_dependencies(&metadata, "a"),
        BTreeSet::from(["serde".to_owned()])
    );
    assert_eq!(
        normal_dependencies(&metadata, "b"),
        BTreeSet::from(["a".to_owned()])
    );
}

#[test]
fn domain_flags_each_forbidden_reference() {
    let cases = [
        "use crate::application::Search;",
        "use super::application::Search;",
        "use serde::Serialize;",
        "use std::fs::File;",
        "use std::process::Command;",
        "let v = std::env::var(\"X\");",
        "use std::net::TcpStream;",
        "use std::thread;",
        "use std::time::SystemTime;",
        "let t = Instant::now();",
    ];
    for source in cases {
        assert!(
            !violations(Layer::Domain, source).is_empty(),
            "domain should flag: {source}"
        );
    }
}

#[test]
fn application_flags_each_forbidden_reference() {
    for source in [
        "use rusqlite::Connection;",
        "use std::fs;",
        "use std::process::Command;",
    ] {
        assert!(
            !violations(Layer::Application, source).is_empty(),
            "application should flag: {source}"
        );
    }
}

#[test]
fn allowed_references_are_not_flagged() {
    assert!(violations(
        Layer::Domain,
        "use std::collections::BTreeMap;\nuse crate::domain::Source;"
    )
    .is_empty());
    assert!(violations(
        Layer::Application,
        "use serde::Serialize;\nuse crate::domain::Source;"
    )
    .is_empty());
}

#[test]
fn comments_are_not_violations() {
    let source = "// use std::fs;\n/// uses serde and SystemTime\n//! crate::application\nfn ok() {} // std::process";
    assert!(violations(Layer::Domain, source).is_empty());
    assert!(violations(Layer::Application, source).is_empty());
}

#[test]
fn rag_core_depends_only_on_allowlisted_crates() {
    let metadata = cargo_metadata();
    assert_eq!(
        normal_dependencies(&metadata, "rag-core"),
        BTreeSet::from(["serde".to_owned()]),
        "update this allowlist deliberately when rag-core gains a dependency"
    );
}

#[test]
fn crates_do_not_depend_on_outer_layers() {
    let metadata = cargo_metadata();
    let core = normal_dependencies(&metadata, "rag-core");
    assert!(
        !core.contains("rag-infra"),
        "rag-core must not depend on rag-infra"
    );
    assert!(
        !core.contains("rag-cli"),
        "rag-core must not depend on rag-cli"
    );
    let infra = normal_dependencies(&metadata, "rag-infra");
    assert!(
        !infra.contains("rag-cli"),
        "rag-infra must not depend on rag-cli"
    );
}

#[test]
fn rag_core_sources_obey_the_dependency_rule() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut failures = Vec::new();
    for (layer, dir) in [
        (Layer::Domain, "domain"),
        (Layer::Application, "application"),
    ] {
        let files = rust_files(&src.join(dir));
        assert!(
            !files.is_empty(),
            "no sources under {dir}; scan would be vacuous"
        );
        for file in files {
            let source = fs::read_to_string(&file).expect("readable source file");
            for message in violations(layer, &source) {
                failures.push(format!("{}: {message}", file.display()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "dependency rule violations:\n{}",
        failures.join("\n")
    );
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).expect("readable directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found
}
