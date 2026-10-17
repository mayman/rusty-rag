//! The command surface (`docs/plan/02-commands.md`), parsed by clap.

use clap::{Parser, Subcommand};
use rag_core::application::dto::{DEFAULT_K, DEFAULT_MAX_CHARS};

/// Local-first keyword search over git repositories. Output is JSON unless
/// `--human` is given.
#[derive(Debug, Parser)]
#[command(name = "rag", bin_name = "rag", version, about)]
pub struct Cli {
    /// Plain text for people instead of JSON.
    #[arg(long, global = true)]
    pub human: bool,
    /// JSON output (the default).
    #[arg(long, global = true, conflicts_with = "human", hide = true)]
    pub json: bool,
    /// Fail at once, instead of waiting up to 20 seconds, when another process is
    /// adding or updating the same source.
    #[arg(long, global = true)]
    pub no_wait: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Clone (or register a local path) and index it.
    Add {
        /// A git URL or a local repository path.
        target: String,
        /// Name for the source (default: host/owner/name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Fetch and re-index what changed; no name updates every source.
    Update { name: Option<String> },
    /// Remove a source and its index.
    Remove { name: String },
    /// List the registered sources.
    List,
    /// Search the indexed repositories.
    Search {
        query: String,
        /// Hits wanted.
        #[arg(short = 'k', default_value_t = DEFAULT_K)]
        k: usize,
        /// Only these sources (name or slug); repeatable.
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Only these languages; repeatable.
        #[arg(long = "lang")]
        languages: Vec<String>,
        /// Only paths matching this glob.
        #[arg(long)]
        path: Option<String>,
        /// Hits to skip.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Cap on the characters of the response.
        #[arg(long, default_value_t = DEFAULT_MAX_CHARS)]
        max_chars: usize,
    },
    /// Show a chunk (`--chunk <id>`) or lines of an indexed file (`<repo> <path>`).
    Show {
        /// The chunk id printed by `rag search`.
        #[arg(
            long,
            conflicts_with_all = ["repo", "path", "lines", "around"]
        )]
        chunk: Option<String>,
        /// Source name or slug.
        #[arg(required_unless_present = "chunk")]
        repo: Option<String>,
        /// Repo-relative path of the file.
        #[arg(required_unless_present = "chunk")]
        path: Option<String>,
        /// First and last line, as `S-E`.
        #[arg(long, value_parser = parse_lines, conflicts_with = "around")]
        lines: Option<(usize, usize)>,
        /// Lines around this one.
        #[arg(long)]
        around: Option<usize>,
        /// Cap on the characters of text shown (at most 8000, at least 100).
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// Rebuild the index of one source, or all, after a tokenizer or chunker change.
    Reindex {
        /// Source name or slug.
        #[arg(conflicts_with = "all")]
        name: Option<String>,
        /// Every source (also what no name means).
        #[arg(long)]
        all: bool,
    },
}

