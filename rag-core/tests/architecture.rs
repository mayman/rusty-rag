//! Dependency-rule test (ddd-onion-rust).
//!
//! (a) Cargo metadata: crate-level dependencies (normal and dev) match the
//!     onion layering, and every package we assert about really exists.
//! (b) A `syn`-based check: every `.rs` file under `domain/` and `application/`
//!     is parsed; all `use` trees (groups expanded, aliases resolved), all paths
//!     in expressions/types/attributes, and macro bodies are checked against the
//!     layer's forbidden roots. Comments and string literals never trigger it.

// This test legitimately spawns `cargo` and reads source files, which
// `clippy.toml` forbids for production code in this crate.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use syn::visit::Visit;
use syn::{ItemExternCrate, ItemUse, UseTree};

/// Dependency names of `package` in `cargo metadata` output for the given kind
/// (`None` = normal, `Some("dev")`, `Some("build")`). Panics if the package is
/// absent so that checks can never pass vacuously.
fn dependencies_of_kind(metadata: &Value, package: &str, kind: Option<&str>) -> BTreeSet<String> {
    let packages: Vec<&Value> = metadata["packages"]
        .as_array()
        .expect("packages array")
        .iter()
        .filter(|p| p["name"] == package)
        .collect();
    assert_eq!(
        packages.len(),
        1,
        "package `{package}` must appear exactly once in cargo metadata"
    );
    packages[0]["dependencies"]
        .as_array()
        .expect("dependencies array")
        .iter()
        .filter(|d| match kind {
            None => d["kind"].is_null(),
            Some(k) => d["kind"] == k,
        })
        .map(|d| d["name"].as_str().expect("dependency name").to_owned())
        .collect()
}

fn normal_dependencies(metadata: &Value, package: &str) -> BTreeSet<String> {
    dependencies_of_kind(metadata, package, None)
}

