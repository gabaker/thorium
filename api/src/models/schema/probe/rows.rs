//! The Scylla half of the probe: the `system_schema` rows it reads for Thorium's keyspace and the
//! canonical forms it builds from them
//!
//! `system_schema` doesn't echo the DDL Thorium sent, so the rows are parsed rather than compared
//! as text: column types are normalized to one spelling, keys are rebuilt from each column's kind
//! and position, and a view's `where_clause` is split into conjuncts with the `IS NOT NULL` ones on
//! key columns dropped, so a clause Scylla stores in its own order and case compares equal.

use scylla::client::session::Session;
use scylla::deserialize::row::DeserializeRow;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use super::super::{Fingerprint, SnapshotObject, canonical_type};
use crate::models::upgrades::SchemaBackend;

/// Why the probe couldn't observe a backend's schema
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// A read of the live schema failed
    Query {
        /// The statement, or what was being read
        statement: String,
        /// The error the backend returned
        error: String,
    },
    /// A stored definition couldn't be put in canonical form
    Unreadable {
        /// The object whose definition couldn't be read
        object: String,
        /// Why it couldn't be read
        reason: String,
    },
}

impl ProbeError {
    /// Build the error for a failed read
    ///
    /// # Arguments
    ///
    /// * `statement` - The statement, or what was being read
    /// * `error` - The error the backend returned
    pub(super) fn query(statement: &str, error: impl std::fmt::Display) -> Self {
        // keep the statement and the backend's message
        ProbeError::Query {
            statement: statement.to_owned(),
            error: error.to_string(),
        }
    }

    /// Build the error for a definition that can't be put in canonical form
    ///
    /// # Arguments
    ///
    /// * `object` - The object whose definition couldn't be read
    /// * `reason` - Why it couldn't be read
    pub(super) fn unreadable(object: &str, reason: impl std::fmt::Display) -> Self {
        // name the object and the reason
        ProbeError::Unreadable {
            object: object.to_owned(),
            reason: reason.to_string(),
        }
    }
}

impl std::fmt::Display for ProbeError {
    /// Describe why the probe stopped
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Query { statement, error } => {
                write!(f, "reading the live schema failed ({statement}): {error}")
            }
            ProbeError::Unreadable { object, reason } => {
                write!(f, "the live definition of {object} can't be read: {reason}")
            }
        }
    }
}

impl std::error::Error for ProbeError {}

/// One `system_schema.columns` row
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnRow {
    /// The table or view the column belongs to
    pub table_name: String,
    /// The column's name
    pub column_name: String,
    /// `partition_key`, `clustering`, `static`, or `regular`
    pub kind: String,
    /// The column's place in its key, `-1` for a column outside the key
    pub position: i32,
    /// `ASC`, `DESC`, or `NONE` for a column outside the clustering key
    pub clustering_order: String,
    /// The column's CQL type as Scylla renders it
    #[serde(rename = "type")]
    pub cql_type: String,
}

/// One `system_schema.views` row
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRow {
    /// The view's name
    pub view_name: String,
    /// The base table the view selects from
    pub base_table_name: String,
    /// The view's filter as Scylla renders it
    pub where_clause: String,
    /// Whether the view selects every column of its base (`SELECT *`)
    pub include_all_columns: bool,
}

/// One `system_schema.types` row
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeRow {
    /// The user-defined type's name
    pub type_name: String,
    /// Its field names, in order
    pub field_names: Vec<String>,
    /// Its field types, in the order of the names
    pub field_types: Vec<String>,
}

/// One `system_schema.indexes` row
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRow {
    /// The table the index is on
    pub table_name: String,
    /// The index's name
    pub index_name: String,
    /// The index's kind, such as `COMPOSITES`
    pub kind: String,
    /// The index's options, such as its `target`
    pub options: BTreeMap<String, String>,
}

/// What `system_schema` holds for one keyspace: the rows the Scylla probe reads, kept apart from
/// their canonical forms so rows captured from a real Scylla can be replayed in tests
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaRows {
    /// The keyspace's replication map as Scylla stores it (strings only), empty when the keyspace
    /// doesn't exist
    pub replication: BTreeMap<String, String>,
    /// Every table's name
    pub tables: Vec<String>,
    /// Every column of every table and view
    pub columns: Vec<ColumnRow>,
    /// Every materialized view, including those backing secondary indexes
    pub views: Vec<ViewRow>,
    /// Every user-defined type
    pub types: Vec<TypeRow>,
    /// Every secondary index
    pub indexes: Vec<IndexRow>,
}

