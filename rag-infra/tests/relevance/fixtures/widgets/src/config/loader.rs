//! Loads the widget configuration file and reports what is wrong with it.

use std::fs;
use std::path::Path;

use crate::config::error::ConfigError;
use crate::config::Config;

/// The keys every configuration file must define.
const REQUIRED_KEYS: &[&str] = &["widget_name", "log_rotation_days"];

/// Reads and parses the configuration file at `path`.
pub fn load_config_file(path: &Path) -> Result<Config, ConfigError> {
    let raw = fs::read_to_string(path).map_err(ConfigError::Io)?;
    parse_config(&raw)
}

/// Parses the text of a configuration file.
pub fn parse_config(input: &str) -> Result<Config, ConfigError> {
    let table: toml::Table = input.parse().map_err(ConfigError::Syntax)?;
    for key in REQUIRED_KEYS {
        if !table.contains_key(*key) {
            return Err(ConfigError::Missing(format!(
                "config file is missing the required key {key}"
            )));
        }
    }
    Config::from_table(table)
}
