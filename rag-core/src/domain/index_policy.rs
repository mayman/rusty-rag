//! IndexPolicy: which files are worth indexing. Pure rules, applied in two steps:
//! `check_entry` on a `TreeEntry` from `git ls-tree` (path and size only, before any
//! blob is read) and `check_content` on the bytes once read. Each skip has a
//! `SkipReason` so output can say why files were left out. `check_source_size`
//! caps how much one source may index in v1.
//!
//! Secrets are screened by file name (`check_entry`) and by a minimal content scan
//! (`check_content`): PEM private-key headers and a few obvious token shapes. It is
//! a safety net against indexing a committed key, not a secret scanner.
//!
//! `POLICY_VERSION` is stored with each index (`IndexVersions::policy`); change it
//! whenever a rule here changes which files or bytes are accepted.

use std::fmt;

use crate::domain::repo_path::RepoPath;
use crate::domain::tree_entry::TreeEntry;

/// Version of the rules in this module. Bump it when they change, so indexes built
/// under older rules are reindexed.
pub const POLICY_VERSION: u32 = 1;
/// Files larger than this are skipped (about 1 MB).
pub const MAX_FILE_BYTES: u64 = 1_000_000;
/// The most files one source may index (after the skip rules).
pub const MAX_FILES_PER_SOURCE: usize = 20_000;
/// The most chunks one source may hold.
pub const MAX_CHUNKS_PER_SOURCE: usize = 200_000;
/// Git LFS pointer files are shorter than this.
const MAX_LFS_POINTER_BYTES: usize = 1024;
const LFS_POINTER_HEADER: &[u8] = b"version https://git-lfs.github.com/spec/v1";
/// Content shorter than this is never judged minified by line length.
const MIN_BYTES_FOR_LINE_CHECK: usize = 1024;
/// Content whose average line is longer than this (in bytes) is minified.
const MAX_AVERAGE_LINE_BYTES: usize = 200;

/// Why a file was left out of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    TooLarge,
    Binary,
    Lockfile,
    Minified,
    Generated,
    Vendored,
    SecretName,
    /// The content holds a private key block or an obvious access token.
    SecretContent,
    /// A Git LFS pointer: the real content is not in the repository.
    LfsPointer,
    /// The bytes are not valid UTF-8.
    InvalidUtf8,
}

impl SkipReason {
    /// Every reason, in a fixed order.
    pub const ALL: [SkipReason; 10] = [
        SkipReason::TooLarge,
        SkipReason::Binary,
        SkipReason::Lockfile,
        SkipReason::Minified,
        SkipReason::Generated,
        SkipReason::Vendored,
        SkipReason::SecretName,
        SkipReason::SecretContent,
        SkipReason::LfsPointer,
        SkipReason::InvalidUtf8,
    ];

    /// Stable string for output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::Binary => "binary",
            Self::Lockfile => "lockfile",
            Self::Minified => "minified",
            Self::Generated => "generated",
            Self::Vendored => "vendored",
            Self::SecretName => "secret_name",
            Self::SecretContent => "secret_content",
            Self::LfsPointer => "lfs_pointer",
            Self::InvalidUtf8 => "invalid_utf8",
        }
    }
}

const LOCKFILE_NAMES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "packages.lock.json",
    "pnpm-lock.yaml",
    "gradle.lockfile",
    "go.sum",
    "bun.lockb",
];
const GENERATED_SUFFIXES: &[&str] = &[
    ".map",
    ".pb.go",
    ".pb.cc",
    ".pb.h",
    "_pb2.py",
    "_pb2_grpc.py",
    ".g.dart",
    ".designer.cs",
    ".svg",
    ".ipynb",
];
const VENDORED_DIRECTORIES: &[&str] =
    &["node_modules", "vendor", "third_party", "bower_components"];
