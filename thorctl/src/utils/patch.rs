//! Edits applied to a resource view held as a JSON value
//!
//! Non-interactive edits change a view through RFC 7386 merge patches and through dotted
//! `--set PATH=VALUE` and `--unset PATH` operations before the view is validated, so every
//! edit source shares one set of rules.

use serde_json::Value;
use thorium::Error;

/// One step of a path into a JSON document
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg {
    /// A key of an object
    Key(String),
    /// An index into an array
    Index(usize),
}

/// A single edit to a view, applied in command-line order
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Set the value at a path, creating missing objects along the way
    Set {
        /// The path to set
        path: Vec<PathSeg>,
        /// The value to store at the path
        value: Value,
    },
    /// Remove the value at a path
    Unset {
        /// The path to remove
        path: Vec<PathSeg>,
    },
}

impl EditOp {
    /// Apply this edit to a view
    ///
    /// # Arguments
    ///
    /// * `target` - The view to edit in place
    pub fn apply(&self, target: &mut Value) -> Result<(), Error> {
        // dispatch on the kind of edit
        match self {
            EditOp::Set { path, value } => set_path(target, path, value.clone()),
            EditOp::Unset { path } => unset_path(target, path),
        }
    }

    /// The first step of the path this edit changes
    pub fn top_level(&self) -> Option<&PathSeg> {
        // both kinds of edit carry a path
        match self {
            EditOp::Set { path, .. } | EditOp::Unset { path } => path.first(),
        }
    }
}

/// Render path steps back into the dotted form users type
///
/// # Arguments
///
/// * `path` - The steps to render
fn render_path(path: &[PathSeg]) -> String {
    // join keys with dots and write indexes in brackets
    let mut out = String::new();
    for seg in path {
        match seg {
            PathSeg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            PathSeg::Index(index) => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
        }
    }
    out
}

/// Name the JSON type of a value the way an error message needs it
///
/// # Arguments
///
/// * `value` - The value to describe
fn describe(value: &Value) -> &'static str {
    // pick the noun with its article
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Build the error for a step that cannot be taken because of the node's type
///
/// # Arguments
///
/// * `verb` - What was being attempted (`set` or `unset`)
/// * `path` - The whole path being edited
/// * `depth` - The position of the failing step in the path
/// * `node` - The value the step was applied to
/// * `wanted` - The type of container the step needs
fn wrong_type(verb: &str, path: &[PathSeg], depth: usize, node: &Value, wanted: &str) -> Error {
    // name the node the step was applied to
    let parent = if depth == 0 {
        "the document".to_string()
    } else {
        format!("'{}'", render_path(&path[..depth]))
    };
    Error::new(format!(
        "cannot {verb} '{}': {parent} is {}, not {wanted}",
        render_path(path),
        describe(node)
    ))
}

/// Parse a dotted path with `[N]` indexes into its steps
///
/// A backslash keeps the next character (a dot, bracket or backslash) inside a key.
///
/// # Arguments
///
/// * `path` - The raw path (`config.tools[0].name`)
pub fn parse_path(path: &str) -> Result<Vec<PathSeg>, Error> {
    // refuse a path with nothing in it
    if path.is_empty() {
        return Err(Error::new("the path is empty"));
    }
    let mut segs = Vec::new();
    let mut key = String::new();
    // whether the current key has any characters yet, escaped or not
    let mut in_key = false;
    // whether the last step was an index, which only a dot or bracket may follow
    let mut after_index = false;
    // whether a dot was just read and a key must follow
    let mut dot_pending = false;
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            // a backslash keeps the next character literally
            '\\' => {
                let Some(escaped) = chars.next() else {
                    return Err(Error::new(format!("path '{path}' ends with a backslash")));
                };
                if after_index {
                    return Err(Error::new(format!(
                        "path '{path}' has a key after ']' without a '.'"
                    )));
                }
                key.push(escaped);
                in_key = true;
                dot_pending = false;
            }
            // a dot ends the current key
            '.' => {
                if !in_key && !after_index {
                    return Err(Error::new(format!("path '{path}' has an empty key")));
                }
                if in_key {
                    segs.push(PathSeg::Key(std::mem::take(&mut key)));
                }
                in_key = false;
                after_index = false;
                dot_pending = true;
            }
            // a bracket opens an index after an optional key
            '[' => {
                if dot_pending && !in_key {
                    return Err(Error::new(format!("path '{path}' has an empty key")));
                }
                if in_key {
                    segs.push(PathSeg::Key(std::mem::take(&mut key)));
                }
                let mut digits = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == ']' {
                        closed = true;
                        break;
                    }
                    digits.push(next);
                }
                if !closed {
                    return Err(Error::new(format!("path '{path}' has an unclosed '['")));
                }
                let index = digits.parse::<usize>().map_err(|_| {
                    Error::new(format!(
                        "path '{path}' has the index '{digits}', which is not a number"
                    ))
                })?;
                segs.push(PathSeg::Index(index));
                in_key = false;
                after_index = true;
                dot_pending = false;
            }
            // any other character belongs to the current key
            other => {
                if after_index {
                    return Err(Error::new(format!(
                        "path '{path}' has '{other}' after ']' without a '.'"
                    )));
                }
                key.push(other);
                in_key = true;
                dot_pending = false;
            }
        }
    }
    // keep the final key, and refuse a trailing dot
    if in_key {
        segs.push(PathSeg::Key(key));
    } else if dot_pending {
        return Err(Error::new(format!("path '{path}' has an empty key")));
    }
    Ok(segs)
}

