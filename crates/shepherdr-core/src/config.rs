//! Schema and parsing for the Shepherdr config file.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

use bytesize::ByteSize;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use thiserror::Error;
use toml::de;

use crate::logging::{DEFAULT_MAX_BYTES, DEFAULT_MAX_GENERATIONS};

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The list of service definitions.
    #[serde(default)]
    pub services: Vec<Service>,
    /// The optional `[log]` section overriding the log rotation limits.
    #[serde(default)]
    pub log: LogConfig,
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
    pub env: FxHashMap<String, String>,
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

/// The optional top-level `[log]` section. Every field falls back to an implementation default
/// when unset, and the section itself may be omitted entirely. Semantic constraints (a positive
/// `max_size`, a `max_generations` of at least 1) are checked in [`Config::validate`], not here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// Per-generation size cap in bytes, parsed from a unit string such as `"10MiB"` or
    /// `"10MB"` (binary and decimal units, case-insensitive; see [`deserialize_size`]).
    /// Defaults to [`DEFAULT_MAX_BYTES`] when unset.
    #[serde(default = "default_max_size", deserialize_with = "deserialize_size")]
    pub max_size: u64,
    /// Number of generations kept, including the current file. Defaults to
    /// [`DEFAULT_MAX_GENERATIONS`] when unset.
    #[serde(default = "default_max_generations")]
    pub max_generations: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_size: default_max_size(),
            max_generations: default_max_generations(),
        }
    }
}

const fn default_max_size() -> u64 {
    DEFAULT_MAX_BYTES
}

const fn default_max_generations() -> u32 {
    DEFAULT_MAX_GENERATIONS
}

