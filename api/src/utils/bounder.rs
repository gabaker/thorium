//! Bounds checking utilities for user input to Thorium

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;

use axum::extract::multipart::Field;
use regex::Regex;
use serde_json::Value;
use tracing::instrument;
use uuid::Uuid;

use super::ApiError;
use crate::bad;
use crate::models::EventTrigger;
use crate::utils::protected_names;

/// Bounds check a string
///
/// This enforces a minimum and maximum size for a string.
///
/// # Arguments
///
/// * `input` - The string to bounds check
/// * `name` - The variable name to be bounds checked (for logging/errors)
/// * `min` - The minimum length of this string
/// * `max` - The maximum length of this string
pub fn string(input: &str, name: &'static str, min: usize, max: usize) -> Result<(), ApiError> {
    // bounds check length
    let input_len = input.len();
    if input_len < min || input_len > max {
        return bad!(format!("{name} must be between {min} and  {max} chars",));
    }

    // ensure this string is alpha numeric
    if !input.chars().all(|chr| chr.is_alphanumeric() || chr == '-') {
        return bad!(format!("{name} must be only alphanumeric or '-' {input}",));
    }
    Ok(())
}

/// Bounds check a lowercase string
///
/// This enforces a minimum and maximum size for a string and ensures it is all lowercase.
///
/// # Arguments
///
/// * `input` - The string to bounds check
/// * `name` - The variable name to be bounds checked (for logging/errors)
/// * `min` - The minimum length of this string
/// * `max` - The maximum length of this string
pub fn string_lower(
    input: &str,
    name: &'static str,
    min: usize,
    max: usize,
) -> Result<(), ApiError> {
    // bounds check length
    let input_len = input.len();
    if input_len < min || input_len > max {
        return bad!(format!("{name} must be between {min} and  {max} chars",));
    }

    // ensure this string is alpha numeric and lowercase or a -
    if !input
        .chars()
        .all(|chr| chr.is_lowercase() || chr.is_numeric() || chr == '-')
    {
        return bad!(format!(
            "{name} must be only lowercase alphanumeric or '-' {input}",
        ));
    }
    Ok(())
}

/// Refuse the name of a new tool, toolset, workflow, or pipeline when it is a protected name
///
/// Only the whole name is compared, without regard to case. This runs after the length and
/// character checks so those keep their own errors.
///
/// # Arguments
///
/// * `input` - The new name to check
pub fn entity_name(input: &str) -> Result<(), ApiError> {
    // refuse the name with the one line that names it and the protected list
    match protected_names::check(input) {
        Ok(()) => Ok(()),
        Err(refusal) => bad!(refusal),
    }
}

/// Bounds check a metagroup to make sure its valid for ldap
///
/// # Arguments
///
/// * `metagroup` - The metagroup name to check
pub fn ldap_metagroup(metagroup: &str) -> Result<(), ApiError> {
    // make sure this string is not empty
    if metagroup.is_empty() {
        return bad!("Ldap metagroups cannot be empty strings".to_owned());
    }
    // make sure this strings characters are alphanumeric or '-' or '_'
    if !metagroup
        .chars()
        .all(|chr| chr.is_alphanumeric() || chr == '-' || chr == '_')
    {
        return bad!(format!(
            "Ldap metagroups must be only alphanumeric or '-' or '_' but is {metagroup}",
        ));
    }
    Ok(())
}

/// Bounds check a set of metagroups to make sure its valid for ldap
///
/// # Arguments
///
/// * `metagroups` - The metagroups name to check
pub fn ldap_metagroups(metagroups: &HashSet<String>) -> Result<(), ApiError> {
    // check all metagroup names in this list
    for metagroup in metagroups {
        // check this metagroup name
        ldap_metagroup(metagroup)?
    }
    Ok(())
}

/// Bounds check a file name
///
/// This enforces a minimum and maximum size for a string.
///
/// # Arguments
///
/// * `input` - The string to bounds check
/// * `name` - The variable name to be bounds checked (for logging/errors)
/// * `min` - The minimum length of this string
/// * `max` - The maximum length of this string
pub fn file_name(input: &str, name: &'static str, min: usize, max: usize) -> Result<(), ApiError> {
    // bounds check length
    let input_len = input.len();
    if input_len < min || input_len > max {
        return bad!(format!("{name} must be between {min} and  {max} chars",));
    }
    // make sure this filename is not just '.'
    if input.chars().all(|chr| chr == '.') {
        return bad!(format!("{name} cannot just be '.'s {input}"));
    }
    // ensure this string is alphanumeric
    if !input
        .chars()
        .all(|chr| chr.is_alphanumeric() || chr == '-' || chr == '.')
    {
        return bad!(format!(
            "{name} must be only alphanumeric or '-'/'.' {input}",
        ));
    }
    Ok(())
}