/// Apply an RFC 7386 merge patch to a value
///
/// # Arguments
///
/// * `target` - The value to patch in place
/// * `patch` - The merge patch to apply
pub fn merge_patch(target: &mut Value, patch: &Value) {
    // anything but an object replaces the target outright
    let Value::Object(changes) = patch else {
        *target = patch.clone();
        return;
    };
    // patching a non-object target starts from an empty object
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    let Value::Object(map) = target else {
        return;
    };
    // null removes a key and anything else merges recursively
    for (key, change) in changes {
        if change.is_null() {
            map.remove(key);
        } else {
            merge_patch(map.entry(key.clone()).or_insert(Value::Null), change);
        }
    }
}

/// Set the value below `node` at the remaining steps of a path
///
/// # Arguments
///
/// * `node` - The value reached so far
/// * `path` - The whole path being set
/// * `depth` - The position of the next step in the path
/// * `value` - The value to store
fn set_at(node: &mut Value, path: &[PathSeg], depth: usize, value: Value) -> Result<(), Error> {
    // the end of the path stores the value
    let Some(seg) = path.get(depth) else {
        *node = value;
        return Ok(());
    };
    // a null node becomes the container the step needs
    if node.is_null() {
        *node = match seg {
            PathSeg::Key(_) => Value::Object(serde_json::Map::new()),
            PathSeg::Index(_) => Value::Array(Vec::new()),
        };
    }
    match (seg, node) {
        // a key steps into an object, building a missing child before inserting it
        (PathSeg::Key(key), Value::Object(map)) => match map.get_mut(key) {
            Some(child) => set_at(child, path, depth + 1, value),
            None => {
                let mut child = Value::Null;
                set_at(&mut child, path, depth + 1, value)?;
                map.insert(key.clone(), child);
                Ok(())
            }
        },
        // an index steps into an array, replacing in range and appending at the end
        (PathSeg::Index(index), Value::Array(items)) => {
            if let Some(child) = items.get_mut(*index) {
                set_at(child, path, depth + 1, value)
            } else if *index == items.len() {
                let mut child = Value::Null;
                set_at(&mut child, path, depth + 1, value)?;
                items.push(child);
                Ok(())
            } else {
                Err(Error::new(format!(
                    "cannot set '{}': index {index} is out of range (the array has {} elements)",
                    render_path(path),
                    items.len()
                )))
            }
        }
        // a key into anything but an object cannot be followed
        (PathSeg::Key(_), node) => Err(wrong_type("set", path, depth, node, "an object")),
        // an index into anything but an array cannot be followed
        (PathSeg::Index(_), node) => Err(wrong_type("set", path, depth, node, "an array")),
    }
}

/// Set the value at a path, creating missing objects and appending at an array's end
///
/// # Arguments
///
/// * `target` - The value to edit in place
/// * `path` - The path to set
/// * `value` - The value to store
pub fn set_path(target: &mut Value, path: &[PathSeg], value: Value) -> Result<(), Error> {
    set_at(target, path, 0, value)
}