/// Read every row of a `system_schema` query on one keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `cql` - The query, binding only the keyspace's name
/// * `keyspace` - The keyspace, as Scylla stores its name
async fn read<R>(session: &Session, cql: &str, keyspace: &str) -> Result<Vec<R>, ProbeError>
where
    R: for<'frame, 'metadata> DeserializeRow<'frame, 'metadata>,
{
    // run the read; the schema tables are small, so one unpaged read holds every row
    let rows = session
        .query_unpaged(cql, (keyspace,))
        .await
        .map_err(|err| ProbeError::query(cql, err))?
        .into_rows_result()
        .map_err(|err| ProbeError::query(cql, err))?;
    // deserialize each row
    rows.rows::<R>()
        .map_err(|err| ProbeError::query(cql, err))?
        .map(|row| row.map_err(|err| ProbeError::query(cql, err)))
        .collect()
}

impl SchemaRows {
    /// Read what `system_schema` holds for a keyspace; only reads
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to read with
    /// * `keyspace` - The keyspace, as Scylla stores its name (lowercase)
    pub async fn read(session: &Session, keyspace: &str) -> Result<SchemaRows, ProbeError> {
        // the keyspace's replication map, which is absent when the keyspace is
        let replication = read::<(BTreeMap<String, String>,)>(
            session,
            "SELECT replication FROM system_schema.keyspaces WHERE keyspace_name = ?",
            keyspace,
        )
        .await?
        .into_iter()
        .next()
        .map_or_else(BTreeMap::new, |(map,)| map);
        // every table's name
        let tables = read::<(String,)>(
            session,
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
            keyspace,
        )
        .await?
        .into_iter()
        .map(|(name,)| name)
        .collect();
        // every column of every table and view
        let columns = read::<(String, String, String, i32, String, String)>(
            session,
            "SELECT table_name, column_name, kind, position, clustering_order, type \
             FROM system_schema.columns WHERE keyspace_name = ?",
            keyspace,
        )
        .await?
        .into_iter()
        .map(
            |(table_name, column_name, kind, position, clustering_order, cql_type)| ColumnRow {
                table_name,
                column_name,
                kind,
                position,
                clustering_order,
                cql_type,
            },
        )
        .collect();
        // every view with its filter
        let views = read::<(String, String, Option<String>, Option<bool>)>(
            session,
            "SELECT view_name, base_table_name, where_clause, include_all_columns \
             FROM system_schema.views WHERE keyspace_name = ?",
            keyspace,
        )
        .await?
        .into_iter()
        .map(
            |(view_name, base_table_name, where_clause, include_all_columns)| ViewRow {
                view_name,
                base_table_name,
                where_clause: where_clause.unwrap_or_default(),
                include_all_columns: include_all_columns.unwrap_or_default(),
            },
        )
        .collect();
        // every user-defined type, which Thorium declares none of
        let types = read::<(String, Option<Vec<String>>, Option<Vec<String>>)>(
            session,
            "SELECT type_name, field_names, field_types FROM system_schema.types \
             WHERE keyspace_name = ?",
            keyspace,
        )
        .await?
        .into_iter()
        .map(|(type_name, field_names, field_types)| TypeRow {
            type_name,
            field_names: field_names.unwrap_or_default(),
            field_types: field_types.unwrap_or_default(),
        })
        .collect();
        // every secondary index, which Thorium declares none of
        let indexes = read::<(
            String,
            String,
            Option<String>,
            Option<BTreeMap<String, String>>,
        )>(
            session,
            "SELECT table_name, index_name, kind, options FROM system_schema.indexes \
             WHERE keyspace_name = ?",
            keyspace,
        )
        .await?
        .into_iter()
        .map(|(table_name, index_name, kind, options)| IndexRow {
            table_name,
            index_name,
            kind: kind.unwrap_or_default(),
            options: options.unwrap_or_default(),
        })
        .collect();
        Ok(SchemaRows {
            replication,
            tables,
            columns,
            views,
            types,
            indexes,
        })
    }

