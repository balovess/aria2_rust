//! Validation framework for configuration options.
//!
//! Provides [`OptionError`], [`OptionValidator`], concrete validators
//! ([`RangeValidator`], [`ChoiceValidator`], [`UrlValidator`],
//! [`PathValidator`], [`RegexValidator`]), [`OptionDefinition`],
//! and [`DependencyChecker`].

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde_json::Value;

// =========================================================================
// Error Type
// =========================================================================

/// Error type for option validation failures.
#[derive(Debug, Clone)]
pub enum OptionError {
    TypeMismatch {
        expected: String,
        got: String,
    },
    OutOfRange {
        value: String,
        min: String,
        max: String,
    },
    InvalidChoice {
        value: String,
        allowed: Vec<String>,
    },
    InvalidUrl {
        url: String,
        reason: String,
    },
    InvalidPath {
        path: String,
        reason: String,
    },
    PatternMismatch {
        value: String,
        pattern: String,
    },
    DependencyConflict {
        option: String,
        conflicts_with: String,
    },
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
                write!(f, "value '{}' is out of range [{}..{}]", value, min, max)
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
                write!(f, "value '{}' does not match pattern '{}'", value, pattern)
            }
            Self::DependencyConflict {
                option,
                conflicts_with,
            } => {
                write!(f, "option '{}' conflicts with '{}'", option, conflicts_with)
            }
            Self::MissingDependency { option, requires } => {
                write!(f, "option '{}' requires '{}' to be set", option, requires)
            }
        }
    }
}

impl std::error::Error for OptionError {}

// =========================================================================
// Validator Trait
// =========================================================================

/// Trait for validating configuration option values.
pub trait OptionValidator: Send + Sync {
    fn validate(&self, name: &str, value: &Value) -> Result<(), OptionError>;
    fn description(&self) -> &str;
}

// =========================================================================
// Concrete Validators
// =========================================================================

/// Validates that numeric values fall within a specified range.
#[derive(Debug, Clone)]
pub struct RangeValidator<T> {
    min: T,
    max: T,
}

impl<T> RangeValidator<T>
where
    T: PartialOrd + fmt::Display + Clone + 'static,
{
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
#[derive(Debug, Clone)]
pub struct ChoiceValidator {
    allowed: Vec<String>,
}

impl ChoiceValidator {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }

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
#[derive(Debug, Clone, Copy)]
pub struct UrlValidator;

impl UrlValidator {
    pub fn new() -> Self {
        Self
    }

    fn is_valid_url(url: &str) -> Result<(), String> {
        if url.is_empty() {
            return Err("URL is empty".to_string());
        }

        if !url.contains("://") {
            return Err(format!("missing scheme separator in '{}'", url));
        }

        let scheme = url.split("://").next().unwrap_or("");
        if scheme.is_empty() {
            return Err("scheme is empty".to_string());
        }

        if !scheme
            .chars()
            .all(|c| c.is_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return Err(format!("invalid scheme '{}'", scheme));
        }

        let after_scheme = url.split_once("://").map(|x| x.1).unwrap_or("");
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
#[derive(Debug, Clone)]
pub struct PathValidator {
    must_exist: bool,
    writable: bool,
}

impl PathValidator {
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
                    let check_path = if path.exists() {
                        path.as_os_str().to_owned()
                    } else {
                        match path.parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                parent.as_os_str().to_owned()
                            }
                            _ => path.as_os_str().to_owned(),
                        }
                    };

                    if let Some(check_path_str) = check_path.to_str() {
                        let check_path = Path::new(check_path_str);
                        if check_path.exists() {
                            match std::fs::metadata(check_path) {
                                Ok(meta) => {
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
                                    }
                                    #[cfg(not(unix))]
                                    {
                                        let _ = &meta;
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
#[derive(Debug, Clone)]
pub struct RegexValidator {
    pattern: String,
    compiled: regex::Regex,
}

impl RegexValidator {
    pub fn new(pattern: &str) -> Self {
        let compiled =
            regex::Regex::new(pattern).expect("Invalid regex pattern in RegexValidator");
        Self {
            pattern: pattern.to_string(),
            compiled,
        }
    }

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

// =========================================================================
// Extended Definition with Validation
// =========================================================================

/// Extended definition for a configuration option with dynamic validation support.
pub struct OptionDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub default_value: Value,
    pub validator: Option<Box<dyn OptionValidator>>,
}

impl OptionDefinition {
    pub fn new(name: &'static str, description: &'static str, default_value: Value) -> Self {
        Self {
            name,
            description,
            default_value,
            validator: None,
        }
    }

    pub fn with_validator(mut self, validator: Box<dyn OptionValidator>) -> Self {
        self.validator = Some(validator);
        self
    }

    pub fn validate(&self, value: &Value) -> Result<(), OptionError> {
        match &self.validator {
            Some(validator) => validator.validate(self.name, value),
            None => Ok(()),
        }
    }

    pub fn get_default_or_fallback<'a>(&'a self, fallback: &'a Value) -> &'a Value {
        match &self.default_value {
            Value::Null => fallback,
            other => other,
        }
    }
}

// =========================================================================
// Dependency Checker
// =========================================================================

/// Checks dependencies between configuration options.
pub struct DependencyChecker {
    mutual_exclusions: Vec<(String, String)>,
    requirements: Vec<(String, String)>,
}

impl DependencyChecker {
    pub fn new() -> Self {
        Self {
            mutual_exclusions: Vec::new(),
            requirements: Vec::new(),
        }
    }

    pub fn add_mutual_exclusion(&mut self, opt_a: String, opt_b: String) {
        self.mutual_exclusions.push((opt_a, opt_b));
    }

    pub fn add_requirement(&mut self, option: String, requires: String) {
        self.requirements.push((option, requires));
    }

    pub fn check(&self, options: &HashMap<String, Value>) -> Vec<OptionError> {
        let mut errors = Vec::new();

        for (opt_a, opt_b) in &self.mutual_exclusions {
            let a_set = options.get(opt_a).is_some_and(|v| !v.is_null());
            let b_set = options.get(opt_b).is_some_and(|v| !v.is_null());

            if a_set && b_set {
                errors.push(OptionError::DependencyConflict {
                    option: opt_a.clone(),
                    conflicts_with: opt_b.clone(),
                });
            }
        }

        for (option, requires) in &self.requirements {
            let option_set = options.get(option).is_some_and(|v| !v.is_null());
            let required_set = options.get(requires).is_some_and(|v| !v.is_null());

            if option_set && !required_set {
                errors.push(OptionError::MissingDependency {
                    option: option.clone(),
                    requires: requires.clone(),
                });
            }
        }

        errors
    }

    pub fn mutual_exclusion_count(&self) -> usize {
        self.mutual_exclusions.len()
    }

    pub fn requirement_count(&self) -> usize {
        self.requirements.len()
    }
}

impl Default for DependencyChecker {
    fn default() -> Self {
        Self::new()
    }
}
