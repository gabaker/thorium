//! Shared output formats for Thorctl views
//!
//! A view is the data one command shows. It serializes as the document that the `json` and
//! `yaml` formats print, and renders itself for people as a table or as human-readable text.

use serde::Serialize;
use serde_json::Value;
use tabled::builder::Builder;
use tabled::settings::Style;
use tabled::{Table, Tabled};
use thorium::Error;

/// The text that stands in for a secret in every output format
pub const REDACTED: &str = "<redacted>";

/// The formats a Thorctl view can be printed in
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// A table with one row per item
    Table,
    /// Labeled fields for people to read
    Human,
    /// The API's response as JSON
    Json,
    /// The same document as the JSON format, written as YAML
    Yaml,
}

/// The data a single command shows, in a form that can be printed in any [`OutputFormat`]
///
/// The view's own serialization is the document the `json` and `yaml` formats print, so a
/// view holding API responses serializes them unchanged. Views never hold a secret.
pub trait View: Serialize {
    /// Render this view as a table
    fn table(&self) -> String;

    /// Render this view as human-readable text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text with ANSI escape codes
    fn human(&self, ansi: bool) -> String;

    /// The items a `--wait-for` condition must hold for
    ///
    /// Defaults to each element when the document is a list, or the document itself.
    fn wait_items(&self) -> Result<Vec<Value>, Error> {
        // convert our document to a JSON value so conditions can inspect it
        let document = to_value(self)?;
        // check every element of a list, or the document itself otherwise
        match document {
            Value::Array(items) => Ok(items),
            document => Ok(vec![document]),
        }
    }
}

/// Convert a serializable value into a JSON value
///
/// # Arguments
///
/// * `value` - The value to convert
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(|err| Error::new(format!("Failed to serialize: {err}")))
}

/// Serialize a document as pretty printed JSON ending in a newline
///
/// # Arguments
///
/// * `document` - The document to serialize
pub fn to_json<T: Serialize + ?Sized>(document: &T) -> Result<String, Error> {
    // serialize our document as pretty JSON
    let mut json = serde_json::to_string_pretty(document)
        .map_err(|err| Error::new(format!("Failed to serialize JSON: {err}")))?;
    // end our output with a newline for the terminal
    json.push('\n');
    Ok(json)
}

/// Serialize a document as YAML
///
/// # Arguments
///
/// * `document` - The document to serialize
pub fn to_yaml<T: Serialize + ?Sized>(document: &T) -> Result<String, Error> {
    serde_norway::to_string(document)
        .map_err(|err| Error::new(format!("Failed to serialize YAML: {err}")))
}

/// Render rows of a [`Tabled`] type as a table
///
/// # Arguments
///
/// * `rows` - The rows to render
pub fn table<R: Tabled, I: IntoIterator<Item = R>>(rows: I) -> String {
    Table::new(rows).with(Style::psql()).to_string()
}

/// Render rows of strings under a set of column names as a table
///
/// # Arguments
///
/// * `columns` - The column names
/// * `rows` - The rows to render, each with one value per column
pub fn table_from(columns: &[&str], rows: Vec<Vec<String>>) -> String {
    // start our table with its header row
    let mut builder = Builder::default();
    builder.push_record(columns.iter().copied());
    // add each of our rows
    for row in rows {
        builder.push_record(row);
    }
    // build and style our table
    builder.build().with(Style::psql()).to_string()
}

/// Decide whether a stream gets ANSI styling from the terminal and environment facts
///
/// Styling is for a terminal only, and a non-empty `NO_COLOR` or `TERM=dumb` turns it off even
/// there.
///
/// # Arguments
///
/// * `is_terminal` - Whether the stream being written is a terminal
/// * `no_color` - The value of `NO_COLOR`, if set
/// * `term` - The value of `TERM`, if set
#[must_use]
pub fn ansi_for(
    is_terminal: bool,
    no_color: Option<&std::ffi::OsStr>,
    term: Option<&std::ffi::OsStr>,
) -> bool {
    // a pipe or file never gets escape sequences
    if !is_terminal {
        return false;
    }
    // NO_COLOR (when not empty) and a dumb terminal ask for plain text
    let no_color = no_color.is_some_and(|value| !value.is_empty());
    let dumb = term.is_some_and(|term| term == "dumb");
    !no_color && !dumb
}

/// Whether output to a stream should get ANSI styling in this process's environment
///
/// # Arguments
///
/// * `is_terminal` - Whether the stream being written is a terminal
#[must_use]
pub fn ansi_allowed(is_terminal: bool) -> bool {
    // read the two variables once and decide
    ansi_for(
        is_terminal,
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
    )
}

/// Whether stdout is an interactive terminal that should get ANSI styling
#[must_use]
pub fn stdout_ansi() -> bool {
    ansi_allowed(std::io::IsTerminal::is_terminal(&std::io::stdout()))
}