    /// Build the canonical form of every object these rows describe, sorted by name
    ///
    /// Tables and views take the registry's canonical form exactly; user-defined types and
    /// secondary indexes, which Thorium never declares, take a form of their own so they are
    /// reported as unexpected. The views Scylla creates to back secondary indexes belong to their
    /// index and aren't listed.
    pub fn objects(&self) -> Result<Vec<SnapshotObject>, ProbeError> {
        // the views that back a secondary index are named after it
        let backing = self
            .indexes
            .iter()
            .map(|index| format!("{}_index", index.index_name))
            .collect::<BTreeSet<_>>();
        // group the columns under their table or view
        let mut columns: BTreeMap<&str, Vec<&ColumnRow>> = BTreeMap::new();
        for column in &self.columns {
            columns
                .entry(column.table_name.as_str())
                .or_default()
                .push(column);
        }
        let none = Vec::new();
        let mut objects = Vec::new();
        // each table, then each view that isn't an index's
        for table in &self.tables {
            let form = canonical_table(table, columns.get(table.as_str()).unwrap_or(&none))?;
            objects.push(form);
        }
        for view in self
            .views
            .iter()
            .filter(|view| !backing.contains(&view.view_name))
        {
            let view_columns = columns.get(view.view_name.as_str()).unwrap_or(&none);
            objects.push(canonical_view(view, view_columns)?);
        }
        // then the types and indexes Thorium doesn't declare
        for udt in &self.types {
            objects.push(canonical_udt(udt)?);
        }
        for index in &self.indexes {
            objects.push(json!({
                "kind": "secondary_index",
                "name": index.index_name,
                "table": index.table_name,
                "index_kind": index.kind.to_lowercase(),
                "options": index.options,
            }));
        }
        // fingerprint each form and sort the objects by name
        let mut objects = objects
            .into_iter()
            .map(|canonical| SnapshotObject {
                backend: SchemaBackend::Scylla,
                name: canonical["name"].as_str().unwrap_or_default().to_owned(),
                fingerprint: Fingerprint::of(&canonical),
                canonical,
            })
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(objects)
    }
}

/// The parts of a table's or view's canonical form its columns give: the sorted columns with
/// their canonical types, the partition key, the full clustering order, and the static columns
struct ColumnForms {
    /// Every column, sorted by name, with its canonical type
    columns: Value,
    /// The partition key's column names, in key order
    partition_key: Value,
    /// The clustering columns with their orders, in key order
    clustering_key: Value,
    /// The static columns' names, sorted
    statics: Value,
    /// The names of every key column
    keys: BTreeSet<String>,
}

/// Build the column parts of an object's canonical form from its `system_schema.columns` rows
///
/// # Arguments
///
/// * `object` - The table or view the columns belong to
/// * `rows` - Its column rows
fn column_forms(object: &str, rows: &[&ColumnRow]) -> Result<ColumnForms, ProbeError> {
    // sort the columns by name, each with its one canonical type spelling
    let mut sorted = rows.to_vec();
    sorted.sort_by(|left, right| left.column_name.cmp(&right.column_name));
    let mut columns = Vec::with_capacity(sorted.len());
    for row in &sorted {
        let cql_type = canonical_type(&row.cql_type)
            .map_err(|err| ProbeError::unreadable(&format!("{object}.{}", row.column_name), err))?;
        columns.push(json!({ "name": row.column_name, "type": cql_type }));
    }
    // rebuild the keys from each column's kind and position
    let mut partition = Vec::new();
    let mut clustering = Vec::new();
    let mut statics = Vec::new();
    for row in &sorted {
        match row.kind.to_lowercase().as_str() {
            "partition_key" => partition.push((row.position, json!(row.column_name))),
            "clustering" => {
                // a clustering column Scylla stores without an order is ascending
                let order = if row.clustering_order.eq_ignore_ascii_case("desc") {
                    "desc"
                } else {
                    "asc"
                };
                let column = json!({ "name": row.column_name, "order": order });
                clustering.push((row.position, column));
            }
            "static" => statics.push(json!(row.column_name)),
            "regular" => (),
            other => {
                let reason = format!("column {} has an unknown kind {other}", row.column_name);
                return Err(ProbeError::unreadable(object, reason));
            }
        }
    }
    // put each key in key order
    partition.sort_by_key(|(position, _)| *position);
    clustering.sort_by_key(|(position, _)| *position);
    let keys = sorted
        .iter()
        .filter(|row| matches!(row.kind.as_str(), "partition_key" | "clustering"))
        .map(|row| row.column_name.clone())
        .collect();
    let in_order = |key: Vec<(i32, Value)>| Value::Array(key.into_iter().map(|(_, c)| c).collect());
    Ok(ColumnForms {
        columns: Value::Array(columns),
        partition_key: in_order(partition),
        clustering_key: in_order(clustering),
        statics: Value::Array(statics),
        keys,
    })
}

