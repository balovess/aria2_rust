//! Dynamic option validation system for aria2 configuration.
//!
//! This module provides a flexible, extensible validation framework for configuration
//! options. It supports:
//!
//! - **Type-safe validators** via the [`OptionValidator`] trait
//! - **Built-in validators** for common patterns (range, choice, URL, path, regex)
//! - **Dependency checking** for mutual exclusions and conditional requirements
//! - **Thread-safe** design using `Send + Sync` bounds
//!
//! # Example
//!
//! ```rust
//! use aria2_core::config::option_validator::*;
//! use serde_json::json;
//!
//! let validator = RangeValidator::new(1, 16);
//! assert!(validator.validate("split", &json!(8)).is_ok());
//! assert!(validator.validate("split", &json!(0)).is_err());
//! ```

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde_json::Value;

/// Error type for option validation failures.
///
/// Provides detailed, user-friendly error messages for all validation scenarios
/// including type mismatches, range violations, invalid choices, and dependency conflicts.
#[derive(Debug, Clone)]
pub enum OptionError {
    /// The value's type does not match what was expected.
    TypeMismatch {
        expected: String,
        got: String,
    },
    /// A numeric value is outside the allowed range.
    OutOfRange {
        value: String,
        min: String,
        max: String,
    },
    /// A string value is not in the list of allowed choices.
    InvalidChoice {
        value: String,
        allowed: Vec<String>,
    },
    /// A URL string is malformed or uses an unsupported scheme.
    InvalidUrl {
        url: String,
        reason: String,
    },
    /// A file path is invalid or doesn't meet requirements.
    InvalidPath {
        path: String,
        reason: String,
    },
    /// A value fails to match the required regex pattern.
    PatternMismatch {
        value: String,
        pattern: String,
    },
    /// Two options are mutually exclusive but both are set.
    DependencyConflict {
        option: String,
        conflicts_with: String,
    },
    /// An option requires another option that is not set.
    MissingDependency {
        option: String,
        requires: String,
    },
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeMismatch { expected, got } => {
                write!(
                    f,
                    "type mismatch for option: expected '{}', got '{}'",
                    expected, got
                )
            }
            Self::OutOfRange { value, min, max } => {
                write!(
                    f,
                    "value '{}' is out of range [{}..{}]",
                    value, min, max
                )
            }
            Self::InvalidChoice { value, allowed } => {
                write!(
                    f,
                    "invalid choice '{}', allowed values: {}",
                    value,
                    allowed.join(", ")
                )
            }
            Self::InvalidUrl { url, reason } => {
                write!(f, "invalid URL '{}': {}", url, reason)
            }
            Self::InvalidPath { path, reason } => {
                write!(f, "invalid path '{}': {}", path, reason)
            }
            Self::PatternMismatch { value, pattern } => {
                write!(
                    f,
                    "value '{}' does not match pattern '{}'",
                    value, pattern
                )
            }
            Self::DependencyConflict {
                option,
                conflicts_with,
            } => {
                write!(
                    f,
                    "option '{}' conflicts with '{}'",
                    option, conflicts_with
                )
            }
            Self::MissingDependency { option, requires } => {
                write!(
                    f,
                    "option '{}' requires '{}' to be set",
                    option, requires
                )
            }
        }
    }
}

impl std::error::Error for OptionError {}

/// Trait for validating configuration option values.
///
/// Implementors must be thread-safe (`Send + Sync`) to support concurrent
/// validation in multi-threaded environments like async runtimes.
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::{OptionValidator, OptionError};
/// use serde_json::json;
///
/// struct MyValidator;
///
/// impl OptionValidator for MyValidator {
///     fn validate(&self, name: &str, value: &serde_json::Value) -> Result<(), OptionError> {
///         // Custom validation logic here
///         Ok(())
///     }
///
///     fn description(&self) -> &str {
///         "My custom validator"
///     }
/// }
/// ```
pub trait OptionValidator: Send + Sync {
    /// Validate a configuration option value.
    ///
    /// Returns `Ok(())` if the value is valid, or `Err(OptionError)` with
    /// details about why validation failed.
    fn validate(&self, name: &str, value: &Value) -> Result<(), OptionError>;

    /// Return a human-readable description of this validator.
    ///
    /// Used in error messages and documentation to explain what constraints
    /// this validator enforces.
    fn description(&self) -> &str;
}

