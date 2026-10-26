//! Reading and writing the YAML or JSON documents that edit commands take and print
//!
//! `--file`, `--patch` and `--spec` arguments name a document inline, as a path, or as `-` for
//! stdin. YAML is a superset of JSON, so one reader accepts both and reports line and column.

use serde_json::Value;
use std::io::Read;
use thorium::Error;

/// The name that stands for stdin in a document argument
pub const STDIN: &str = "-";

/// Read the text of a document from a file or from stdin
///
/// # Arguments
///
/// * `source` - A path, or `-` to read stdin to its end
pub fn read_text(source: &str) -> Result<String, Error> {
    // read stdin when asked to
    if source == STDIN {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|err| Error::new(format!("Failed to read stdin: {err}")))?;
        return Ok(text);
    }
    // otherwise read the file
    std::fs::read_to_string(source)
        .map_err(|err| Error::new(format!("Failed to read '{source}': {err}")))
}

/// Parse a YAML or JSON document into a JSON value
///
/// Text that looks like JSON is read as JSON so its errors point into the JSON; anything else is
/// read as YAML, whose errors carry a line and column.
///
/// # Arguments
///
/// * `text` - The document text
/// * `what` - What the document is, for error messages (`--file`, `--patch`)
pub fn parse_document(text: &str, what: &str) -> Result<Value, Error> {
    // an empty document has nothing to apply
    if text.trim().is_empty() {
        return Err(Error::new(format!("The {what} document is empty")));
    }
    // read JSON-looking text as JSON first
    let trimmed = text.trim_start();
    let json_like = trimmed.starts_with(['{', '[']) && trimmed.contains('"');
    if json_like {
        match serde_json::from_str::<Value>(text) {
            Ok(value) => return Ok(value),
            Err(json_err) => {
                // YAML flow syntax is not JSON, so fall back before reporting the JSON error
                if let Ok(value) = serde_norway::from_str::<Value>(text) {
                    return Ok(value);
                }
                return Err(Error::new(format!(
                    "The {what} document is not valid JSON: {json_err}"
                )));
            }
        }
    }
    // everything else is YAML
    serde_norway::from_str::<Value>(text)
        .map_err(|err| Error::new(format!("The {what} document is not valid YAML: {err}")))
}

/// Read and parse a document from a file or stdin
///
/// # Arguments
///
/// * `source` - A path, or `-` for stdin
/// * `what` - What the document is, for error messages
pub fn read_document(source: &str, what: &str) -> Result<Value, Error> {
    // read the text and parse it
    let text = read_text(source)?;
    parse_document(&text, what)
}

/// Read a `--patch` argument, which is inline JSON or YAML, or `@FILE` to read a file
///
/// # Arguments
///
/// * `arg` - The raw argument
pub fn parse_patch(arg: &str) -> Result<Value, Error> {
    // an @ names a file (or - for stdin) to read the patch from
    match arg.strip_prefix('@') {
        Some(source) => read_document(source, "--patch"),
        None => parse_document(arg, "--patch"),
    }
}

/// Render a value as a YAML document
///
/// # Arguments
///
/// * `value` - The value to render
pub fn render_yaml(value: &Value) -> Result<String, Error> {
    serde_norway::to_string(value)
        .map_err(|err| Error::new(format!("Failed to render YAML: {err}")))
}

/// Render a value as a pretty JSON document ending in a newline
///
/// # Arguments
///
/// * `value` - The value to render
pub fn render_json(value: &Value) -> Result<String, Error> {
    // render pretty JSON and end the file with a newline
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|err| Error::new(format!("Failed to render JSON: {err}")))?;
    text.push('\n');
    Ok(text)
}

/// Unit tests for document reading
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// YAML and JSON documents parse to the same value
    #[test]
    fn parses_yaml_and_json() {
        let yaml = parse_document("a: 1\nb:\n  - x\n  - y\n", "--file").unwrap();
        let json = parse_document("{\"a\": 1, \"b\": [\"x\", \"y\"]}", "--file").unwrap();
        assert_eq!(yaml, json);
        assert_eq!(yaml, json!({"a":1,"b":["x","y"]}));
    }

    /// JSON indented with tabs is still read
    #[test]
    fn parses_tab_indented_json() {
        let value = parse_document("{\n\t\"a\": 1\n}", "--file").unwrap();
        assert_eq!(value, json!({"a":1}));
    }

    /// YAML flow syntax that is not JSON parses as YAML
    #[test]
    fn parses_yaml_flow_collections() {
        let value = parse_document("{a: \"x\", b: [1]}", "--patch").unwrap();
        assert_eq!(value, json!({"a":"x","b":[1]}));
    }

    /// A syntax error names the line and column of the problem
    #[test]
    fn yaml_errors_carry_a_location() {
        let err = parse_document("a: 1\nb: [1, 2\nc: 3\n", "--file")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--file") && err.contains("line 2"), "{err}");
    }

    /// Bad JSON reports the JSON parser's complaint
    #[test]
    fn json_errors_are_reported_as_json() {
        let err = parse_document("{\"a\": 1", "--patch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid JSON"), "{err}");
    }

    /// An empty document is refused rather than read as null
    #[test]
    fn empty_documents_are_refused() {
        let err = parse_document("  \n", "--file").unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
    }

    /// An inline patch is read directly and an `@` patch is read from a file
    #[test]
    fn patch_arguments() {
        assert_eq!(
            parse_patch("{\"description\": \"x\"}").unwrap(),
            json!({"description":"x"})
        );
        let dir = std::env::temp_dir().join(format!("thorctl-spec-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patch.yml");
        std::fs::write(&path, "description: from file\n").unwrap();
        let from_file = parse_patch(&format!("@{}", path.display())).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(from_file, json!({"description":"from file"}));
    }

    /// A missing file names the path in its error
    #[test]
    fn missing_files_name_the_path() {
        let err = read_document("/nonexistent/thorctl/doc.yml", "--file")
            .unwrap_err()
            .to_string();
        assert!(err.contains("/nonexistent/thorctl/doc.yml"), "{err}");
    }

    /// Rendered documents round trip through the parser
    #[test]
    fn rendering_round_trips() {
        let value = json!({"a":1,"b":["x"],"c":{"d":null}});
        assert_eq!(
            parse_document(&render_yaml(&value).unwrap(), "x").unwrap(),
            value
        );
        let json = render_json(&value).unwrap();
        assert!(json.ends_with("}\n"));
        assert_eq!(parse_document(&json, "x").unwrap(), value);
    }
}