const SECRET_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "ppk", "tfvars", "kdbx",
];
const SECRET_NAMES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".htpasswd",
    "credentials",
    "credentials.json",
    "secrets.json",
    "secrets.yml",
    "secrets.yaml",
    ".envrc",
    "terraform.tfstate",
    "terraform.tfstate.backup",
    ".pgpass",
    ".git-credentials",
    "kubeconfig",
];
/// Extensions of prose, which is often one long line per paragraph and so exempt
/// from the minified heuristic.
const PROSE_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "txt"];
/// Token shapes that are plainly credentials: a prefix, then at least `min_tail`
/// characters of the allowed kind.
const TOKEN_SHAPES: &[TokenShape] = &[
    TokenShape::new("ghp_", 36, TailKind::Alphanumeric),
    TokenShape::new("gho_", 36, TailKind::Alphanumeric),
    TokenShape::new("ghu_", 36, TailKind::Alphanumeric),
    TokenShape::new("ghs_", 36, TailKind::Alphanumeric),
    TokenShape::new("ghr_", 36, TailKind::Alphanumeric),
    TokenShape::new("github_pat_", 22, TailKind::Word),
    TokenShape::new("AKIA", 16, TailKind::UpperAlphanumeric),
    TokenShape::new("ASIA", 16, TailKind::UpperAlphanumeric),
    TokenShape::new("xoxb-", 10, TailKind::DashedAlphanumeric),
    TokenShape::new("xoxp-", 10, TailKind::DashedAlphanumeric),
    TokenShape::new("xoxa-", 10, TailKind::DashedAlphanumeric),
    TokenShape::new("xoxr-", 10, TailKind::DashedAlphanumeric),
    TokenShape::new("xoxs-", 10, TailKind::DashedAlphanumeric),
    TokenShape::new("glpat-", 20, TailKind::Word),
    TokenShape::new("AIza", 35, TailKind::Word),
];
const SECRET_NAME_PREFIXES: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
const ENV_TEMPLATE_SUFFIXES: &[&str] = &["example", "sample", "template", "dist"];
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tif", "tiff", "psd", "pdf", "zip", "tar",
    "gz", "tgz", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib", "a", "lib", "o", "obj",
    "class", "jar", "war", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "mov", "avi", "wav",
    "ogg", "flac", "webm", "sqlite", "sqlite3", "db", "bin", "dat", "pyc", "wasm", "iso", "dmg",
];

/// Domain service holding the indexing rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexPolicy;

impl IndexPolicy {
    /// Judges a file from its path and size alone.
    pub fn check_entry(entry: &TreeEntry) -> Result<(), SkipReason> {
        let path = entry.path.as_str().to_ascii_lowercase();
        let (directories, name) = path.rsplit_once('/').unwrap_or(("", &path));
        if directories
            .split('/')
            .any(|directory| VENDORED_DIRECTORIES.contains(&directory))
        {
            return Err(SkipReason::Vendored);
        }
        if is_secret_name(name) {
            return Err(SkipReason::SecretName);
        }
        if name.ends_with(".lock") || LOCKFILE_NAMES.contains(&name) {
            return Err(SkipReason::Lockfile);
        }
        if name.contains(".min.") || name.ends_with("-min.js") || name.ends_with("-min.css") {
            return Err(SkipReason::Minified);
        }
        if name.contains(".generated.") || GENERATED_SUFFIXES.iter().any(|s| name.ends_with(s)) {
            return Err(SkipReason::Generated);
        }
        if extension(name).is_some_and(|ext| BINARY_EXTENSIONS.contains(&ext)) {
            return Err(SkipReason::Binary);
        }
        if entry.size > MAX_FILE_BYTES {
            return Err(SkipReason::TooLarge);
        }
        Ok(())
    }