/// Validate a path in a multipart form data field that becomes an S3 key, such as a result
/// file's name, segment by segment (see [`file_path`])
///
/// # Arguments
///
/// * `field` - The field to get the file name from
/// * `name` - The name fo the field to use in error messages
/// * `allow_absolute` - Whether to allow absolute paths or not
pub fn multipart_path(
    field: &Field<'_>,
    name: &str,
    allow_absolute: bool,
) -> Result<Option<String>, ApiError> {
    // try to get the name for this file, validating it when there is one
    field
        .file_name()
        .map(|file_name| file_path(file_name, name, allow_absolute))
        .transpose()
}

/// Validate a file name in a multipart form data field that is stored only as metadata, such as a
/// sample's name: it may not be absolute (unless allowed), hold a component of only dots, or be
/// invalid UTF-8, while segments a path's parsing normalizes (an empty name, a doubled or trailing
/// `/`) pass, since the name never becomes a key
///
/// # Arguments
///
/// * `field` - The field to get the file name from
/// * `name` - The name of the field to use in error messages
/// * `allow_absolute` - Whether to allow absolute paths or not
pub fn multipart_name(
    field: &Field<'_>,
    name: &str,
    allow_absolute: bool,
) -> Result<Option<String>, ApiError> {
    // try to get the name for this file, validating it when there is one
    field
        .file_name()
        .map(|file_name| file_name_path(file_name, name, allow_absolute))
        .transpose()
}

/// Validate a file name stored only as metadata, by the components a path parses it into
///
/// # Arguments
///
/// * `file_name` - The name to validate
/// * `name` - The name of the field to use in error messages
/// * `allow_absolute` - Whether to allow absolute paths or not
pub fn file_name_path(
    file_name: &str,
    name: &str,
    allow_absolute: bool,
) -> Result<String, ApiError> {
    // convert our file name to a path to validate it
    let path = PathBuf::from_str(file_name)?;
    // validate this file name is not an absolute path
    if !allow_absolute && path.is_absolute() {
        return bad!(format!("{name} paths cannot be an absolute: {path:?}"));
    }
    // make sure this path not contain a component with just '.'s
    for component in path.components() {
        // convert this component to an os str
        match component.as_os_str().to_str() {
            Some(comp_str) => {
                if comp_str.chars().all(|chr| chr == '.') {
                    return bad!(format!(
                        "{name} paths cannot have components that have .. in them: {path:?}"
                    ));
                }
            }
            None => {
                return bad!(format!("{name} paths must be valid utf-8"));
            }
        }
    }
    // return our validated file name
    Ok(file_name.to_owned())
}

/// Validate a path a client named that becomes an S3 key, such as a result file's name
///
/// The name is checked as given, segment by segment, since it is stored and later used as an S3
/// key unchanged: an empty segment, a segment of only dots, or a trailing `/` would be dropped by
/// a path's own parsing but stay in the key.
///
/// # Arguments
///
/// * `file_name` - The path to validate
/// * `name` - The name of the field to use in error messages
/// * `allow_absolute` - Whether to allow absolute paths or not
pub fn file_path(file_name: &str, name: &str, allow_absolute: bool) -> Result<String, ApiError> {
    // convert our file name to a path to validate it
    let path = PathBuf::from_str(file_name)?;
    // validate this file name is not an absolute path
    if !allow_absolute && path.is_absolute() {
        return bad!(format!("{name} paths cannot be an absolute: {path:?}"));
    }
    // an absolute path starts with one separator; every segment after it must hold a name
    let relative = file_name.strip_prefix('/').unwrap_or(file_name);
    for segment in relative.split('/') {
        if segment.is_empty() {
            return bad!(format!(
                "{name} paths cannot have empty segments or end with '/': {path:?}"
            ));
        }
        if segment.chars().all(|chr| chr == '.') {
            return bad!(format!(
                "{name} paths cannot have components that have .. in them: {path:?}"
            ));
        }
    }
    // return our validated file name
    Ok(file_name.to_owned())
}

/// Bounds check a JsonValue that should be cast as a string
///
/// This enforces a minimum and maximum size for a string
///
/// # Arguments
///
/// * `input` - The json value to cast as a string and bounds check
/// * `name` - The variable name to be bounds checked (for logging/errors)
/// * `min` - The minimum length of this string
/// * `max` - The maximum length of this string
pub fn string_json_value(
    input: &serde_json::Value,
    name: &'static str,
    min: usize,
    max: usize,
) -> Result<String, ApiError> {
    // cast to string string
    if !input.is_string() {
        return bad!(format!("{name} must be a string - {input:#?}"));
    }
    let input_str = input.as_str().unwrap_or("");

    // bounds check length
    let input_len = input_str.len();
    if input_len < min || input_len > max {
        return bad!(format!(
            "{name} must be between {min} and  {max} chars - {input}",
        ));
    }
    Ok(input_str.to_string())
}
/// Bounds check a number
///
/// This enforces a minimum and maximum value for a signed int.
///
/// # Arguments
///
/// * `input` - The signed into to bounds check
/// * `name` - The variable name to be bounds checked (for logging/errors)
/// * `min` - The minimum value for this int
/// * `max` - The maximum value for this int
pub fn number(input: i64, name: &'static str, min: i64, max: i64) -> Result<i64, ApiError> {
    // bounds check size
    if input < min || input > max {
        return bad!(format!(
            "{name} must be between {min} and  {max} but is {input}",
        ));
    }
    Ok(input)
}