/// Render a view in a format
///
/// # Arguments
///
/// * `view` - The view to render
/// * `format` - The format to render it in
/// * `ansi` - Whether human-readable text gets ANSI styling
pub fn render<V: View + ?Sized>(
    view: &V,
    format: OutputFormat,
    ansi: bool,
) -> Result<String, Error> {
    match format {
        // render a table ending in a newline
        OutputFormat::Table => Ok(format!("{}\n", view.table())),
        // render text for people ending in a newline
        OutputFormat::Human => {
            // render our text
            let mut text = view.human(ansi);
            // make sure our text ends in a newline
            if !text.ends_with('\n') {
                text.push('\n');
            }
            Ok(text)
        }
        // print the view's document as JSON
        OutputFormat::Json => to_json(view),
        // print the view's JSON document as YAML, so enums print as maps like they do in JSON
        OutputFormat::Yaml => to_yaml(&to_value(view)?),
    }
}

/// Render a view in a format and print it to stdout
///
/// # Arguments
///
/// * `view` - The view to print
/// * `format` - The format to print it in
pub fn print<V: View + ?Sized>(view: &V, format: OutputFormat) -> Result<(), Error> {
    // render our view for stdout
    let rendered = render(view, format, stdout_ansi())?;
    // print our view without adding another newline
    print!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    /// Only a terminal that is not asking for plain text gets styling
    #[test]
    fn ansi_needs_a_plain_text_free_terminal() {
        // a terminal with nothing set is styled
        assert!(super::ansi_for(true, None, Some(OsStr::new("xterm"))));
        assert!(super::ansi_for(true, None, None));
        // a pipe never is
        assert!(!super::ansi_for(false, None, Some(OsStr::new("xterm"))));
        // NO_COLOR turns styling off, unless it is empty
        assert!(!super::ansi_for(true, Some(OsStr::new("1")), None));
        assert!(super::ansi_for(true, Some(OsStr::new("")), None));
        // a dumb terminal can't show it
        assert!(!super::ansi_for(true, None, Some(OsStr::new("dumb"))));
    }

    use super::{OutputFormat, View, render, table_from, to_value};
    use serde::Serialize;
    use serde_json::json;

    /// A test item holding fields in a fixed order
    #[derive(Serialize)]
    struct Item {
        /// The item's name
        name: String,
        /// The item's status
        status: String,
    }

    /// A test view over a list of items
    #[derive(Serialize)]
    #[serde(transparent)]
    struct Items(Vec<Item>);

    impl View for Items {
        /// Render the items as a table
        fn table(&self) -> String {
            // build one row per item
            let rows = self
                .0
                .iter()
                .map(|item| vec![item.name.clone(), item.status.clone()])
                .collect();
            table_from(&["NAME", "STATUS"], rows)
        }

        /// Render the items as text
        ///
        /// # Arguments
        ///
        /// * `_ansi` - Whether to style the text
        fn human(&self, _ansi: bool) -> String {
            // print one line per item
            self.0
                .iter()
                .map(|item| format!("{}: {}", item.name, item.status))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    /// Build a test view with two items
    fn items() -> Items {
        Items(vec![
            Item {
                name: "alpha".to_owned(),
                status: "Completed".to_owned(),
            },
            Item {
                name: "beta".to_owned(),
                status: "Created".to_owned(),
            },
        ])
    }

    /// `json` prints the view's own serialization, in its field order
    #[test]
    fn json_is_the_document() {
        // render our view as JSON
        let rendered = render(&items(), OutputFormat::Json, false).unwrap();
        // the output must be the view's own pretty serialization
        assert_eq!(
            rendered,
            serde_json::to_string_pretty(&items()).unwrap() + "\n"
        );
        // the fields must keep the struct's order
        let name = rendered.find("\"name\"").unwrap();
        let status = rendered.find("\"status\"").unwrap();
        assert!(name < status);
    }

    /// `yaml` prints the same document as `json`
    #[test]
    fn yaml_is_the_same_document() {
        // render our view as YAML and parse it back
        let rendered = render(&items(), OutputFormat::Yaml, false).unwrap();
        let parsed: serde_json::Value = serde_norway::from_str(&rendered).unwrap();
        // the YAML document must match the JSON document
        assert_eq!(parsed, to_value(&items()).unwrap());
    }

    /// `table` and `human` render every item
    #[test]
    fn table_and_human_render() {
        // render our view as a table and as text
        let table = render(&items(), OutputFormat::Table, false).unwrap();
        let human = render(&items(), OutputFormat::Human, false).unwrap();
        // the table has a header and both rows
        assert!(table.contains("NAME") && table.contains("STATUS"));
        assert!(table.contains("alpha") && table.contains("beta"));
        // the text has both items and ends in a newline
        assert!(human.contains("alpha: Completed") && human.contains("beta: Created"));
        assert!(human.ends_with('\n'));
    }

    /// Conditions check each element of a list document
    #[test]
    fn wait_items_are_list_elements() {
        // get the items a condition would check
        let items = items().wait_items().unwrap();
        // each element of our list is one item
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["name"], json!("alpha"));
    }
}