    /// Judges a file from its bytes, in this order: binary (a NUL byte or a UTF-16
    /// byte order mark), invalid UTF-8, a Git LFS pointer, minified (average line
    /// very long; markdown and text files are exempt), then secret content.
    pub fn check_content(path: &RepoPath, content: &[u8]) -> Result<(), SkipReason> {
        if content.contains(&0)
            || content.starts_with(&[0xff, 0xfe])
            || content.starts_with(&[0xfe, 0xff])
        {
            return Err(SkipReason::Binary);
        }
        let Ok(text) = std::str::from_utf8(content) else {
            return Err(SkipReason::InvalidUtf8);
        };
        if is_lfs_pointer(content) {
            return Err(SkipReason::LfsPointer);
        }
        if !is_prose(path)
            && content.len() >= MIN_BYTES_FOR_LINE_CHECK
            && content.len() / line_count(content) > MAX_AVERAGE_LINE_BYTES
        {
            return Err(SkipReason::Minified);
        }
        if has_private_key_block(text) || has_token(text) {
            return Err(SkipReason::SecretContent);
        }
        Ok(())
    }

    /// Fails when a source holds more indexable files or chunks than v1 supports.
    pub fn check_source_size(files: usize, chunks: usize) -> Result<(), SourceTooLarge> {
        if files > MAX_FILES_PER_SOURCE {
            return Err(SourceTooLarge::Files {
                found: files,
                limit: MAX_FILES_PER_SOURCE,
            });
        }
        if chunks > MAX_CHUNKS_PER_SOURCE {
            return Err(SourceTooLarge::Chunks {
                found: chunks,
                limit: MAX_CHUNKS_PER_SOURCE,
            });
        }
        Ok(())
    }
}

/// A source over the per-source caps (v1 has no partial or resumable indexing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTooLarge {
    Files { found: usize, limit: usize },
    Chunks { found: usize, limit: usize },
}

impl fmt::Display for SourceTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (found, limit, what) = match self {
            Self::Files { found, limit } => (found, limit, "files"),
            Self::Chunks { found, limit } => (found, limit, "chunks"),
        };
        write!(
            f,
            "this repository has {found} indexable {what}, over the limit of {limit}; \
             it is too large to index in this version"
        )
    }
}

impl std::error::Error for SourceTooLarge {}

/// Which characters may follow a token prefix.
#[derive(Debug, Clone, Copy)]
enum TailKind {
    Alphanumeric,
    UpperAlphanumeric,
    /// Letters, digits, `_`, and `-`.
    Word,
    /// Letters, digits, and `-`.
    DashedAlphanumeric,
}

impl TailKind {
    fn accepts(self, c: char) -> bool {
        match self {
            Self::Alphanumeric => c.is_ascii_alphanumeric(),
            Self::UpperAlphanumeric => c.is_ascii_uppercase() || c.is_ascii_digit(),
            Self::Word => c.is_ascii_alphanumeric() || matches!(c, '_' | '-'),
            Self::DashedAlphanumeric => c.is_ascii_alphanumeric() || c == '-',
        }
    }
}

struct TokenShape {
    prefix: &'static str,
    min_tail: usize,
    tail: TailKind,
}

impl TokenShape {
    const fn new(prefix: &'static str, min_tail: usize, tail: TailKind) -> Self {
        Self {
            prefix,
            min_tail,
            tail,
        }
    }

    /// Whether `text` holds the prefix at a word start, followed by enough tail.
    fn occurs_in(&self, text: &str) -> bool {
        text.match_indices(self.prefix).any(|(at, _)| {
            let starts_word = text[..at]
                .chars()
                .next_back()
                .is_none_or(|before| !before.is_ascii_alphanumeric() && before != '_');
            let tail = text[at + self.prefix.len()..]
                .chars()
                .take_while(|c| self.tail.accepts(*c))
                .count();
            starts_word && tail >= self.min_tail
        })
    }
}

fn has_token(text: &str) -> bool {
    TOKEN_SHAPES.iter().any(|shape| shape.occurs_in(text))
}

/// A PEM header line that begins a private key (`-----BEGIN RSA PRIVATE KEY-----`,
/// `-----BEGIN PGP PRIVATE KEY BLOCK-----`, ...).
fn has_private_key_block(text: &str) -> bool {
    text.match_indices("-----BEGIN ").any(|(at, _)| {
        let line = text[at..].lines().next().unwrap_or("");
        line.contains("PRIVATE KEY")
    })
}