/// Deserializes `max_size` from a unit string such as `"10MiB"` or `"10MB"` into its byte
/// count, via the `bytesize` crate.
///
/// `bytesize` distinguishes binary units (`KiB`/`MiB`/`GiB`, factors of 1024) from decimal units
/// (`KB`/`MB`/`GB`, factors of 1000), matches units case-insensitively, and accepts a fractional
/// value (e.g. `"1.5GiB"`).
///
/// # Errors
///
/// Returns a deserialization error when the value cannot be parsed as a `bytesize::ByteSize`.
fn deserialize_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse::<ByteSize>()
        .map(|size| size.as_u64())
        .map_err(D::Error::custom)
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
    /// The `[log]` section's `max_size` is not a positive number of bytes.
    #[error("log max_size must be positive")]
    LogMaxSizeNotPositive,
    /// The `[log]` section's `max_generations` is not at least 1.
    #[error("log max_generations must be at least 1")]
    LogMaxGenerationsTooSmall,
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
    /// Returns an error when TOML parsing fails (including an unparseable `max_size` unit
    /// string), when a `name` is duplicated, when a `command` is empty, or when the `[log]`
    /// section's `max_size` is not positive or `max_generations` is not at least 1.
    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates the `[log]` section, `name` uniqueness, and that each `command` is non-empty.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.log.max_size == 0 {
            return Err(ConfigError::LogMaxSizeNotPositive);
        }
        if self.log.max_generations == 0 {
            return Err(ConfigError::LogMaxGenerationsTooSmall);
        }

        let mut seen = FxHashSet::default();
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
                env: FxHashMap::default(),
                cwd: None,
                enabled: true,
            }],
            log: LogConfig::default(),
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
                env: FxHashMap::from_iter([("RUST_LOG".to_owned(), "info".to_owned())]),
                cwd: Some(PathBuf::from("/Users/cffnpwr/work")),
                enabled: false,
            }],
            log: LogConfig::default(),
        };
        assert_eq!(result.ok(), Some(expected));
    }

    #[test]
    fn positive_parse_accepts_empty_input() {
        // Given empty input
        let input = "";

        // When it is parsed
        let result = Config::parse(input);

        // Then the config has no services and the log section takes its defaults
        let expected = Config {
            services: vec![],
            log: LogConfig::default(),
        };
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

    #[test]
    fn positive_parse_applies_log_defaults_when_the_section_is_omitted() {
        // Given input with no [log] section at all
        let input = "";

        // When it is parsed
        let result = Config::parse(input).expect("parse should succeed");

        // Then the resolved limits fall back to the implementation defaults
        assert_eq!(result.log.max_size, DEFAULT_MAX_BYTES);
        assert_eq!(result.log.max_generations, DEFAULT_MAX_GENERATIONS);
    }

    #[test]
    fn positive_parse_reads_the_log_section() {
        // Given a [log] section that sets every field
        let input = r#"
            [log]
            max_size = "20MiB"
            max_generations = 3
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then max_size is parsed into bytes and max_generations is read as written
        let expected = Config {
            services: vec![],
            log: LogConfig {
                max_size: 20 * 1024 * 1024,
                max_generations: 3,
            },
        };
        assert_eq!(result.ok(), Some(expected));
    }

    #[test]
    fn positive_parse_defaults_an_omitted_log_field_individually() {
        // Given a [log] section that only sets max_generations
        let input = r"
            [log]
            max_generations = 2
            ";

        // When it is parsed
        let result = Config::parse(input).expect("parse should succeed");

        // Then max_size still falls back to the default while max_generations is as written
        assert_eq!(result.log.max_size, DEFAULT_MAX_BYTES);
        assert_eq!(result.log.max_generations, 2);
    }

    #[test]
    fn positive_parse_distinguishes_binary_and_decimal_log_max_size_units() {
        // Given two [log] sections differing only in KiB (binary) vs KB (decimal)
        let binary = Config::parse(
            r#"
            [log]
            max_size = "10KiB"
            "#,
        )
        .expect("parse should succeed");
        let decimal = Config::parse(
            r#"
            [log]
            max_size = "10KB"
            "#,
        )
        .expect("parse should succeed");

        // When resolved to bytes
        // Then KiB uses the 1024-based factor and KB uses the 1000-based factor
        assert_eq!(binary.log.max_size, 10 * 1024);
        assert_eq!(decimal.log.max_size, 10_000);
    }

    #[test]
    fn positive_parse_log_max_size_unit_is_case_insensitive() {
        // Given the same value spelled with different unit casing
        let lower = Config::parse(
            r#"
            [log]
            max_size = "10mib"
            "#,
        )
        .expect("parse should succeed");
        let mixed = Config::parse(
            r#"
            [log]
            max_size = "10MiB"
            "#,
        )
        .expect("parse should succeed");

        // Then both resolve to the same byte count
        assert_eq!(lower.log.max_size, mixed.log.max_size);
        assert_eq!(lower.log.max_size, 10 * 1024 * 1024);
    }

    #[test]
    fn negative_parse_rejects_an_unrecognized_log_max_size_unit() {
        // Given a max_size using a unit that does not exist
        let input = r#"
            [log]
            max_size = "10XB"
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails while parsing TOML, since the unit string cannot be deserialized
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn negative_parse_rejects_a_negative_log_max_size() {
        // Given a max_size with a negative sign, which bytesize does not accept
        let input = r#"
            [log]
            max_size = "-10MiB"
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails while parsing TOML
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn negative_parse_rejects_a_zero_log_max_size() {
        // Given a max_size of zero, which deserializes fine but is semantically invalid
        let input = r#"
            [log]
            max_size = "0MiB"
            "#;

        // When it is parsed
        let result = Config::parse(input);

        // Then validation rejects the resolved zero byte count
        assert!(matches!(result, Err(ConfigError::LogMaxSizeNotPositive)));
    }

    #[test]
    fn negative_parse_rejects_a_zero_log_max_generations() {
        // Given a max_generations of zero
        let input = r"
            [log]
            max_generations = 0
            ";

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails
        assert!(matches!(
            result,
            Err(ConfigError::LogMaxGenerationsTooSmall)
        ));
    }

    #[test]
    fn negative_parse_rejects_an_unknown_log_field() {
        // Given a [log] section with an unknown field
        let input = r"
            [log]
            typo_field = true
            ";

        // When it is parsed
        let result = Config::parse(input);

        // Then it fails while parsing TOML
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }
}