/// Build a table's canonical form from its column rows
///
/// # Arguments
///
/// * `name` - The table's name
/// * `rows` - Its column rows
fn canonical_table(name: &str, rows: &[&ColumnRow]) -> Result<Value, ProbeError> {
    // the table's form is its columns and keys
    let forms = column_forms(name, rows)?;
    Ok(json!({
        "kind": "table",
        "name": name,
        "columns": forms.columns,
        "partition_key": forms.partition_key,
        "clustering_key": forms.clustering_key,
        "static": forms.statics,
    }))
}

/// Build a view's canonical form from its row and its column rows
///
/// # Arguments
///
/// * `view` - The view's row
/// * `rows` - Its column rows
fn canonical_view(view: &ViewRow, rows: &[&ColumnRow]) -> Result<Value, ProbeError> {
    // the view's form is its columns, keys, base, and the filter beyond its key's `IS NOT NULL`s
    let forms = column_forms(&view.view_name, rows)?;
    let keys = forms
        .keys
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let filter = canonical_filter(&view.where_clause, &keys)
        .map_err(|reason| ProbeError::unreadable(&view.view_name, reason))?;
    Ok(json!({
        "kind": "view",
        "name": view.view_name,
        "base": view.base_table_name,
        "columns": forms.columns,
        "partition_key": forms.partition_key,
        "clustering_key": forms.clustering_key,
        "filter": filter,
    }))
}

/// Build a user-defined type's canonical form, its fields kept in order
///
/// # Arguments
///
/// * `udt` - The type's row
fn canonical_udt(udt: &TypeRow) -> Result<Value, ProbeError> {
    // a type's fields are ordered, so they keep their stored order
    let mut fields = Vec::with_capacity(udt.field_names.len());
    for (name, cql_type) in udt.field_names.iter().zip(&udt.field_types) {
        let cql_type = canonical_type(cql_type)
            .map_err(|err| ProbeError::unreadable(&format!("{}.{name}", udt.type_name), err))?;
        fields.push(json!({ "name": name, "type": cql_type }));
    }
    Ok(json!({ "kind": "type", "name": udt.type_name, "fields": fields }))
}

/// One token of a view filter
#[derive(Debug, Clone, PartialEq, Eq)]
enum FilterToken {
    /// An identifier, keyword, or literal word, lowercased unless it was quoted
    Word(String),
    /// A quoted identifier that has to stay quoted, quotes included
    Quoted(String),
    /// A string literal, quotes included
    Text(String),
    /// An operator or punctuation
    Symbol(String),
}

impl FilterToken {
    /// Render this token as it appears in a canonical filter
    fn render(&self) -> &str {
        match self {
            FilterToken::Word(text)
            | FilterToken::Quoted(text)
            | FilterToken::Text(text)
            | FilterToken::Symbol(text) => text,
        }
    }

    /// Whether this token is the unquoted keyword or identifier given (lowercase)
    ///
    /// # Arguments
    ///
    /// * `word` - The lowercase word to match
    fn is(&self, word: &str) -> bool {
        // only unquoted words match a keyword
        matches!(self, FilterToken::Word(text) if text == word)
    }
}

