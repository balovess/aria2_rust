//! Typed codecs for aria2's JSON wire conventions.
//!
//! The RPC protocol intentionally uses strings for most numeric values and
//! for booleans. These codecs keep that representation at the serialization
//! seam while allowing internal Rust models to use native numeric and bool
//! types. Deserialization accepts the canonical string form and native JSON
//! numbers/bools for in-process callers.

use serde::{Deserialize, Deserializer, Serializer};
use std::fmt::Display;
use std::str::FromStr;

pub(super) fn serialize_display_as_string<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Display,
    S: Serializer,
{
    serializer.collect_str(value)
}

pub(super) fn serialize_option_display_as_string<T, S>(
    value: &Option<T>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: Display,
    S: Serializer,
{
    match value {
        Some(value) => serializer.collect_str(value),
        None => serializer.serialize_none(),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber<T> {
    String(String),
    Number(T),
}

pub(super) fn deserialize_string_or_number<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + FromStr,
    T::Err: Display,
{
    match StringOrNumber::<T>::deserialize(deserializer)? {
        StringOrNumber::String(value) => value.parse().map_err(serde::de::Error::custom),
        StringOrNumber::Number(value) => Ok(value),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OptionalStringOrNumber<T> {
    String(String),
    Number(T),
    Null,
}

pub(super) fn deserialize_option_string_or_number<'de, D, T>(
    deserializer: D,
) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + FromStr,
    T::Err: Display,
{
    match OptionalStringOrNumber::<T>::deserialize(deserializer)? {
        OptionalStringOrNumber::String(value) => {
            value.parse().map(Some).map_err(serde::de::Error::custom)
        }
        OptionalStringOrNumber::Number(value) => Ok(Some(value)),
        OptionalStringOrNumber::Null => Ok(None),
    }
}

pub(super) fn serialize_bool_as_string<S: Serializer>(
    value: &bool,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(if *value { "true" } else { "false" })
}

pub(super) fn deserialize_bool_from_string_or_bool<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrString {
        Bool(bool),
        String(String),
    }

    match BoolOrString::deserialize(deserializer)? {
        BoolOrString::Bool(value) => Ok(value),
        BoolOrString::String(value) => match value.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(serde::de::Error::custom(format!(
                "invalid aria2 boolean '{}'; expected true or false",
                value
            ))),
        },
    }
}
