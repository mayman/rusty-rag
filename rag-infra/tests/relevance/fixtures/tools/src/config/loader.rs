//! Legacy configuration loader kept for the tools binary.

/// Parses the old `key = value` configuration format, one pair per line.
pub fn parse_config(input: &str) -> Vec<(String, String)> {
    input
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}