/// Bounds check a unsigned number
///
/// This enforces a minimum and maximum value for a unsigned int.
///
/// # Arguments
///
/// * `input` - The unsigned int to bounds check
/// * `name` - The variable name to be bounds checked (for logging/errors)
/// * `min` - The minimum value for this int
/// * `max` - The maximum value for this int
pub fn unsigned(input: u64, name: &'static str, min: u64, max: u64) -> Result<u64, ApiError> {
    // bounds check size
    if input < min || input > max {
        return bad!(format!("{name} must be between {min} and {max} - {input}",));
    }
    Ok(input)
}

/// Bounds check a pipeline order
///
/// This enforces that a pipeline order is a non empty list of stages with valid names, where a
/// stage is either one name or a non empty list of names that run in parallel. Each stage is
/// backed by a tool, so stage names don't need to name images.
///
/// # Arguments
///
/// * `raw` - The raw pipeline order to bounds check
#[instrument(name = "utils::bounder::pipeline_order", skip_all, err(Debug))]
pub fn pipeline_order(raw: &Value) -> Result<Vec<Vec<String>>, ApiError> {
    // make sure order is an array
    let Some(stages) = raw.as_array() else {
        return bad!("order must be an array".to_string());
    };
    // cast raw order to Vec<Vec<String>>
    let mut cast = Vec::with_capacity(stages.len());
    // iterate over stages in order
    for stage in stages {
        // handle stages with sub stages
        if let Some(sub_stages) = stage.as_array() {
            // make sure that we have some sub stages defined
            if sub_stages.is_empty() {
                return bad!("order cannot have an empty stage".to_owned());
            }
            // bounds check each sub stage's name
            let inner_cast = sub_stages
                .iter()
                .map(|item| string_json_value(item, "stage", 1, 255))
                .collect::<Result<Vec<String>, ApiError>>()?;
            cast.push(inner_cast);
        } else {
            // a single stage runs alone in its step
            cast.push(vec![string_json_value(stage, "stage", 1, 255)?]);
        }
    }
    // make sure order is not empty
    if cast.is_empty() {
        return bad!("order must not be empty".to_string());
    }
    Ok(cast)
}

/// Convert a string to a uuid
///
/// This will error on invalid uuidv4 inputs.
///
/// # Arguments
///
/// * `uuid` - A uuidv4 as a string
/// * `name` - The variable name to be bounds checked (for logging/errors)
pub fn uuid<'a>(uuid: &'a str, name: &'a str) -> Result<Uuid, ApiError> {
    // throw an error if an invalid uuid was passed
    match Uuid::parse_str(uuid) {
        Ok(valid) => Ok(valid),
        Err(_) => bad!(format!("{name} must be a valid uuidv4")),
    }
}

/// Validate triggers
///
/// # Arguments
///
/// * `triggers` - The triggers to validate
pub fn triggers(triggers: &HashMap<String, EventTrigger>) -> Result<(), ApiError> {
    // make sure all new tag type event triggers have types
    for (name, trigger) in triggers.iter() {
        // make sure new tag triggers have tag types set
        match trigger {
            EventTrigger::NewSample => continue,
            EventTrigger::Tag { tag_types, .. } => {
                // make sure we have some tag type set
                if tag_types.is_empty() {
                    return bad!(format!("tag triggers must have tag types set: {name}"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{file_name_path, file_path};

    /// A sample's name is only metadata, so names a path normalizes pass while `..` and absolute
    /// names (unless allowed) are still refused
    #[test]
    fn metadata_file_names_keep_the_component_check() {
        // names whose segments a path drops pass unchanged
        for name in ["", "a//b", "dir/", "/", "a/./b"] {
            assert_eq!(file_name_path(name, "File name", true).unwrap(), name);
        }
        // a component of only dots, and an absolute name unless allowed, are refused
        assert!(file_name_path("a/../b", "File name", true).is_err());
        assert!(file_name_path("/a/b", "File name", false).is_err());
    }

    /// A stored file name becomes its object's key unchanged, so names whose segments a path
    /// would normalize away are refused, and ordinary names pass unchanged
    #[test]
    fn file_paths_with_segments_a_path_drops_are_refused() {
        // empty segments, `.` segments, and a trailing slash are refused, relative or absolute
        for name in [
            "a//b", "a/./b", "a/", "./a", "a/..", "/a//b", "/a/./b", "/a/",
        ] {
            assert!(file_path(name, "Result", true).is_err(), "{name} passed");
        }
        // an absolute name is refused unless allowed
        assert!(file_path("/a/b", "Result", false).is_err());
        assert_eq!(file_path("/a/b", "Result", true).unwrap(), "/a/b");
        // ordinary names pass unchanged, including dots inside a segment
        for name in ["out.txt", "nested/out.txt", "a/b.c/d..e", ".hidden/file"] {
            assert_eq!(file_path(name, "Result", false).unwrap(), name);
        }
    }
}