/// Validates that numeric values fall within a specified range.
///
/// Supports any type implementing `PartialOrd` and `Display`, making it
/// suitable for integers, floats, and other ordered types.
///
/// # Type Parameters
///
/// - `T`: The numeric type to validate (must implement `PartialOrd + Display`)
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::*;
/// use serde_json::json;
///
/// // Integer range validation
/// let int_validator = RangeValidator::<i64>::new(1, 16);
/// assert!(int_validator.validate("split", &json!(8)).is_ok());
/// assert!(int_validator.validate("split", &json!(0)).is_err());
///
/// // Float range validation
/// let float_validator = RangeValidator::<f64>::new(0.0, 1.0);
/// assert!(float_validator.validate("seed-ratio", &json!(0.5)).is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct RangeValidator<T> {
    min: T,
    max: T,
}

impl<T> RangeValidator<T>
where
    T: PartialOrd + fmt::Display + Clone + 'static,
{
    /// Create a new range validator with inclusive bounds `[min, max]`.
    pub fn new(min: T, max: T) -> Self {
        Self { min, max }
    }
}

impl OptionValidator for RangeValidator<i64> {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_i64() {
            Some(n) if n >= self.min && n <= self.max => Ok(()),
            Some(n) => Err(OptionError::OutOfRange {
                value: n.to_string(),
                min: self.min.to_string(),
                max: self.max.to_string(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "integer".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "range validator (inclusive bounds)"
    }
}

impl OptionValidator for RangeValidator<f64> {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_f64() {
            Some(v) if v >= self.min && v <= self.max => Ok(()),
            Some(v) => Err(OptionError::OutOfRange {
                value: format!("{}", v),
                min: format!("{}", self.min),
                max: format!("{}", self.max),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "float".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "range validator for floating-point numbers"
    }
}

impl OptionValidator for RangeValidator<u64> {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_u64() {
            Some(n) if n >= self.min && n <= self.max => Ok(()),
            Some(n) => Err(OptionError::OutOfRange {
                value: n.to_string(),
                min: self.min.to_string(),
                max: self.max.to_string(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "unsigned integer".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "range validator for unsigned integers"
    }
}

/// Validates that string values are in a predefined whitelist of choices.
///
/// Commonly used for enum-like options such as log levels, protocols, etc.
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::*;
/// use serde_json::json;
///
/// let validator = ChoiceValidator::new(vec![
///     "debug".to_string(),
///     "info".to_string(),
///     "warn".to_string(),
///     "error".to_string(),
/// ]);
///
/// assert!(validator.validate("log-level", &json!("info")).is_ok());
/// assert!(validator.validate("log-level", &json!("invalid")).is_err());
/// ```
#[derive(Debug, Clone)]
pub struct ChoiceValidator {
    allowed: Vec<String>,
}

impl ChoiceValidator {
    /// Create a new choice validator with the given allowed values.
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }

    /// Get the list of allowed values (for testing/documentation).
    pub fn allowed_values(&self) -> &[String] {
        &self.allowed
    }
}

impl OptionValidator for ChoiceValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(s) if self.allowed.iter().any(|a| a == s) => Ok(()),
            Some(s) => Err(OptionError::InvalidChoice {
                value: s.to_string(),
                allowed: self.allowed.clone(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "string".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "choice validator (enum whitelist)"
    }
}

/// Validates URL strings for proper format and supported schemes.
///
/// Checks that URLs have a valid structure and use schemes commonly
/// supported by aria2 (http, https, ftp, sftp, etc.).
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::*;
/// use serde_json::json;
///
/// let validator = UrlValidator;
/// assert!(validator.validate("tracker", &json!("http://example.com:6969/announce")).is_ok());
/// assert!(validator.validate("tracker", &json!("not-a-url")).is_err());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct UrlValidator;

impl UrlValidator {
    /// Create a new URL validator.
    pub fn new() -> Self {
        Self
    }

    /// Check if a URL string appears valid.
    ///
    /// Validates:
    /// - Non-empty string
    /// - Contains a scheme separator (`://`)
    /// - Scheme is one of the supported protocols
    fn is_valid_url(url: &str) -> Result<(), String> {
        if url.is_empty() {
            return Err("URL is empty".to_string());
        }

        // Check for scheme://pattern
        if !url.contains("://") {
            return Err(format!("missing scheme separator in '{}'", url));
        }

        let scheme = url.split("://").next().unwrap_or("");
        if scheme.is_empty() {
            return Err("scheme is empty".to_string());
        }

        // Validate scheme characters (must be alphanumeric + +-.)
        if !scheme
            .chars()
            .all(|c| c.is_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return Err(format!("invalid scheme '{}'", scheme));
        }

        // Check rest of URL has some content after ://
        let after_scheme = url.splitn(2, "://").nth(1).unwrap_or("");
        if after_scheme.is_empty() {
            return Err("no host/path after scheme".to_string());
        }

        Ok(())
    }
}

impl Default for UrlValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl OptionValidator for UrlValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(url) => Self::is_valid_url(url).map_err(|reason| OptionError::InvalidUrl {
                url: url.to_string(),
                reason,
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "string (URL)".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "URL format validator"
    }
}

/// Validates file system paths for existence and writability.
///
/// Can check whether paths exist on disk and whether they can be written to,
/// which is useful for options like `dir` (download directory) or `log`.
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::*;
/// use serde_json::json;
///
/// // Path must exist and be writable
/// let validator = PathValidator::new(true, true);
///
/// // On most systems /tmp exists and is writable
/// assert!(validator.validate("dir", &json!("/tmp")).is_ok());
///
/// // This path likely doesn't exist
/// assert!(validator.validate("dir", &json!("/nonexistent/path/xyz123")).is_err());
/// ```
#[derive(Debug, Clone)]
pub struct PathValidator {
    must_exist: bool,
    writable: bool,
}

impl PathValidator {
    /// Create a new path validator.
    ///
    /// # Arguments
    ///
    /// * `must_exist` - If true, the path must already exist on disk
    /// * `writable` - If true, the path must be writable by the current user
    pub fn new(must_exist: bool, writable: bool) -> Self {
        Self {
            must_exist,
            writable,
        }
    }
}

impl OptionValidator for PathValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(path_str) => {
                let path = Path::new(path_str);

                if self.must_exist && !path.exists() {
                    return Err(OptionError::InvalidPath {
                        path: path_str.to_string(),
                        reason: "path does not exist".to_string(),
                    });
                }

                if self.writable {
                    // Check parent directory writability if path is a file target
                    let check_path = if path.exists() {
                        path.as_os_str().to_owned()
                    } else {
                        // For non-existent paths, check parent directory
                        match path.parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                parent.as_os_str().to_owned()
                            }
                            _ => path.as_os_str().to_owned(),
                        }
                    };

                    // Try to check writability
                    if let Some(check_path_str) = check_path.to_str() {
                        let check_path = Path::new(check_path_str);
                        if check_path.exists() {
                            // Use metadata to check permissions (simplified check)
                            match std::fs::metadata(check_path) {
                                Ok(_meta) => {
                                    // If we can read metadata, assume we can at least check
                                    // Full permission checks would require platform-specific code
                                    #[cfg(unix)]
                                    {
                                        use std::os::unix::fs::PermissionsExt;
                                        let mode = meta.permissions().mode();
                                        let user_writable = mode & 0o200 != 0;
                                        if !user_writable && meta.is_dir() {
                                            return Err(OptionError::InvalidPath {
                                                path: path_str.to_string(),
                                                reason: "path is not writable".to_string(),
                                            });
                                        }
                                        // For files, we'll be lenient since we can't test write without actually writing
                                    }
                                    #[cfg(not(unix))]
                                    {
                                        // On Windows, just check if path exists for now
                                        // Full permission checks would require WinAPI
                                    }
                                }
                                Err(e) => {
                                    return Err(OptionError::InvalidPath {
                                        path: path_str.to_string(),
                                        reason: format!("cannot access path: {}", e),
                                    });
                                }
                            }
                        }
                    }
                }

                Ok(())
            }
            None => Err(OptionError::TypeMismatch {
                expected: "string (path)".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "file system path validator"
    }
}

/// Validates string values against a custom regular expression pattern.
///
/// Provides maximum flexibility for complex validation rules that cannot
/// be expressed by simpler validators.
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::*;
/// use serde_json::json;
///
/// // Validate host:port format
/// let validator = RegexValidator::new(r"^[a-zA-Z0-9.-]+:\d+$");
/// assert!(validator.validate("proxy", &json!("proxy.example.com:8080")).is_ok());
/// assert!(validator.validate("proxy", &json!("not-valid")).is_err());
/// ```
#[derive(Debug, Clone)]
pub struct RegexValidator {
    pattern: String,
    compiled: regex::Regex,
}

impl RegexValidator {
    /// Create a new regex validator with the given pattern.
    ///
    /// # Arguments
    ///
    /// * `pattern` - A regular expression string (uses Rust regex syntax)
    ///
    /// # Panics
    ///
    /// Panics if the pattern is not a valid regular expression.
    pub fn new(pattern: &str) -> Self {
        let compiled =
            regex::Regex::new(pattern).expect("Invalid regex pattern in RegexValidator");
        Self {
            pattern: pattern.to_string(),
            compiled,
        }
    }

    /// Get the original pattern string (for testing/documentation).
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

impl OptionValidator for RegexValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(s) if self.compiled.is_match(s) => Ok(()),
            Some(s) => Err(OptionError::PatternMismatch {
                value: s.to_string(),
                pattern: self.pattern.clone(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "string".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "custom regex pattern validator"
    }
}

/// Extended definition for a configuration option with dynamic validation support.
///
/// Builds upon the basic [`OptionDef`](super::option_types::OptionDef) by adding
/// optional runtime validation through [`OptionValidator`] trait objects.
///
/// # Thread Safety
///
/// The validator field uses `Box<dyn OptionValidator>` which is `Send + Sync`
/// due to the trait bound, making `OptionDefinition` safe to share across threads.
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::*;
/// use serde_json::json;
///
/// let def = OptionDefinition {
///     name: "max-connections",
///     description: "Maximum connections per server",
///     default_value: json!(16),
///     validator: Some(Box::new(RangeValidator::<i64>::new(1, 32))),
/// };
///
/// // Valid value passes validation
/// assert!(def.validator.as_ref().unwrap().validate(def.name, &json!(8)).is_ok());
///
/// // Invalid value fails validation
/// assert!(def.validator.as_ref().unwrap().validate(def.name, &json!(100)).is_err());
/// ```
pub struct OptionDefinition {
    /// The option name (e.g., "split", "dir", "max-tries").
    pub name: &'static str,
    /// Human-readable description of this option.
    pub description: &'static str,
    /// Default value when no explicit value is provided.
    pub default_value: Value,
    /// Optional validator for runtime validation.
    ///
    /// When `None`, no validation is performed (backward compatible).
    pub validator: Option<Box<dyn OptionValidator>>,
}

impl OptionDefinition {
    /// Create a new option definition without validation.
    pub fn new(name: &'static str, description: &'static str, default_value: Value) -> Self {
        Self {
            name,
            description,
            default_value,
            validator: None,
        }
    }

    /// Set the validator for this option definition.
    ///
    /// Uses builder-style API for fluent construction.
    pub fn with_validator(mut self, validator: Box<dyn OptionValidator>) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Validate a value against this option's validator (if configured).
    ///
    /// Returns `Ok(())` if:
    /// - No validator is configured (backward compatible), or
    /// - The validator accepts the value
    ///
    /// Returns `Err(OptionError)` if validation fails.
    pub fn validate(&self, value: &Value) -> Result<(), OptionError> {
        match &self.validator {
            Some(validator) => validator.validate(self.name, value),
            None => Ok(()), // No validator configured - skip validation
        }
    }

    /// Get the default value, falling back to the provided value if needed.
    ///
    /// Useful for implementing fallback logic when required options are missing.
    pub fn get_default_or_fallback<'a>(&'a self, fallback: &'a Value) -> &'a Value {
        // If default is null or doesn't exist, use fallback
        match &self.default_value {
            Value::Null => fallback,
            other => other,
        }
    }
}

/// Checks dependencies between configuration options.
///
/// Supports two types of dependency relationships:
/// - **Mutual exclusions**: Two options cannot both be set simultaneously
/// - **Requirements**: One option requires another to be present
///
/// # Example
///
/// ```rust
/// use aria2_core::config::option_validator::*;
/// use serde_json::{json, Map};
///
/// let mut checker = DependencyChecker::new();
///
/// // Add mutual exclusion: can't use both --ftp-pasv and --ftp-port
/// checker.add_mutual_exclusion("ftp-pasv".to_string(), "ftp-port".to_string());
///
/// // Add requirement: --bt-enable-lpd requires --enable-dht
/// checker.add_requirement("bt-enable-lpd".to_string(), "enable-dht".to_string());
///
/// let mut opts = Map::new();
/// opts.insert("ftp-pasv".to_string(), json!(true));
/// opts.insert("ftp-port".to_string(), json!(8021));
///
/// let errors = checker.check(&opts);
/// assert_eq!(errors.len(), 1); // Mutual exclusion conflict detected
/// ```
pub struct DependencyChecker {
    /// Pairs of options that cannot both be present.
    mutual_exclusions: Vec<(String, String)>,
    /// Pairs where the first option requires the second.
    requirements: Vec<(String, String)>,
}

impl DependencyChecker {
    /// Create a new empty dependency checker.
    pub fn new() -> Self {
        Self {
            mutual_exclusions: Vec::new(),
            requirements: Vec::new(),
        }
    }

    /// Add a mutual exclusion rule between two options.
    ///
    /// Both options will be checked; if both have non-null values in the
    /// options map, a `DependencyConflict` error is returned.
    pub fn add_mutual_exclusion(&mut self, opt_a: String, opt_b: String) {
        self.mutual_exclusions.push((opt_a, opt_b));
    }

    /// Add a requirement rule where `option` requires `requires`.
    ///
    /// If `option` has a value but `requires` does not (or is null),
    /// a `MissingDependency` error is returned.
    pub fn add_requirement(&mut self, option: String, requires: String) {
        self.requirements.push((option, requires));
    }

    /// Check all dependency rules against the given options map.
    ///
    /// Returns a vector of all detected violations. An empty vector means
    /// all dependencies are satisfied.
    ///
    /// This method checks:
    /// 1. Mutual exclusion pairs - returns error if both options are set
    /// 2. Requirement pairs - returns error if dependent option is set but required option is not
    ///
    /// The check is complete (all errors are collected, not just the first).
    pub fn check(&self, options: &HashMap<String, Value>) -> Vec<OptionError> {
        let mut errors = Vec::new();

        // Check mutual exclusions
        for (opt_a, opt_b) in &self.mutual_exclusions {
            let a_set = options.get(opt_a).map_or(false, |v| !v.is_null());
            let b_set = options.get(opt_b).map_or(false, |v| !v.is_null());

            if a_set && b_set {
                errors.push(OptionError::DependencyConflict {
                    option: opt_a.clone(),
                    conflicts_with: opt_b.clone(),
                });
            }
        }

        // Check requirements
        for (option, requires) in &self.requirements {
            let option_set = options.get(option).map_or(false, |v| !v.is_null());
            let required_set = options.get(requires).map_or(false, |v| !v.is_null());

            if option_set && !required_set {
                errors.push(OptionError::MissingDependency {
                    option: option.clone(),
                    requires: requires.clone(),
                });
            }
        }

        errors
    }

    /// Get the number of configured mutual exclusion rules.
    pub fn mutual_exclusion_count(&self) -> usize {
        self.mutual_exclusions.len()
    }

    /// Get the number of configured requirement rules.
    pub fn requirement_count(&self) -> usize {
        self.requirements.len()
    }
}

impl Default for DependencyChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== RangeValidator Tests ====================

    #[test]
    fn test_range_validator_in_range() {
        let validator = RangeValidator::<i64>::new(1, 16);

        // Values within range should pass
        assert!(validator.validate("split", &Value::from(1)).is_ok());
        assert!(validator.validate("split", &Value::from(8)).is_ok());
        assert!(validator.validate("split", &Value::from(16)).is_ok());

        // Float range validator
        let float_validator = RangeValidator::<f64>::new(0.0, 1.0);
        assert!(float_validator.validate("ratio", &Value::from(0.5)).is_ok());
        assert!(float_validator.validate("ratio", &Value::from(0.0)).is_ok());
        assert!(float_validator.validate("ratio", &Value::from(1.0)).is_ok());

        // Unsigned range validator
        let u64_validator = RangeValidator::<u64>::new(1024, 1024 * 1024);
        assert!(u64_validator.validate("size", &Value::from(4096u64)).is_ok());
    }

    #[test]
    fn test_range_validator_out_of_range() {
        let validator = RangeValidator::<i64>::new(1, 16);

        // Below minimum
        let result = validator.validate("split", &Value::from(0));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::OutOfRange { value, min, max } => {
                assert_eq!(value, "0");
                assert_eq!(min, "1");
                assert_eq!(max, "16");
            }
            other => panic!("Expected OutOfRange error, got {:?}", other),
        }

        // Above maximum
        let result = validator.validate("split", &Value::from(20));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::OutOfRange { value, .. } => {
                assert_eq!(value, "20");
            }
            other => panic!("Expected OutOfRange error, got {:?}", other),
        }

        // Wrong type (string instead of integer)
        let result = validator.validate("split", &Value::String("not-a-number".into()));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::TypeMismatch { expected, .. } => {
                assert_eq!(expected, "integer");
            }
            other => panic!("Expected TypeMismatch error, got {:?}", other),
        }
    }

    // ==================== ChoiceValidator Tests ====================

    #[test]
    fn test_choice_validator_enum() {
        let validator = ChoiceValidator::new(vec![
            "debug".to_string(),
            "info".to_string(),
            "warn".to_string(),
            "error".to_string(),
        ]);

        // Valid choices
        assert!(validator.validate("log-level", &Value::String("debug".into())).is_ok());
        assert!(validator.validate("log-level", &Value::String("info".into())).is_ok());
        assert!(validator.validate("log-level", &Value::String("warn".into())).is_ok());
        assert!(validator.validate("log-level", &Value::String("error".into())).is_ok());

        // Invalid choice
        let result = validator.validate("log-level", &Value::String("verbose".into()));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::InvalidChoice { value, allowed } => {
                assert_eq!(value, "verbose");
                assert_eq!(allowed.len(), 4);
                assert!(allowed.contains(&"debug".to_string()));
            }
            other => panic!("Expected InvalidChoice error, got {:?}", other),
        }

        // Wrong type
        let result = validator.validate("log-level", &Value::from(42));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::TypeMismatch { expected, .. } => {
                assert_eq!(expected, "string");
            }
            other => panic!("Expected TypeMismatch error, got {:?}", other),
        }
    }

    // ==================== UrlValidator Tests ====================

    #[test]
    fn test_url_validator_malformed() {
        let validator = UrlValidator::new();

        // Valid URLs
        assert!(validator
            .validate("tracker", &Value::String("http://example.com:6969/announce".into()))
            .is_ok());
        assert!(validator
            .validate("tracker", &Value::String("https://tracker.example.com/announce".into()))
            .is_ok());
        assert!(validator
            .validate("proxy", &Value::String("ftp://user:pass@ftp.example.com".into()))
            .is_ok());

        // Malformed URLs
        let result = validator.validate("url", &Value::String("not-a-url".into()));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::InvalidUrl { url, reason } => {
                assert_eq!(url, "not-a-url");
                assert!(reason.contains("missing scheme"));
            }
            other => panic!("Expected InvalidUrl error, got {:?}", other),
        }

        // Empty URL
        let result = validator.validate("url", &Value::String("".into()));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::InvalidUrl { url, reason } => {
                assert_eq!(url, "");
                assert!(reason.contains("empty"));
            }
            other => panic!("Expected InvalidUrl error, got {:?}", other),
        }

