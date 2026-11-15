//! Tests for the option module.

use std::collections::HashMap;

use serde_json::Value;

use super::registry::OptionRegistry;
use super::types::{
    OptionCategory, OptionDef, OptionOwner, OptionType, OptionValue, PiecePriorityRule,
    parse_index_out, parse_piece_priority,
};
use super::validator::{
    ChoiceValidator, DependencyChecker, OptionDefinition, OptionError, OptionValidator,
    RangeValidator, RegexValidator, UrlValidator,
};

mod option_definitions;
mod registry;
mod validators;