fn dev_dependencies(metadata: &Value, package: &str) -> BTreeSet<String> {
    dependencies_of_kind(metadata, package, Some("dev"))
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

/// Forbidden path prefixes (segment-wise). A one-segment entry is a crate root.
const DOMAIN_FORBIDDEN: &[&[&str]] = &[
    &["std", "fs"],
    &["std", "process"],
    &["std", "env"],
    &["std", "net"],
    &["std", "thread"],
    &["std", "io"],
    &["std", "time", "SystemTime"],
    &["std", "time", "Instant"],
    &["serde"],
    &["crate", "application"],
    &["super", "application"],
    &["super", "super"],
    &["rusqlite"],
    &["clap"],
    &["ignore"],
    &["dirs"],
    &["tokio"],
    &["rag_infra"],
    &["rag_cli"],
];

const APPLICATION_FORBIDDEN: &[&[&str]] = &[
    &["std", "fs"],
    &["std", "process"],
    &["std", "env"],
    &["std", "net"],
    &["std", "thread"],
    &["rusqlite"],
    &["clap"],
    &["ignore"],
    &["dirs"],
    &["crate", "infrastructure"],
    &["rag_infra"],
    &["rag_cli"],
];

type Segments = Vec<String>;

/// A path found in the source. `strict` paths (use trees, attributes, `extern
/// crate`) may be a bare crate name; ordinary paths need two segments for that,
/// so a local variable called `dirs` is not mistaken for the `dirs` crate.
struct Found {
    segments: Segments,
    strict: bool,
}

#[derive(Default)]
struct Collector {
    found: Vec<Found>,
    /// Name bound by a `use` (or `use ... as`) to the full path it refers to.
    bindings: BTreeMap<String, Segments>,
    /// Prefixes of glob imports (`use std::*;`).
    globs: Vec<Segments>,
}

impl Collector {
    fn add_use(&mut self, tree: &UseTree, prefix: &mut Segments) {
        match tree {
            UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.add_use(&p.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(n) => {
                let ident = n.ident.to_string();
                self.add_binding(prefix, &ident, &ident);
            }
            UseTree::Rename(r) => {
                self.add_binding(prefix, &r.ident.to_string(), &r.rename.to_string());
            }
            UseTree::Glob(_) => {
                self.found.push(Found {
                    segments: prefix.clone(),
                    strict: true,
                });
                self.globs.push(prefix.clone());
            }
            UseTree::Group(g) => {
                for item in &g.items {
                    self.add_use(item, prefix);
                }
            }
        }
    }

    /// `ident` is the imported name (`self` means the prefix itself); `bind` is
    /// the local name it is bound to.
    fn add_binding(&mut self, prefix: &Segments, ident: &str, bind: &str) {
        let mut full = prefix.clone();
        let bind = if ident == "self" {
            if bind == "self" {
                prefix.last().cloned().unwrap_or_default()
            } else {
                bind.to_owned() // `use x::{self as y}`
            }
        } else {
            full.push(ident.to_owned());
            bind.to_owned()
        };
        self.found.push(Found {
            segments: full.clone(),
            strict: true,
        });
        self.bindings.insert(bind, full);
    }

    /// Records every `a::b::c` identifier chain found in a token stream's text.
    /// String literals are skipped, so they can never match.
    fn scan_tokens(&mut self, tokens: &str) {
        let chars: Vec<char> = tokens.chars().collect();
        let mut i = 0;
        let mut chain: Segments = Vec::new();
        let flush = |chain: &mut Segments, found: &mut Vec<Found>| {
            if !chain.is_empty() {
                found.push(Found {
                    segments: std::mem::take(chain),
                    strict: false,
                });
            }
        };
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                flush(&mut chain, &mut self.found);
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += if chars[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
            } else if c.is_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                chain.push(chars[start..i].iter().collect());
                // Continue the chain only across `::` (possibly space-separated).
                let mut j = i;
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                if chars.get(j) == Some(&':') && chars.get(j + 1) == Some(&':') {
                    i = j + 2;
                    while i < chars.len() && chars[i] == ' ' {
                        i += 1;
                    }
                } else {
                    flush(&mut chain, &mut self.found);
                }
            } else {
                flush(&mut chain, &mut self.found);
                i += 1;
            }
        }
        flush(&mut chain, &mut self.found);
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        self.add_use(&node.tree, &mut Vec::new());
    }

    fn visit_item_extern_crate(&mut self, node: &'ast ItemExternCrate) {
        self.found.push(Found {
            segments: vec![node.ident.to_string()],
            strict: true,
        });
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        self.found.push(Found {
            segments: node.segments.iter().map(|s| s.ident.to_string()).collect(),
            strict: false,
        });
        syn::visit::visit_path(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.scan_tokens(&node.tokens.to_string());
        syn::visit::visit_macro(self, node);
    }

    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        self.found.push(Found {
            segments: node
                .path()
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect(),
            strict: true,
        });
        if let syn::Meta::List(list) = &node.meta {
            self.scan_tokens(&list.tokens.to_string());
        }
    }
}

/// Replaces a leading alias with the path it was imported from (bounded, so
/// alias cycles cannot loop).
fn resolve(bindings: &BTreeMap<String, Segments>, segments: &Segments) -> Segments {
    let mut current = segments.clone();
    for _ in 0..8 {
        match current.first().and_then(|first| bindings.get(first)) {
            Some(target) if target.first() != current.first() || target.len() > 1 => {
                let mut next = target.clone();
                next.extend(current.iter().skip(1).cloned());
                if next == current {
                    break;
                }
                current = next;
            }
            _ => break,
        }
    }
    current
}

fn starts_with_pattern(path: &[String], pattern: &[&str]) -> bool {
    path.len() >= pattern.len() && path.iter().zip(pattern).all(|(a, b)| a == b)
}

/// One message per forbidden reference found in `source`.
fn violations(layer: Layer, source: &str) -> Vec<String> {
    let forbidden = match layer {
        Layer::Domain => DOMAIN_FORBIDDEN,
        Layer::Application => APPLICATION_FORBIDDEN,
    };
    let file = match syn::parse_file(source) {
        Ok(file) => file,
        Err(error) => return vec![format!("{layer:?} source does not parse: {error}")],
    };
    let mut collector = Collector::default();
    collector.visit_file(&file);

    let mut messages = BTreeSet::new();
    for found in &collector.found {
        let mut candidates = vec![resolve(&collector.bindings, &found.segments)];
        for glob in &collector.globs {
            let mut joined = glob.clone();
            joined.extend(found.segments.iter().cloned());
            candidates.push(resolve(&collector.bindings, &joined));
        }
        for candidate in candidates {
            for pattern in forbidden {
                let bare_crate = pattern.len() == 1 && candidate.len() == 1 && !found.strict;
                if !bare_crate && starts_with_pattern(&candidate, pattern) {
                    messages.insert(format!(
                        "{layer:?} must not reference `{}` (found `{}`)",
                        pattern.join("::"),
                        found.segments.join("::")
                    ));
                }
            }
        }
    }
    messages.into_iter().collect()
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
        dev_dependencies(&metadata, "a"),
        BTreeSet::from(["serde_json".to_owned()])
    );
    assert_eq!(
        normal_dependencies(&metadata, "b"),
        BTreeSet::from(["a".to_owned()])
    );
}