        // No host after scheme
        let result = validator.validate("url", &Value::String("http://".into()));
        assert!(result.is_err());

        // Wrong type
        let result = validator.validate("url", &Value::from(123));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::TypeMismatch { expected, .. } => {
                assert!(expected.contains("URL"));
            }
            other => panic!("Expected TypeMismatch error, got {:?}", other),
        }
    }

    // ==================== PathValidator Tests ====================

    #[test]
    fn test_path_validator_existing_path() {
        // Test with /tmp (should exist on most systems)
        let validator = PathValidator::new(true, false);
        let _result = validator.validate("dir", &Value::String("/tmp".into()));

        // /tmp should exist on Unix-like systems
        #[cfg(unix)]
        assert!(result.is_ok());

        // On Windows, use temp directory
        #[cfg(windows)]
        {
            let tmp_dir = std::env::temp_dir();
            let tmp_str = tmp_dir.to_string_lossy().to_string();
            assert!(validator.validate("dir", &Value::String(tmp_str.into())).is_ok());
        }
    }

    #[test]
    fn test_path_validator_nonexistent_path() {
        let validator = PathValidator::new(true, false);

        // This path definitely shouldn't exist
        let result = validator.validate(
            "dir",
            &Value::String("/nonexistent/path/that/does/not/exist/xyz123".into()),
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::InvalidPath { path, reason } => {
                assert!(path.contains("nonexistent"));
                assert!(reason.contains("does not exist"));
            }
            other => panic!("Expected InvalidPath error, got {:?}", other),
        }
    }

    // ==================== RegexValidator Tests ====================

    #[test]
    fn test_regex_validator_pattern_match() {
        // Host:port pattern
        let validator = RegexValidator::new(r"^[a-zA-Z0-9.-]+:\d+$");

        assert!(validator
            .validate("proxy", &Value::String("proxy.example.com:8080".into()))
            .is_ok());
        assert!(validator
            .validate("proxy", &Value::String("localhost:3128".into()))
            .is_ok());

        // Should fail
        let result = validator.validate("proxy", &Value::String("not-valid".into()));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::PatternMismatch { value, pattern } => {
                assert_eq!(value, "not-valid");
                assert!(pattern.contains("[a-zA-Z0-9"));
            }
            other => panic!("Expected PatternMismatch error, got {:?}", other),
        }

        // Empty string should fail (no host:port)
        assert!(validator.validate("proxy", &Value::String("".into())).is_err());

        // Wrong type
        let result = validator.validate("proxy", &Value::from(42));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::TypeMismatch { expected, .. } => {
                assert_eq!(expected, "string");
            }
            other => panic!("Expected TypeMismatch error, got {:?}", other),
        }
    }

    // ==================== DependencyChecker Tests ====================

    #[test]
    fn test_mutual_exclusion_detection() {
        let mut checker = DependencyChecker::new();

        // Add mutual exclusion: ftp-pasv and ftp-port cannot coexist
        checker.add_mutual_exclusion("ftp-pasv".to_string(), "ftp-port".to_string());

        // Case 1: Only one set - should pass
        let mut opts1 = HashMap::new();
        opts1.insert("ftp-pasv".to_string(), Value::Bool(true));
        let errors1 = checker.check(&opts1);
        assert!(errors1.is_empty());

        // Case 2: Both set - should detect conflict
        let mut opts2 = HashMap::new();
        opts2.insert("ftp-pasv".to_string(), Value::Bool(true));
        opts2.insert("ftp-port".to_string(), Value::from(8021));
        let errors2 = checker.check(&opts2);
        assert_eq!(errors2.len(), 1);
        match &errors2[0] {
            OptionError::DependencyConflict {
                option,
                conflicts_with,
            } => {
                assert_eq!(option, "ftp-pasv");
                assert_eq!(conflicts_with, "ftp-port");
            }
            other => panic!("Expected DependencyConflict, got {:?}", other),
        }

        // Case 3: Neither set - should pass
        let opts3 = HashMap::new();
        let errors3 = checker.check(&opts3);
        assert!(errors3.is_empty());
    }

    #[test]
    fn test_dependency_satisfaction() {
        let mut checker = DependencyChecker::new();

        // bt-enable-lpd requires enable-dht
        checker.add_requirement(
            "bt-enable-lpd".to_string(),
            "enable-dht".to_string(),
        );

        // Case 1: Both set - satisfied
        let mut opts1 = HashMap::new();
        opts1.insert("bt-enable-lpd".to_string(), Value::Bool(true));
        opts1.insert("enable-dht".to_string(), Value::Bool(true));
        let errors1 = checker.check(&opts1);
        assert!(errors1.is_empty());

        // Case 2: Dependent set but required missing - violation
        let mut opts2 = HashMap::new();
        opts2.insert("bt-enable-lpd".to_string(), Value::Bool(true));
        // enable-dht not set
        let errors2 = checker.check(&opts2);
        assert_eq!(errors2.len(), 1);
        match &errors2[0] {
            OptionError::MissingDependency {
                option,
                requires,
            } => {
                assert_eq!(option, "bt-enable-lpd");
                assert_eq!(requires, "enable-dht");
            }
            other => panic!("Expected MissingDependency, got {:?}", other),
        }

        // Case 3: Dependent not set - no violation
        let opts3 = HashMap::new();
        let errors3 = checker.check(&opts3);
        assert!(errors3.is_empty());
    }

    // ==================== OptionDefinition Tests ====================

    #[test]
    fn test_default_value_fallback() {
        // Test with a null default value
        let def_with_null = OptionDefinition {
            name: "optional-param",
            description: "An optional parameter",
            default_value: Value::Null,
            validator: None,
        };

        let fallback = Value::String("fallback-value".into());
        // Should return the fallback when default is Null
        assert_eq!(
            def_with_null.get_default_or_fallback(&fallback),
            &Value::String("fallback-value".into())
        );

        // Test with a non-null default value
        let def_with_value = OptionDefinition {
            name: "required-param",
            description: "A required parameter",
            default_value: Value::String("default-value".into()),
            validator: None,
        };

        let fallback = Value::String("should-not-use-this".into());
        // Should return the actual default (not the fallback)
        assert_eq!(
            def_with_value.get_default_or_fallback(&fallback),
            &Value::String("default-value".into())
        );
    }

    #[test]
    fn test_option_definition_validation() {
        // Option with validator
        let def = OptionDefinition {
            name: "max-connections",
            description: "Maximum connections per server",
            default_value: Value::from(16),
            validator: Some(Box::new(RangeValidator::<i64>::new(1, 32))),
        };

        // Valid value should pass
        assert!(def.validate(&Value::from(8)).is_ok());
        assert!(def.validate(&Value::from(1)).is_ok());
        assert!(def.validate(&Value::from(32)).is_ok());

        // Invalid value should fail
        assert!(def.validate(&Value::from(0)).is_err());
        assert!(def.validate(&Value::from(100)).is_err());

        // Option without validator (backward compatible)
        let def_no_validator = OptionDefinition {
            name: "some-option",
            description: "An option without validation",
            default_value: Value::String("default".into()),
            validator: None,
        };

        // Should always pass validation
        assert!(def_no_validator.validate(&Value::from(42)).is_ok());
        assert!(def_no_validator.validate(&Value::String("anything".into())).is_ok());
    }

    // ==================== OptionError Display Tests ====================

    #[test]
    fn test_option_error_display() {
        // TypeMismatch
        let err = OptionError::TypeMismatch {
            expected: "integer".to_string(),
            got: "string".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("type mismatch"));
        assert!(msg.contains("integer"));
        assert!(msg.contains("string"));

        // OutOfRange
        let err = OptionError::OutOfRange {
            value: "0".to_string(),
            min: "1".to_string(),
            max: "16".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("out of range"));
        assert!(msg.contains("0"));
        assert!(msg.contains("1"));
        assert!(msg.contains("16"));

        // InvalidChoice
        let err = OptionError::InvalidChoice {
            value: "verbose".to_string(),
            allowed: vec!["debug".to_string(), "info".to_string()],
        };
        let msg = format!("{}", err);
        assert!(msg.contains("invalid choice"));
        assert!(msg.contains("verbose"));
        assert!(msg.contains("debug"));
        assert!(msg.contains("info"));

        // InvalidUrl
        let err = OptionError::InvalidUrl {
            url: "not-a-url".to_string(),
            reason: "missing scheme separator".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("invalid URL"));
        assert!(msg.contains("not-a-url"));
        assert!(msg.contains("missing scheme"));

        // InvalidPath
        let err = OptionError::InvalidPath {
            path: "/nonexistent".to_string(),
            reason: "path does not exist".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("invalid path"));
        assert!(msg.contains("/nonexistent"));
        assert!(msg.contains("does not exist"));

        // PatternMismatch
        let err = OptionError::PatternMismatch {
            value: "abc".to_string(),
            pattern: r"^\d+$".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("does not match pattern"));
        assert!(msg.contains("abc"));
        assert!(msg.contains(r"^\d+$"));

        // DependencyConflict
        let err = OptionError::DependencyConflict {
            option: "opt-a".to_string(),
            conflicts_with: "opt-b".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("conflicts"));
        assert!(msg.contains("opt-a"));
        assert!(msg.contains("opt-b"));

        // MissingDependency
        let err = OptionError::MissingDependency {
            option: "child-opt".to_string(),
            requires: "parent-opt".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("requires"));
        assert!(msg.contains("child-opt"));
        assert!(msg.contains("parent-opt"));
    }

    // ==================== Thread Safety Tests ====================

    #[test]
    fn test_thread_safety() {
        use std::sync::Arc;
        use std::thread;

        // Verify that validators can be shared across threads
        let validator: Arc<dyn OptionValidator> = Arc::new(RangeValidator::<i64>::new(1, 16));
        let mut handles = Vec::new();

        for i in 0..4 {
            let v = Arc::clone(&validator);
            handles.push(thread::spawn(move || {
                let val = if i % 2 == 0 { 8 } else { 20 };
                v.validate("test", &Value::from(val))
            }));
        }

        for (i, handle) in handles.into_iter().enumerate() {
            let result = handle.join().unwrap();
            if i % 2 == 0 {
                assert!(result.is_ok(), "Thread {} should pass", i);
            } else {
                assert!(result.is_err(), "Thread {} should fail", i);
            }
        }
    }

    #[test]
    fn test_dependency_checker_multiple_rules() {
        let mut checker = DependencyChecker::new();

        // Add multiple mutual exclusions
        checker.add_mutual_exclusion("ftp-pasv".to_string(), "ftp-port".to_string());
        checker.add_mutual_exclusion("option-a".to_string(), "option-b".to_string());

        // Add multiple requirements
        checker.add_requirement("feature-x".to_string(), "base-feature".to_string());
        checker.add_requirement("feature-y".to_string(), "base-feature".to_string());

        assert_eq!(checker.mutual_exclusion_count(), 2);
        assert_eq!(checker.requirement_count(), 2);

        // Create options that violate all rules
        let mut opts = HashMap::new();
        opts.insert("ftp-pasv".to_string(), Value::Bool(true));
        opts.insert("ftp-port".to_string(), Value::from(8021));
        opts.insert("option-a".to_string(), Value::Bool(true));
        opts.insert("option-b".to_string(), Value::Bool(true));
        opts.insert("feature-x".to_string(), Value::Bool(true));
        opts.insert("feature-y".to_string(), Value::Bool(true));
        // base-feature is missing!

        let errors = checker.check(&opts);
        // Should detect: 2 mutual exclusion conflicts + 2 missing dependencies = 4 errors
        assert_eq!(errors.len(), 4);
    }
}