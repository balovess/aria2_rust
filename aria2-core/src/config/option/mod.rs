//! Configuration option types, validation, and registry.
//!
//! This module provides:
//! - [`OptionType`], [`OptionCategory`], [`OptionValue`], [`OptionDef`] — core type definitions
//! - [`OptionRegistry`] — the central registry for all aria2 configuration options
//! - [`OptionValidator`], [`OptionError`], validators — dynamic validation framework
//! - [`DependencyChecker`] — inter-option dependency checking
//!
//! Built-in option registrations are in [`option_definitions`](super::option_definitions).

mod registry;
mod types;
mod validator;

#[cfg(test)]
mod tests;

// Re-export all public API from sub-modules.
pub use registry::OptionRegistry;
pub use types::{OptionCategory, OptionDef, OptionType, OptionValue, parse_integer_segments};
pub use validator::{
    ChoiceValidator, DependencyChecker, OptionDefinition, OptionError, OptionValidator,
    PathValidator, RangeValidator, RegexValidator, UrlValidator,
};
