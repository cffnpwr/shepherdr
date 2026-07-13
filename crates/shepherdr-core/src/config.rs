//! Schema and parsing for the Shepherdr config file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use serde::Deserialize;
use thiserror::Error;
use toml::de;

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The list of service definitions.
    #[serde(default)]
    pub services: Vec<Service>,
}

/// A single `[[services]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Service name. Unique across all entries. Used for the log file name and UI display.
    pub name: String,
    /// The argv to execute. Not evaluated by a shell.
    pub command: Vec<String>,
    /// When `true`, launch through a login shell.
    #[serde(default)]
    pub login_shell: bool,
    /// Extra environment variables layered on top of the app process environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory. Inherits the app process's when unset.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// When `false`, keep the definition but stop auto-starting it.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

/// Errors raised while loading or parsing the configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The home directory could not be resolved.
    #[error("failed to resolve the home directory")]
    HomeDirNotFound,
    /// The configuration file could not be read.
    #[error("failed to read the configuration file: {path}")]
    Read {
        /// The path that was being read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// TOML parsing failed.
    #[error("failed to parse the configuration file")]
    Parse(#[from] de::Error),
    /// A service name is duplicated.
    #[error("duplicate service name: {0}")]
    DuplicateName(String),
    /// A `command` is empty.
    #[error("command of service \"{0}\" is empty")]
    EmptyCommand(String),
}

impl Config {
    /// Loads from the default path (`~/.config/shepherdr/config.toml`).
    ///
    /// # Errors
    ///
    /// Returns an error when the home directory cannot be resolved, when the file cannot be
    /// read, or when parsing or validation fails.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&config_path()?)
    }

    /// Loads from the given path, then parses and validates.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, or when parsing or validation fails.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&content)
    }

    /// Parses a TOML string and validates it.
    ///
    /// # Errors
    ///
    /// Returns an error when TOML parsing fails, when a `name` is duplicated, or when a
    /// `command` is empty.
    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates `name` uniqueness and that each `command` is non-empty.
    fn validate(&self) -> Result<(), ConfigError> {
        let mut seen = HashSet::new();
        for service in &self.services {
            if service.command.is_empty() {
                return Err(ConfigError::EmptyCommand(service.name.clone()));
            }
            if !seen.insert(service.name.as_str()) {
                return Err(ConfigError::DuplicateName(service.name.clone()));
            }
        }
        Ok(())
    }
}

/// Resolves the default configuration file path.
///
/// Uses `$XDG_CONFIG_HOME/shepherdr/config.toml` when `XDG_CONFIG_HOME` is set to an
/// absolute path, otherwise falls back to `~/.config/shepherdr/config.toml`. A relative or
/// empty `XDG_CONFIG_HOME` is ignored, per the XDG Base Directory Specification.
fn config_path() -> Result<PathBuf, ConfigError> {
    let config_home = match env::var_os("XDG_CONFIG_HOME") {
        Some(value) if Path::new(&value).is_absolute() => PathBuf::from(value),
        _ => env::home_dir()
            .ok_or(ConfigError::HomeDirNotFound)?
            .join(".config"),
    };
    Ok(config_home.join("shepherdr").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_parse_applies_defaults() {
        // Given a service that sets only the required fields
        let input = r#"
            [[services]]
            name = "herdr"
            command = ["herdr", "server"]
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then the optional fields take their default values
        let expected = Config {
            services: vec![Service {
                name: "herdr".to_owned(),
                command: vec!["herdr".to_owned(), "server".to_owned()],
                login_shell: false,
                env: HashMap::new(),
                cwd: None,
                enabled: true,
            }],
        };
        assert_eq!(result.ok(), Some(expected));
    }

    #[test]
    fn positive_parse_reads_all_fields() {
        // Given a service that sets every field
        let input = r#"
            [[services]]
            name = "example-daemon"
            command = ["/opt/homebrew/bin/example-daemon", "--verbose"]
            login_shell = true
            env = { RUST_LOG = "info" }
            cwd = "/Users/cffnpwr/work"
            enabled = false
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then every field is read exactly as written
        let expected = Config {
            services: vec![Service {
                name: "example-daemon".to_owned(),
                command: vec![
                    "/opt/homebrew/bin/example-daemon".to_owned(),
                    "--verbose".to_owned(),
                ],
                login_shell: true,
                env: HashMap::from([("RUST_LOG".to_owned(), "info".to_owned())]),
                cwd: Some(PathBuf::from("/Users/cffnpwr/work")),
                enabled: false,
            }],
        };
        assert_eq!(result.ok(), Some(expected));
    }

    #[test]
    fn positive_parse_accepts_empty_input() {
        // Given empty input
        let input = "";

        // When it is parsed
        let result = Config::parse(input);

        // Then the config has no services
        let expected = Config { services: vec![] };
        assert_eq!(result.ok(), Some(expected));
    }

    #[test]
    fn negative_parse_rejects_duplicate_name() {
        // Given two services sharing the same name
        let input = r#"
            [[services]]
            name = "dup"
            command = ["a"]

            [[services]]
            name = "dup"
            command = ["b"]
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails with the duplicated name
        assert!(matches!(result, Err(ConfigError::DuplicateName(name)) if name == "dup"));
    }

    #[test]
    fn negative_parse_rejects_empty_command() {
        // Given a service whose command is empty
        let input = r#"
            [[services]]
            name = "empty"
            command = []
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails with the offending service name
        assert!(matches!(result, Err(ConfigError::EmptyCommand(name)) if name == "empty"));
    }

    #[test]
    fn negative_parse_rejects_missing_required_field() {
        // Given a service without the required command field
        let input = r#"
            [[services]]
            name = "no-command"
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails while parsing TOML
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn negative_parse_rejects_unknown_field() {
        // Given a service with an unknown field
        let input = r#"
            [[services]]
            name = "svc"
            command = ["a"]
            typo_field = true
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails while parsing TOML
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }
}