/// A Git LFS pointer is a short file that starts with the spec header and names
/// an object id.
fn is_lfs_pointer(content: &[u8]) -> bool {
    content.len() < MAX_LFS_POINTER_BYTES
        && content.starts_with(LFS_POINTER_HEADER)
        && content
            .windows(b"\noid sha256:".len())
            .any(|window| window == b"\noid sha256:")
}

/// Markdown and plain-text files, by extension.
fn is_prose(path: &RepoPath) -> bool {
    let name = path.file_name().to_ascii_lowercase();
    extension(&name).is_some_and(|ext| PROSE_EXTENSIONS.contains(&ext))
}

fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.').map(|(_, ext)| ext)
}

/// `name` is already lowercase.
fn is_secret_name(name: &str) -> bool {
    if name == ".env" {
        return true;
    }
    if let Some(suffix) = name.strip_prefix(".env.") {
        return !ENV_TEMPLATE_SUFFIXES.contains(&suffix);
    }
    SECRET_NAMES.contains(&name)
        || extension(name).is_some_and(|ext| SECRET_EXTENSIONS.contains(&ext))
        || (SECRET_NAME_PREFIXES.iter().any(|p| name.starts_with(p)) && !name.ends_with(".pub"))
}

/// Lines in `content`: LF, CRLF, and a lone CR each end a line; a last line
/// without a terminator counts too. At least 1.
fn line_count(content: &[u8]) -> usize {
    let breaks = content
        .iter()
        .enumerate()
        .filter(|(i, b)| **b == b'\n' || (**b == b'\r' && content.get(i + 1) != Some(&b'\n')))
        .count();
    let unterminated_tail = content.last().is_some_and(|b| !matches!(b, b'\n' | b'\r'));
    (breaks + usize::from(unterminated_tail)).max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        IndexPolicy, SkipReason, SourceTooLarge, MAX_CHUNKS_PER_SOURCE, MAX_FILES_PER_SOURCE,
        MAX_FILE_BYTES,
    };
    use crate::domain::oid::BlobOid;
    use crate::domain::repo_path::RepoPath;
    use crate::domain::tree_entry::TreeEntry;

    fn entry(path: &str, size: u64) -> TreeEntry {
        TreeEntry {
            path: RepoPath::parse(path).unwrap(),
            blob: BlobOid::parse(&"ab".repeat(20)).unwrap(),
            size,
        }
    }

    fn verdict(path: &str) -> Result<(), SkipReason> {
        IndexPolicy::check_entry(&entry(path, 10))
    }

    /// The content verdict for a code file.
    fn content(bytes: &[u8]) -> Result<(), SkipReason> {
        content_of("src/a.rs", bytes)
    }

    fn content_of(path: &str, bytes: &[u8]) -> Result<(), SkipReason> {
        IndexPolicy::check_content(&RepoPath::parse(path).unwrap(), bytes)
    }

    #[test]
    fn ordinary_source_files_are_indexed() {
        for path in [
            "src/main.rs",
            "README.md",
            "docs/guide.md",
            ".github/workflows/ci.yml",
            "Cargo.toml",
            ".env.example",
            "src/locker.rs",
            "assets/minimal.js",
        ] {
            assert_eq!(verdict(path), Ok(()), "{path}");
        }
    }

    #[test]
    fn size_cap_is_one_megabyte() {
        assert_eq!(MAX_FILE_BYTES, 1_000_000);
        assert_eq!(
            IndexPolicy::check_entry(&entry("a.rs", MAX_FILE_BYTES)),
            Ok(())
        );
        assert_eq!(
            IndexPolicy::check_entry(&entry("a.rs", MAX_FILE_BYTES + 1)),
            Err(SkipReason::TooLarge)
        );
    }

    #[test]
    fn lockfiles_are_skipped() {
        for path in [
            "Cargo.lock",
            "package-lock.json",
            "web/yarn.lock",
            "pnpm-lock.yaml",
            "go.sum",
            "poetry.lock",
            "Gemfile.lock",
            "deps/Anything.LOCK",
        ] {
            assert_eq!(verdict(path), Err(SkipReason::Lockfile), "{path}");
        }
    }

    #[test]
    fn minified_files_are_skipped() {
        for path in [
            "dist/app.min.js",
            "css/site.min.css",
            "APP.MIN.JS",
            "lib/x.min.mjs",
        ] {
            assert_eq!(verdict(path), Err(SkipReason::Minified), "{path}");
        }
    }

    #[test]
    fn generated_files_are_skipped() {
        for path in [
            "app.js.map",
            "api/service.pb.go",
            "api/service_pb2.py",
            "src/schema.generated.ts",
            "lib/model.g.dart",
        ] {
            assert_eq!(verdict(path), Err(SkipReason::Generated), "{path}");
        }
    }

    #[test]
    fn vendored_directories_are_skipped() {
        for path in [
            "node_modules/left-pad/index.js",
            "web/node_modules/x/y.js",
            "vendor/github.com/x/y.go",
            "third_party/lib/a.c",
        ] {
            assert_eq!(verdict(path), Err(SkipReason::Vendored), "{path}");
        }
        assert_eq!(verdict("src/vendored_notes.md"), Ok(()));
    }

    #[test]
    fn secret_looking_names_are_skipped() {
        for path in [
            ".env",
            "config/.env.production",
            "keys/server.pem",
            "deploy.key",
            "cert.p12",
            "id_rsa",
            "home/.ssh/id_ed25519",
            "id_rsa.bak",
            ".netrc",
            "credentials.json",
            "aws/credentials",
            "SERVER.PEM",
        ] {
            assert_eq!(verdict(path), Err(SkipReason::SecretName), "{path}");
        }
        assert_eq!(verdict("id_rsa.pub"), Ok(()), "public keys are not secret");
    }

    #[test]
    fn binary_extensions_are_skipped() {
        for path in [
            "logo.png",
            "a/b/photo.JPG",
            "dist.zip",
            "lib.dll",
            "app.exe",
            "font.woff2",
            "data.sqlite",
            "doc.pdf",
            "mod.wasm",
        ] {
            assert_eq!(verdict(path), Err(SkipReason::Binary), "{path}");
        }
    }

    #[test]
    fn skip_reasons_have_stable_strings() {
        let strings: Vec<&str> = SkipReason::ALL.iter().map(SkipReason::as_str).collect();
        assert_eq!(
            strings,
            [
                "too_large",
                "binary",
                "lockfile",
                "minified",
                "generated",
                "vendored",
                "secret_name",
                "secret_content",
                "lfs_pointer",
                "invalid_utf8"
            ]
        );
    }

    #[test]
    fn plain_text_passes_the_content_check() {
        assert_eq!(content(b"fn main() {}\n"), Ok(()));
        assert_eq!(content(b""), Ok(()));
        assert_eq!(content("h\u{e9}llo w\u{f6}rld\n".as_bytes()), Ok(()));
    }

    #[test]
    fn a_nul_byte_means_binary() {
        assert_eq!(content(b"text\0more"), Err(SkipReason::Binary));
    }

    #[test]
    fn a_utf16_byte_order_mark_means_binary() {
        assert_eq!(content(&[0xff, 0xfe, b'a', 0]), Err(SkipReason::Binary));
        assert_eq!(content(&[0xfe, 0xff, 0, b'a']), Err(SkipReason::Binary));
    }

    #[test]
    fn a_utf8_byte_order_mark_is_text() {
        assert_eq!(content(b"\xef\xbb\xbfhello\n"), Ok(()));
    }

    #[test]
    fn very_long_average_lines_mean_minified() {
        let one_long_line = "x".repeat(5000);
        assert_eq!(content(one_long_line.as_bytes()), Err(SkipReason::Minified));
        let few_lines = format!("{}\n{}\n", "y".repeat(3000), "z".repeat(3000));
        assert_eq!(content(few_lines.as_bytes()), Err(SkipReason::Minified));
    }

    #[test]
    fn short_files_and_normal_lines_are_not_minified() {
        let short = "x".repeat(900);
        assert_eq!(content(short.as_bytes()), Ok(()));
        let normal = "let value = compute(argument);\n".repeat(200);
        assert_eq!(content(normal.as_bytes()), Ok(()));
    }

    #[test]
    fn carriage_return_line_endings_count_as_line_breaks() {
        let cr_only = "let value = compute(argument);\r".repeat(200);
        assert_eq!(content(cr_only.as_bytes()), Ok(()));
        let crlf = "let value = compute(argument);\r\n".repeat(200);
        assert_eq!(content(crlf.as_bytes()), Ok(()));
    }

    #[test]
    fn more_secret_file_names_are_skipped() {
        for path in [
            ".envrc",
            "deploy/.envrc",
            "infra/prod.tfvars",
            "infra/prod.auto.tfvars",
            "TERRAFORM.TFVARS",
            "infra/terraform.tfstate",
            "infra/terraform.tfstate.backup",
            ".pgpass",
            "home/.git-credentials",
            "kubeconfig",
            ".kube/KubeConfig",
            "vault/passwords.kdbx",
        ] {
            assert_eq!(verdict(path), Err(SkipReason::SecretName), "{path}");
        }
        for path in [
            "infra/main.tf",
            "infra/variables.tf",
            "docs/tfvars.md",
            "src/kubeconfig.rs",
            "src/envrc.rs",
        ] {
            assert_eq!(verdict(path), Ok(()), "{path}");
        }
    }

    #[test]
    fn svg_and_notebook_files_count_as_generated() {
        for path in ["assets/logo.svg", "analysis.ipynb", "nb/Notebook.IPYNB"] {
            assert_eq!(verdict(path), Err(SkipReason::Generated), "{path}");
        }
    }

    #[test]
    fn invalid_utf8_is_its_own_skip_reason() {
        assert_eq!(content(b"caf\xe9 au lait\n"), Err(SkipReason::InvalidUtf8));
        assert_eq!(content(b"ok\n\xff\xfe\xfd"), Err(SkipReason::InvalidUtf8));
        assert_eq!(content("caf\u{e9}\n".as_bytes()), Ok(()));
    }

    const LFS_POINTER: &str = "version https://git-lfs.github.com/spec/v1\n\
        oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
        size 12345\n";

    #[test]
    fn git_lfs_pointers_are_skipped_because_they_hold_no_content() {
        assert_eq!(content(LFS_POINTER.as_bytes()), Err(SkipReason::LfsPointer));
        assert_eq!(
            content_of("docs/diagram.md", LFS_POINTER.as_bytes()),
            Err(SkipReason::LfsPointer)
        );
        assert_eq!(
            content(b"version https://example.com/spec/v1\noid sha256:abc\n"),
            Ok(()),
            "only the git-lfs spec header marks a pointer"
        );
        let doc = format!("{}{}", LFS_POINTER, "padding line\n".repeat(200));
        assert_eq!(
            content(doc.as_bytes()),
            Ok(()),
            "a real pointer is under 1 KiB; a long file that merely starts alike is text"
        );
    }

    #[test]
    fn private_key_blocks_are_secret_content() {
        for header in [
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN EC PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----",
            "-----BEGIN PGP PRIVATE KEY BLOCK-----",
        ] {
            let text = format!("config:\n{header}\nMIIEvQIBADANBg\n");
            assert_eq!(
                content(text.as_bytes()),
                Err(SkipReason::SecretContent),
                "{header}"
            );
        }
        for harmless in [
            "-----BEGIN CERTIFICATE-----",
            "-----BEGIN PUBLIC KEY-----",
            "a private key is a secret",
            "-----BEGIN PRIVATE-----",
        ] {
            assert_eq!(content(harmless.as_bytes()), Ok(()), "{harmless}");
        }
    }

    #[test]
    fn obvious_token_shapes_are_secret_content() {
        let tokens = [
            format!("ghp_{}", "a1B2".repeat(9)),
            format!("gho_{}", "Z".repeat(36)),
            format!("github_pat_{}", "x9".repeat(15)),
            "AKIAIOSFODNN7EXAMPLE".to_owned(),
            "xoxb-123456789012-abcdefghijkl".to_owned(),
            format!("glpat-{}", "q".repeat(20)),
            format!("AIza{}", "Sy-".repeat(12)),
        ];
        for token in tokens {
            for text in [
                format!("key = \"{token}\"\n"),
                format!("{token}\n"),
                token.clone(),
            ] {
                assert_eq!(
                    content(text.as_bytes()),
                    Err(SkipReason::SecretContent),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn prefixes_that_are_not_tokens_are_text() {
        for text in [
            "the ghp_ prefix marks a GitHub token",
            "ghp_short",
            "see AKIA keys in the AWS docs",
            "xAKIAIOSFODNN7EXAMPLE",
            "my_ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "xoxb-",
            "glpat-tooshort",
            "skip_the_AIza_part",
        ] {
            assert_eq!(content(text.as_bytes()), Ok(()), "{text}");
        }
    }

    #[test]
    fn markdown_and_text_are_exempt_from_the_minified_heuristic() {
        let one_long_line = "word ".repeat(2000);
        for path in [
            "README.md",
            "docs/a.MARKDOWN",
            "docs/page.mdx",
            "notes/todo.txt",
        ] {
            assert_eq!(content_of(path, one_long_line.as_bytes()), Ok(()), "{path}");
        }
        for path in ["app.js", "data.json", "styles.css", "no_extension"] {
            assert_eq!(
                content_of(path, one_long_line.as_bytes()),
                Err(SkipReason::Minified),
                "{path}"
            );
        }
    }

    #[test]
    fn prose_files_are_still_scanned_for_secrets_and_binary_content() {
        assert_eq!(
            content_of("README.md", b"-----BEGIN RSA PRIVATE KEY-----\n"),
            Err(SkipReason::SecretContent)
        );
        assert_eq!(content_of("notes.txt", b"a\0b"), Err(SkipReason::Binary));
    }

    #[test]
    fn a_source_is_capped_in_files_and_in_chunks() {
        assert_eq!(MAX_FILES_PER_SOURCE, 20_000);
        assert_eq!(MAX_CHUNKS_PER_SOURCE, 200_000);
        assert_eq!(
            IndexPolicy::check_source_size(MAX_FILES_PER_SOURCE, MAX_CHUNKS_PER_SOURCE),
            Ok(())
        );
        assert_eq!(
            IndexPolicy::check_source_size(MAX_FILES_PER_SOURCE + 1, 0),
            Err(SourceTooLarge::Files {
                found: MAX_FILES_PER_SOURCE + 1,
                limit: MAX_FILES_PER_SOURCE
            })
        );
        assert_eq!(
            IndexPolicy::check_source_size(1, MAX_CHUNKS_PER_SOURCE + 1),
            Err(SourceTooLarge::Chunks {
                found: MAX_CHUNKS_PER_SOURCE + 1,
                limit: MAX_CHUNKS_PER_SOURCE
            })
        );
    }

    #[test]
    fn the_cap_error_says_what_was_found_and_what_the_limit_is() {
        let files = SourceTooLarge::Files {
            found: 25_000,
            limit: 20_000,
        }
        .to_string();
        assert!(
            files.contains("25000") && files.contains("20000") && files.contains("files"),
            "{files}"
        );
        let chunks = SourceTooLarge::Chunks {
            found: 300_000,
            limit: 200_000,
        }
        .to_string();
        assert!(
            chunks.contains("300000") && chunks.contains("200000") && chunks.contains("chunks"),
            "{chunks}"
        );
    }
}