/// `S-E` as two 1-based line numbers.
fn parse_lines(text: &str) -> Result<(usize, usize), String> {
    let wrong = || format!("{text:?} is not a line range; use S-E, for example 10-40");
    let (start, end) = text.split_once('-').ok_or_else(wrong)?;
    let start = start.trim().parse().map_err(|_| wrong())?;
    let end = end.trim().parse().map_err(|_| wrong())?;
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use clap::Parser;

    use super::{parse_lines, Cli, Command};

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("rag").chain(args.iter().copied()))
    }

    #[test]
    fn search_takes_every_documented_option() {
        let cli = parse(&[
            "search",
            "parse config",
            "-k",
            "7",
            "--repo",
            "a",
            "--repo",
            "b",
            "--lang",
            "rust",
            "--path",
            "src/**",
            "--offset",
            "3",
            "--max-chars",
            "900",
            "--human",
        ])
        .unwrap();
        assert!(cli.human);
        let Command::Search {
            query,
            k,
            repos,
            languages,
            path,
            offset,
            max_chars,
        } = cli.command
        else {
            panic!("not a search");
        };
        assert_eq!(query, "parse config");
        assert_eq!((k, offset, max_chars), (7, 3, 900));
        assert_eq!(repos, ["a", "b"]);
        assert_eq!(languages, ["rust"]);
        assert_eq!(path.as_deref(), Some("src/**"));
    }

    #[test]
    fn search_defaults_to_five_hits_and_the_default_budget() {
        let Command::Search {
            k,
            offset,
            max_chars,
            ..
        } = parse(&["search", "x"]).unwrap().command
        else {
            panic!("not a search");
        };
        assert_eq!((k, offset, max_chars), (5, 0, 8000));
    }

    #[test]
    fn human_is_global_so_it_may_follow_any_subcommand() {
        assert!(parse(&["list", "--human"]).unwrap().human);
        assert!(parse(&["--human", "list"]).unwrap().human);
        assert!(!parse(&["list"]).unwrap().human);
    }

    #[test]
    fn show_by_chunk_needs_no_repo_or_path() {
        let Command::Show { chunk, repo, .. } = parse(&["show", "--chunk", "ab"]).unwrap().command
        else {
            panic!("not a show");
        };
        assert_eq!((chunk.as_deref(), repo), (Some("ab"), None));
    }

    #[test]
    fn show_by_path_takes_lines_or_around() {
        let Command::Show { lines, .. } = parse(&["show", "r", "a.rs", "--lines", "3-9"])
            .unwrap()
            .command
        else {
            panic!("not a show");
        };
        assert_eq!(lines, Some((3, 9)));
        assert!(parse(&["show", "r", "a.rs", "--around", "4"]).is_ok());
    }

    #[test]
    fn show_takes_max_chars_with_either_form() {
        let Command::Show { max_chars, .. } = parse(&["show", "r", "a.rs", "--max-chars", "300"])
            .unwrap()
            .command
        else {
            panic!("not a show");
        };
        assert_eq!(max_chars, Some(300));
        assert!(parse(&["show", "--chunk", "ab", "--max-chars", "300"]).is_ok());
        let Command::Show { max_chars, .. } = parse(&["show", "r", "a.rs"]).unwrap().command else {
            panic!("not a show");
        };
        assert_eq!(max_chars, None);
    }

    #[test]
    fn show_rejects_mixed_and_missing_forms() {
        for args in [
            &["show"][..],
            &["show", "r"],
            &["show", "--chunk", "ab", "r", "a.rs"],
            &["show", "r", "a.rs", "--lines", "1-2", "--around", "1"],
            &["show", "r", "a.rs", "--lines", "nope"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn reindex_takes_a_name_or_all_but_not_both() {
        assert!(parse(&["reindex"]).is_ok());
        assert!(parse(&["reindex", "--all"]).is_ok());
        assert!(parse(&["reindex", "r"]).is_ok());
        assert!(parse(&["reindex", "r", "--all"]).is_err());
    }

    #[test]
    fn add_update_remove_and_list_parse() {
        assert!(parse(&["add", "https://github.com/o/r", "--name", "r"]).is_ok());
        assert!(parse(&["update"]).is_ok());
        assert!(parse(&["update", "r"]).is_ok());
        assert!(parse(&["remove", "r"]).is_ok());
        assert!(parse(&["list"]).is_ok());
        assert!(parse(&["remove"]).is_err());
    }

    #[test]
    fn an_unknown_command_is_a_usage_error_not_a_help_request() {
        let error = parse(&["frobnicate"]).unwrap_err();
        assert_ne!(error.kind(), ErrorKind::DisplayHelp);
    }

    #[test]
    fn line_ranges_need_two_numbers() {
        assert_eq!(parse_lines("10-40"), Ok((10, 40)));
        assert!(parse_lines("10").is_err());
        assert!(parse_lines("a-b").is_err());
        assert!(parse_lines("-5").is_err());
    }
}