#[test]
#[should_panic(expected = "must appear exactly once")]
fn missing_package_is_an_error_not_an_empty_set() {
    let metadata = serde_json::json!({"packages": [{"name": "a", "dependencies": []}]});
    let _ = normal_dependencies(&metadata, "rag-core");
}

fn flagged(layer: Layer, source: &str) -> bool {
    !violations(layer, source).is_empty()
}

#[test]
fn domain_flags_each_forbidden_reference() {
    let cases = [
        "use crate::application::Search;",
        "use super::application::Search;",
        "use super::super::Elsewhere;",
        "fn f() { let _ = super::super::x(); }",
        "use serde::Serialize;",
        "#[serde(rename = \"x\")] struct S;",
        "use std::fs::File;",
        "use std::process::Command;",
        "fn f() { let _ = std::env::var(\"X\"); }",
        "use std::net::TcpStream;",
        "use std::thread;",
        "use std::io::Read;",
        "use std::time::SystemTime;",
        "use std::time::Instant;",
        "fn f() { let _ = std::time::Instant::now(); }",
        "use rusqlite::Connection;",
        "use clap::Parser;",
        "use ignore::WalkBuilder;",
        "use dirs::home_dir;",
        "use tokio::spawn;",
        "extern crate rusqlite;",
    ];
    for source in cases {
        assert!(
            flagged(Layer::Domain, source),
            "domain should flag: {source}"
        );
    }
}

#[test]
fn application_flags_each_forbidden_reference() {
    for source in [
        "use rusqlite::Connection;",
        "use clap::Parser;",
        "use ignore::Walk;",
        "use dirs::home_dir;",
        "use std::fs;",
        "use std::process::Command;",
        "use std::env;",
        "use std::net::TcpStream;",
        "use std::thread::spawn;",
        "use crate::infrastructure::Sqlite;",
        "use rag_infra::Store;",
        "use rag_cli::Cli;",
    ] {
        assert!(
            flagged(Layer::Application, source),
            "application should flag: {source}"
        );
    }
}

#[test]
fn allowed_references_are_not_flagged() {
    assert!(!flagged(
        Layer::Domain,
        "use std::collections::BTreeMap;\nuse std::time::Duration;\nuse blake3::Hasher;\nuse crate::domain::Source;"
    ));
    assert!(!flagged(
        Layer::Application,
        "use serde::Serialize;\nuse std::io::Read;\nuse std::time::SystemTime;\nuse crate::domain::Source;\n#[cfg(test)] mod t { use super::*; }"
    ));
    // A local variable that happens to share a crate name is not a path into that crate.
    assert!(!flagged(
        Layer::Domain,
        "fn f(dirs: Vec<u8>) -> usize { let ignore = 1; dirs.len() + ignore }"
    ));
}

#[test]
fn grouped_use_is_expanded() {
    assert!(flagged(Layer::Domain, "use std::{fs, io};"));
    assert!(flagged(
        Layer::Application,
        "use std::{fs, collections::BTreeMap};"
    ));
    assert!(!flagged(
        Layer::Application,
        "use std::{io, collections::BTreeMap};"
    ));
}

#[test]
fn nested_group_is_expanded() {
    assert!(flagged(
        Layer::Domain,
        "use std::{collections::{BTreeMap, BTreeSet}, time::{Duration, Instant}};"
    ));
    assert!(flagged(
        Layer::Application,
        "use std::{collections::BTreeMap, {process::{self, Command}}};"
    ));
    assert!(!flagged(
        Layer::Domain,
        "use std::{collections::{BTreeMap, BTreeSet}, time::{Duration}};"
    ));
}

#[test]
fn fully_qualified_inline_path_is_flagged() {
    let source = "fn load() -> Vec<u8> { std::fs::read(\"x\").unwrap() }";
    assert!(flagged(Layer::Domain, source));
    assert!(flagged(Layer::Application, source));
    assert!(flagged(
        Layer::Application,
        "fn load() -> Vec<u8> { ::std::fs::read(\"x\").unwrap() }"
    ));
}