/// Remove the value below `node` at the remaining steps of a path
///
/// # Arguments
///
/// * `node` - The value reached so far
/// * `path` - The whole path being removed
/// * `depth` - The position of the next step in the path
fn unset_at(node: &mut Value, path: &[PathSeg], depth: usize) -> Result<(), Error> {
    let last = depth + 1 >= path.len();
    match (&path[depth], node) {
        // nothing exists below a null, so there is nothing to remove
        (_, Value::Null) => Ok(()),
        // a key removes from or steps into an object
        (PathSeg::Key(key), Value::Object(map)) => {
            if last {
                map.remove(key);
                Ok(())
            } else if let Some(child) = map.get_mut(key) {
                unset_at(child, path, depth + 1)
            } else {
                Ok(())
            }
        }
        // an index removes from or steps into an array
        (PathSeg::Index(index), Value::Array(items)) => {
            if *index >= items.len() {
                Ok(())
            } else if last {
                items.remove(*index);
                Ok(())
            } else {
                unset_at(&mut items[*index], path, depth + 1)
            }
        }
        // a key into anything but an object cannot be followed
        (PathSeg::Key(_), node) => Err(wrong_type("unset", path, depth, node, "an object")),
        // an index into anything but an array cannot be followed
        (PathSeg::Index(_), node) => Err(wrong_type("unset", path, depth, node, "an array")),
    }
}

/// Remove the value at a path; a path that does not exist is left alone
///
/// # Arguments
///
/// * `target` - The value to edit in place
/// * `path` - The path to remove
pub fn unset_path(target: &mut Value, path: &[PathSeg]) -> Result<(), Error> {
    // removing the whole document is not an edit of a view
    if path.is_empty() {
        return Err(Error::new("cannot unset the whole document"));
    }
    unset_at(target, path, 0)
}

/// Parse the value of a `--set` argument as a YAML scalar or flow value
///
/// Only `true`, `false`, `null`, `~` and JSON numbers are read as non-strings; quotes force a
/// string, and `[...]` and `{...}` are read as YAML flow collections.
///
/// # Arguments
///
/// * `raw` - The raw text after the `=`
pub fn parse_value(raw: &str) -> Result<Value, Error> {
    let text = raw.trim();
    match text.chars().next() {
        // an empty value is an empty string
        None => Ok(Value::String(String::new())),
        // flow collections parse as YAML
        Some('[' | '{') => serde_norway::from_str::<Value>(text)
            .map_err(|err| Error::new(format!("invalid flow value '{raw}': {err}"))),
        // quotes force a string, with JSON escapes for double quotes
        Some(quote @ ('"' | '\'')) => {
            let parsed = if quote == '"' {
                serde_json::from_str::<String>(text)
                    .ok()
                    .or_else(|| serde_norway::from_str::<String>(text).ok())
            } else {
                serde_norway::from_str::<String>(text).ok()
            };
            parsed
                .map(Value::String)
                .ok_or_else(|| Error::new(format!("invalid quoted value '{raw}'")))
        }
        // plain text is a keyword, a JSON number, or a string
        Some(_) => Ok(match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" | "~" => Value::Null,
            _ => match serde_json::from_str::<Value>(text) {
                Ok(number @ Value::Number(_)) => number,
                _ => Value::String(text.to_string()),
            },
        }),
    }
}

/// Turn a path error into the text clap shows for a bad argument
///
/// # Arguments
///
/// * `err` - The error to render
fn usage_message(err: Error) -> String {
    // use the bare message so clap does not repeat the error prefix
    match err {
        Error::Generic(msg) => msg,
        other => other.to_string(),
    }
}

/// Parse a `--set PATH=VALUE` argument
///
/// # Arguments
///
/// * `raw` - The raw argument
pub fn parse_set(raw: &str) -> Result<EditOp, String> {
    // split at the first equals sign so values may hold more
    let Some((path, value)) = raw.split_once('=') else {
        return Err(format!("'{raw}' must be PATH=VALUE"));
    };
    // parse both halves
    let path = parse_path(path).map_err(usage_message)?;
    let value = parse_value(value).map_err(usage_message)?;
    Ok(EditOp::Set { path, value })
}

/// Parse a `--unset PATH` argument
///
/// # Arguments
///
/// * `raw` - The raw argument
pub fn parse_unset(raw: &str) -> Result<EditOp, String> {
    // parse the path to remove
    let path = parse_path(raw).map_err(usage_message)?;
    Ok(EditOp::Unset { path })
}