/// Read a quoted run of a filter up to its closing quote, a doubled quote standing for one
///
/// # Arguments
///
/// * `quote` - The quote character
/// * `chars` - The characters after the opening quote
fn quoted(
    quote: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<String, String> {
    // collect up to the closing quote, keeping doubled quotes as they are
    let mut inner = String::new();
    loop {
        match chars.next() {
            Some(c) if c == quote && chars.peek() == Some(&quote) => {
                chars.next();
                inner.push(quote);
                inner.push(quote);
            }
            Some(c) if c == quote => return Ok(inner),
            Some(c) => inner.push(c),
            None => return Err(format!("an unterminated {quote} in the view filter")),
        }
    }
}

/// Split a view filter into tokens
///
/// Unquoted words are lowercased, since CQL identifiers and keywords are case-insensitive; a
/// quoted identifier that is a plain lowercase name loses its quotes, the way Scylla renders it.
///
/// # Arguments
///
/// * `clause` - The filter's text
fn filter_tokens(clause: &str) -> Result<Vec<FilterToken>, String> {
    // walk the clause one character at a time
    let mut tokens = Vec::new();
    let mut chars = clause.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => (),
            '"' => {
                // a plain lowercase name doesn't need its quotes
                let inner = quoted('"', &mut chars)?;
                let plain = inner.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && inner
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if plain {
                    tokens.push(FilterToken::Word(inner));
                } else {
                    tokens.push(FilterToken::Quoted(format!("\"{inner}\"")));
                }
            }
            '\'' => {
                let inner = quoted('\'', &mut chars)?;
                tokens.push(FilterToken::Text(format!("'{inner}'")));
            }
            c if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' => {
                // a word runs to the next character that can't be in a name or a number
                let mut word = c.to_lowercase().to_string();
                while let Some(next) = chars
                    .peek()
                    .filter(|n| n.is_alphanumeric() || **n == '_' || **n == '-' || **n == '.')
                {
                    word.extend(next.to_lowercase());
                    chars.next();
                }
                tokens.push(FilterToken::Word(word));
            }
            '<' | '>' | '!' | '=' => {
                // a comparison is one or two symbols
                let mut symbol = c.to_string();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    symbol.push('=');
                }
                tokens.push(FilterToken::Symbol(symbol));
            }
            '(' | ')' | ',' | '[' | ']' | '{' | '}' | ':' | '+' | '*' | '/' | '%' => {
                tokens.push(FilterToken::Symbol(c.to_string()));
            }
            other => return Err(format!("an unexpected {other} in the view filter")),
        }
    }
    Ok(tokens)
}

/// Normalize a view filter into the canonical form's extra filter: its conjuncts, each with one
/// spelling, without the `IS NOT NULL` conjuncts on key columns, deduplicated, sorted, and joined
/// with ` AND `; `None` when nothing is left
///
/// A view must filter `IS NOT NULL` on every key column, so those conjuncts say nothing the key
/// doesn't, and Scylla stores them in its own order and case (`id IS NOT null AND sha256 IS NOT
/// null`), so they are dropped rather than compared. The registry declares at most the one extra
/// condition (`default_policy = true`), already in this form.
///
/// # Arguments
///
/// * `clause` - The filter as written or as Scylla stores it
/// * `keys` - The view's key columns
pub fn canonical_filter(clause: &str, keys: &BTreeSet<&str>) -> Result<Option<String>, String> {
    // split the tokens into conjuncts at each top-level `AND`
    let tokens = filter_tokens(clause)?;
    let mut conjuncts: Vec<Vec<FilterToken>> = vec![Vec::new()];
    let mut depth = 0_usize;
    for token in tokens {
        match token.render() {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            _ => (),
        }
        if depth == 0 && token.is("and") {
            conjuncts.push(Vec::new());
        } else if let Some(last) = conjuncts.last_mut() {
            last.push(token);
        }
    }
    // drop the `IS NOT NULL` conjuncts on key columns and render the rest with single spaces
    let mut kept = BTreeSet::new();
    for conjunct in conjuncts {
        if conjunct.is_empty() {
            continue;
        }
        let not_null_key = matches!(
            conjunct.as_slice(),
            [FilterToken::Word(column), is, not, null]
                if is.is("is") && not.is("not") && null.is("null") && keys.contains(column.as_str())
        );
        if !not_null_key {
            let text = conjunct
                .iter()
                .map(FilterToken::render)
                .collect::<Vec<_>>()
                .join(" ");
            kept.insert(text);
        }
    }
    Ok((!kept.is_empty()).then(|| kept.into_iter().collect::<Vec<_>>().join(" AND ")))
}

/// Normalize a keyspace's stored replication map: the strategy class without its package prefix,
/// replication factors as integers, keys sorted
///
/// The keyspace isn't part of the canonical form; this is the form it would take there.
///
/// # Arguments
///
/// * `replication` - The map as `system_schema.keyspaces` stores it
#[must_use]
pub fn canonical_replication(replication: &BTreeMap<String, String>) -> Value {
    // a BTreeMap keeps its keys sorted
    let mut form = serde_json::Map::new();
    for (key, value) in replication {
        let value = if key == "class" {
            json!(value.rsplit('.').next().unwrap_or(value))
        } else {
            value
                .parse::<u64>()
                .map_or_else(|_| json!(value), |n| json!(n))
        };
        form.insert(key.clone(), value);
    }
    Value::Object(form)
}