#[test]
fn alias_of_a_forbidden_root_is_flagged() {
    assert!(flagged(
        Layer::Application,
        "use std as s;\nfn f() { let _ = s::fs::read(\"x\"); }"
    ));
    assert!(flagged(Layer::Application, "use std::fs as files;"));
    assert!(flagged(
        Layer::Application,
        "use std::process;\nfn f() { let _ = process::Command::new(\"x\"); }"
    ));
    assert!(flagged(
        Layer::Domain,
        "use std::time;\nfn f() { let _ = time::Instant::now(); }"
    ));
    assert!(flagged(
        Layer::Application,
        "use std::*;\nfn f() { let _ = fs::read(\"x\"); }"
    ));
    assert!(flagged(
        Layer::Application,
        "use std::{self as s};\nfn f() { let _ = s::env::var(\"x\"); }"
    ));
}

#[test]
fn forbidden_paths_inside_macro_bodies_are_flagged() {
    assert!(flagged(
        Layer::Application,
        "fn f() { let _ = vec![std::fs::read(\"x\")]; }"
    ));
    assert!(!flagged(
        Layer::Application,
        "fn f() { let _ = format!(\"std::fs::read {}\", 1); }"
    ));
}

#[test]
fn string_containing_url_does_not_hide_following_code() {
    let source = "const URL: &str = \"http://x\";\nfn f() { let _ = std::fs::read(URL); }";
    assert!(flagged(Layer::Domain, source));
    assert!(flagged(Layer::Application, source));
}

#[test]
fn comments_and_strings_are_not_violations() {
    let source =
        "//! crate::application and std::fs\n// use std::fs;\n/// uses serde and SystemTime\n\
        /* block comment mentioning std::fs::read and std::process::Command */\n\
        /* multi\n line std::env */\n\
        const S: &str = \"use std::fs; crate::application serde\";\n\
        fn ok() {} // std::process";
    assert!(violations(Layer::Domain, source).is_empty());
    assert!(violations(Layer::Application, source).is_empty());
}

#[test]
fn identifiers_that_merely_contain_a_forbidden_word_are_not_flagged() {
    let source =
        "struct Instantiate; fn serde_like() {} mod fsm {} fn f() { let _ = Instantiate; }";
    assert!(violations(Layer::Domain, source).is_empty());
    assert!(violations(Layer::Application, source).is_empty());
}

#[test]
fn unparsable_source_is_reported_not_ignored() {
    assert!(flagged(Layer::Domain, "fn broken( {"));
}

#[test]
fn rag_core_depends_only_on_allowlisted_crates() {
    let metadata = cargo_metadata();
    assert_eq!(
        normal_dependencies(&metadata, "rag-core"),
        BTreeSet::from(["serde".to_owned(), "blake3".to_owned()]),
        "update this allowlist deliberately when rag-core gains a dependency"
    );
}

#[test]
fn rag_core_dev_dependencies_are_allowlisted() {
    let metadata = cargo_metadata();
    let dev = dev_dependencies(&metadata, "rag-core");
    let allowed: BTreeSet<String> = ["proptest", "tempfile", "serde_json", "syn"]
        .map(str::to_owned)
        .into();
    assert!(
        dev.is_subset(&allowed),
        "unexpected rag-core dev-dependencies: {:?}",
        dev.difference(&allowed).collect::<Vec<_>>()
    );
    assert!(
        !dev.contains("rag-infra"),
        "rag-core must not dev-depend on rag-infra"
    );
    assert!(
        !dev.contains("rag-cli"),
        "rag-core must not dev-depend on rag-cli"
    );
}

#[test]
fn all_three_packages_exist_in_the_workspace() {
    let metadata = cargo_metadata();
    for package in ["rag-core", "rag-infra", "rag-cli"] {
        let count = metadata["packages"]
            .as_array()
            .expect("packages array")
            .iter()
            .filter(|p| p["name"] == package)
            .count();
        assert_eq!(count, 1, "package `{package}` must exist exactly once");
    }
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
    let infra_dev = dev_dependencies(&metadata, "rag-infra");
    assert!(
        !infra_dev.contains("rag-cli"),
        "rag-infra must not dev-depend on rag-cli"
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