/// Unit tests for merge patches and path edits
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Apply a merge patch to a copy of `target` and return the result
    ///
    /// # Arguments
    ///
    /// * `target` - The document to patch
    /// * `patch` - The merge patch
    fn patched(target: Value, patch: &Value) -> Value {
        // apply the patch to our copy
        let mut target = target;
        merge_patch(&mut target, patch);
        target
    }

    /// Set a path given as text and return the edited document
    ///
    /// # Arguments
    ///
    /// * `target` - The document to edit
    /// * `path` - The raw path
    /// * `value` - The value to store
    fn set(target: Value, path: &str, value: Value) -> Result<Value, Error> {
        // parse the path and edit our copy
        let mut target = target;
        set_path(&mut target, &parse_path(path)?, value)?;
        Ok(target)
    }

    /// Unset a path given as text and return the edited document
    ///
    /// # Arguments
    ///
    /// * `target` - The document to edit
    /// * `path` - The raw path
    fn unset(target: Value, path: &str) -> Result<Value, Error> {
        // parse the path and edit our copy
        let mut target = target;
        unset_path(&mut target, &parse_path(path)?)?;
        Ok(target)
    }

    /// The error text of a failed result
    ///
    /// # Arguments
    ///
    /// * `result` - The result expected to be an error
    fn message<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
        // render the error the way the command line prints it
        result.expect_err("expected an error").to_string()
    }

    /// The examples from the merge patch RFC (appendix A) produce their documented results
    #[test]
    fn merge_patch_rfc_vectors() {
        let cases = [
            (json!({"a":"b"}), json!({"a":"c"}), json!({"a":"c"})),
            (json!({"a":"b"}), json!({"b":"c"}), json!({"a":"b","b":"c"})),
            (json!({"a":"b"}), json!({"a":null}), json!({})),
            (
                json!({"a":"b","b":"c"}),
                json!({"a":null}),
                json!({"b":"c"}),
            ),
            (json!({"a":["b"]}), json!({"a":"c"}), json!({"a":"c"})),
            (json!({"a":"c"}), json!({"a":["b"]}), json!({"a":["b"]})),
            (
                json!({"a":{"b":"c"}}),
                json!({"a":{"b":"d","c":null}}),
                json!({"a":{"b":"d"}}),
            ),
            (json!({"a":[{"b":"c"}]}), json!({"a":[1]}), json!({"a":[1]})),
            (json!(["a", "b"]), json!(["c", "d"]), json!(["c", "d"])),
            (json!({"a":"b"}), json!(["c"]), json!(["c"])),
            (json!({"a":"foo"}), json!(null), json!(null)),
            (json!({"a":"foo"}), json!("bar"), json!("bar")),
            (json!({"e":null}), json!({"a":1}), json!({"e":null,"a":1})),
            (json!([1, 2]), json!({"a":"b","c":null}), json!({"a":"b"})),
            (
                json!({}),
                json!({"a":{"bb":{"ccc":null}}}),
                json!({"a":{"bb":{}}}),
            ),
        ];
        // every case must match the RFC's documented result
        for (target, patch, expected) in cases {
            assert_eq!(
                patched(target.clone(), &patch),
                expected,
                "{target} + {patch}"
            );
        }
    }

    /// A merge patch cannot set a field to null, which removes it instead
    #[test]
    fn merge_patch_null_removes_nested_keys() {
        let result = patched(json!({"a":{"b":1,"c":2}}), &json!({"a":{"b":null}}));
        assert_eq!(result, json!({"a":{"c":2}}));
    }

    /// Dotted paths split into keys and indexes
    #[test]
    fn path_parses_keys_and_indexes() {
        assert_eq!(
            parse_path("config.tools[0].name").unwrap(),
            vec![
                PathSeg::Key("config".into()),
                PathSeg::Key("tools".into()),
                PathSeg::Index(0),
                PathSeg::Key("name".into()),
            ]
        );
        assert_eq!(
            parse_path("[2][10]").unwrap(),
            vec![PathSeg::Index(2), PathSeg::Index(10)]
        );
        assert_eq!(
            parse_path("*id*").unwrap(),
            vec![PathSeg::Key("*id*".into())]
        );
    }

    /// A backslash keeps a dot, bracket or backslash inside a key
    #[test]
    fn path_escapes_special_characters() {
        assert_eq!(
            parse_path(r"labels.app\.io\/name").unwrap(),
            vec![
                PathSeg::Key("labels".into()),
                PathSeg::Key("app.io/name".into())
            ]
        );
        assert_eq!(
            parse_path(r"a\[0\]").unwrap(),
            vec![PathSeg::Key("a[0]".into())]
        );
    }

    /// Malformed paths are refused with a message naming the problem
    #[test]
    fn path_errors_name_the_problem() {
        assert!(message(parse_path("")).contains("empty"));
        assert!(message(parse_path("a..b")).contains("empty"));
        assert!(message(parse_path("a.")).contains("empty"));
        assert!(message(parse_path("a[1")).contains("unclosed"));
        assert!(message(parse_path("a[x]")).contains("index"));
        assert!(message(parse_path("a[1]b")).contains("after"));
    }

    /// Setting walks nested objects and creates the ones that are missing
    #[test]
    fn set_creates_missing_objects() {
        let result = set(json!({"a":1}), "config.limits.cpu", json!(4)).unwrap();
        assert_eq!(result, json!({"a":1,"config":{"limits":{"cpu":4}}}));
    }

    /// Setting replaces an existing value of any type
    #[test]
    fn set_replaces_values() {
        let result = set(json!({"a":{"b":[1,2]}}), "a.b", json!("x")).unwrap();
        assert_eq!(result, json!({"a":{"b":"x"}}));
    }

    /// Indexes replace in range and append one past the end
    #[test]
    fn set_indexes_replace_and_append() {
        let doc = json!({"tools":["a","b"]});
        assert_eq!(
            set(doc.clone(), "tools[1]", json!("z")).unwrap(),
            json!({"tools":["a","z"]})
        );
        assert_eq!(
            set(doc.clone(), "tools[2]", json!("c")).unwrap(),
            json!({"tools":["a","b","c"]})
        );
        assert_eq!(
            set(json!({}), "tools[0]", json!("c")).unwrap(),
            json!({"tools":["c"]})
        );
    }

    /// Indexes reach objects inside arrays
    #[test]
    fn set_reaches_into_arrays_of_objects() {
        let doc = json!({"agents":[{"name":"a"},{"name":"b"}]});
        let result = set(doc, "agents[1].name", json!("c")).unwrap();
        assert_eq!(result, json!({"agents":[{"name":"a"},{"name":"c"}]}));
    }

    /// An explicit null along the path is replaced by the container the path needs
    #[test]
    fn set_replaces_null_containers() {
        let result = set(json!({"selector":null}), "selector.key", json!("v")).unwrap();
        assert_eq!(result, json!({"selector":{"key":"v"}}));
    }

    /// Paths that cannot be followed explain why
    #[test]
    fn set_errors_are_specific() {
        let scalar = message(set(json!({"a":1}), "a.b", json!(2)));
        assert!(
            scalar.contains("a.b") && scalar.contains("number"),
            "{scalar}"
        );
        let range = message(set(json!({"a":[1]}), "a[5]", json!(2)));
        assert!(
            range.contains("out of range") && range.contains('5'),
            "{range}"
        );
        let kind = message(set(json!({"a":[1]}), "a.b", json!(2)));
        assert!(kind.contains("array"), "{kind}");
        let index = message(set(json!({"a":{"b":1}}), "a[0]", json!(2)));
        assert!(index.contains("object"), "{index}");
    }

    /// Unsetting removes a key, and array elements shift down
    #[test]
    fn unset_removes_keys_and_elements() {
        assert_eq!(unset(json!({"a":1,"b":2}), "a").unwrap(), json!({"b":2}));
        assert_eq!(
            unset(json!({"t":["x","y","z"]}), "t[1]").unwrap(),
            json!({"t":["x","z"]})
        );
        assert_eq!(
            unset(json!({"a":{"b":1,"c":2}}), "a.b").unwrap(),
            json!({"a":{"c":2}})
        );
    }

    /// Unsetting something that is not there changes nothing
    #[test]
    fn unset_missing_paths_is_a_no_op() {
        let doc = json!({"a":{"b":1},"t":[1]});
        assert_eq!(unset(doc.clone(), "x").unwrap(), doc);
        assert_eq!(unset(doc.clone(), "a.zz.y").unwrap(), doc);
        assert_eq!(unset(doc.clone(), "t[9]").unwrap(), doc);
    }

    /// Unsetting through a value of the wrong type is an error
    #[test]
    fn unset_through_scalars_fails() {
        assert!(message(unset(json!({"a":1}), "a.b")).contains("number"));
    }

    /// Plain scalars take the type a YAML reader would give them
    #[test]
    fn value_scalars() {
        assert_eq!(parse_value("5").unwrap(), json!(5));
        assert_eq!(parse_value("-3").unwrap(), json!(-3));
        assert_eq!(parse_value("1.5").unwrap(), json!(1.5));
        assert_eq!(parse_value("true").unwrap(), json!(true));
        assert_eq!(parse_value("false").unwrap(), json!(false));
        assert_eq!(parse_value("null").unwrap(), json!(null));
        assert_eq!(parse_value("~").unwrap(), json!(null));
        assert_eq!(parse_value("hello").unwrap(), json!("hello"));
        assert_eq!(
            parse_value("ghcr.io/x/y:2").unwrap(),
            json!("ghcr.io/x/y:2")
        );
    }

    /// Text that only looks numeric to YAML stays a string
    #[test]
    fn value_lookalikes_stay_strings() {
        assert_eq!(parse_value("0x10").unwrap(), json!("0x10"));
        assert_eq!(parse_value("+5").unwrap(), json!("+5"));
        assert_eq!(parse_value(".inf").unwrap(), json!(".inf"));
        assert_eq!(parse_value("a: b").unwrap(), json!("a: b"));
        assert_eq!(parse_value("- x").unwrap(), json!("- x"));
        assert_eq!(parse_value("").unwrap(), json!(""));
    }

    /// Quotes force a string, and flow collections parse as collections
    #[test]
    fn value_quotes_and_flow() {
        assert_eq!(parse_value("'5'").unwrap(), json!("5"));
        assert_eq!(parse_value("\"a b\"").unwrap(), json!("a b"));
        assert_eq!(parse_value("[a, b]").unwrap(), json!(["a", "b"]));
        assert_eq!(parse_value("[]").unwrap(), json!([]));
        assert_eq!(
            parse_value("{a: 1, b: [x]}").unwrap(),
            json!({"a":1,"b":["x"]})
        );
        assert!(message(parse_value("[a, b")).contains("flow"));
    }

    /// `--set` splits at the first equals sign so values may contain more
    #[test]
    fn set_argument_splits_at_first_equals() {
        let op = parse_set("config.args=--a=b").unwrap();
        assert_eq!(
            op,
            EditOp::Set {
                path: parse_path("config.args").unwrap(),
                value: json!("--a=b")
            }
        );
        let typed = parse_set("config.max_turns=5").unwrap();
        assert_eq!(
            typed,
            EditOp::Set {
                path: parse_path("config.max_turns").unwrap(),
                value: json!(5)
            }
        );
    }

    /// A `--set` without an equals sign or with a bad path is a usage error
    #[test]
    fn set_argument_errors() {
        assert!(parse_set("config").unwrap_err().contains("PATH=VALUE"));
        assert!(parse_set("=5").unwrap_err().contains("empty"));
        assert!(parse_set("a[x]=5").unwrap_err().contains("index"));
    }

    /// `--unset` takes a bare path
    #[test]
    fn unset_argument() {
        assert_eq!(
            parse_unset("a.b[1]").unwrap(),
            EditOp::Unset {
                path: parse_path("a.b[1]").unwrap()
            }
        );
        assert!(parse_unset("a[").is_err());
    }

    /// Operations apply in order, so a later one sees an earlier one's result
    #[test]
    fn operations_apply_in_order() {
        let mut doc = json!({"a":1});
        for op in [
            parse_set("b=2").unwrap(),
            parse_unset("a").unwrap(),
            parse_set("a=3").unwrap(),
        ] {
            op.apply(&mut doc).unwrap();
        }
        assert_eq!(doc, json!({"a":3,"b":2}));
    }

    /// An operation reports the first step of its path for the read-only check
    #[test]
    fn operations_report_their_top_level_key() {
        assert_eq!(
            parse_set("a.b=1").unwrap().top_level(),
            Some(&PathSeg::Key("a".into()))
        );
        assert_eq!(
            parse_unset("[0]").unwrap().top_level(),
            Some(&PathSeg::Index(0))
        );
    }
}
