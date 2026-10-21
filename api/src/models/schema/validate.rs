//! Static validation of registered statements against the declared schema
//!
//! A small CQL tokenizer reads each statement's target, the columns it selects, writes, sets, and
//! restricts, and its trailing clauses, then checks them against the declared objects: the
//! columns exist, the `WHERE` clause restricts the whole partition key, one-sided range
//! comparisons on clustering columns point the way the read order implies, and no statement is
//! conditional or updates a counter.

use std::fmt;

use super::definition::{ClusterOrder, ColumnDef, ColumnRole, ObjectDef};
use super::statements::RegisteredStatement;

/// A rule of statement validation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// The statement uses syntax the validator doesn't understand
    Syntax,
    /// The statement targets an object that isn't declared
    UnknownObject,
    /// The statement targets a different object than the one it's registered with
    WrongObject,
    /// The statement names a column its object doesn't have
    UnknownColumn,
    /// The statement's `WHERE` clause doesn't restrict every partition key column
    PartitionKey,
    /// The statement restricts a column that isn't part of the primary key
    NonKeyRestriction,
    /// The statement compares a column that isn't a clustering column by range
    RangeColumn,
    /// A one-sided range comparison points against the order the statement reads rows in
    ClusteringOrder,
    /// The statement is conditional (`IF`, `IF EXISTS`, or `IF NOT EXISTS`)
    Conditional,
    /// The statement updates a counter
    Counter,
}

impl Rule {
    /// The rule's name, as validation errors report it
    #[must_use]
    pub const fn name(self) -> &'static str {
        // each rule's name reads as what the statement does wrong
        match self {
            Rule::Syntax => "syntax",
            Rule::UnknownObject => "unknown object",
            Rule::WrongObject => "wrong object",
            Rule::UnknownColumn => "unknown column",
            Rule::PartitionKey => "partition key not fully restricted",
            Rule::NonKeyRestriction => "restriction on a non-key column",
            Rule::RangeColumn => "range on a non-clustering column",
            Rule::ClusteringOrder => "comparison against the clustering order",
            Rule::Conditional => "conditional statement",
            Rule::Counter => "counter update",
        }
    }
}

/// A registered statement that breaks a validation rule
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementError {
    /// The statement's registered name
    pub statement: String,
    /// The rule it breaks
    pub rule: Rule,
    /// What breaks the rule
    pub detail: String,
}

impl fmt::Display for StatementError {
    /// Name the statement, the rule, and what breaks it
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the statement first, so every error is found by its name
        write!(
            f,
            "{}: {}: {}",
            self.statement,
            self.rule.name(),
            self.detail
        )
    }
}

impl std::error::Error for StatementError {}

/// One token of a statement
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// An unquoted identifier or keyword, or the `{ns}` keyspace placeholder
    Word(String),
    /// A double-quoted identifier, without its quotes
    Quoted(String),
    /// A single-quoted string literal
    Text,
    /// A number literal
    Number,
    /// A bind marker
    Marker,
    /// A symbol such as `(`, `,`, `<=`, or `*`
    Symbol(&'static str),
}

/// The symbols the tokenizer recognizes, longest first
const SYMBOLS: [&str; 15] = [
    "<=", ">=", "!=", "(", ")", ",", ".", "=", "<", ">", "[", "]", "+", "-", "*",
];

/// Split a statement into tokens
///
/// # Arguments
///
/// * `cql` - The statement's text
fn tokenize(cql: &str) -> Result<Vec<Token>, String> {
    // walk the text one token at a time
    let mut tokens = Vec::new();
    let mut rest = cql.trim_start();
    while let Some(first) = rest.chars().next() {
        // read the token that starts here and how much of the text it covers
        let (token, len) = if first == '{' {
            // the keyspace placeholder reads as one identifier
            let end = rest
                .find('}')
                .ok_or_else(|| format!("unclosed placeholder at `{rest}`"))?;
            (Token::Word(rest[..=end].to_owned()), end + 1)
        } else if first.is_ascii_alphabetic() || first == '_' {
            // identifiers and keywords run until the first character that can't be in one
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            (Token::Word(rest[..end].to_owned()), end)
        } else if first.is_ascii_digit() {
            // number literals run until the first character that can't be in one
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
                .unwrap_or(rest.len());
            (Token::Number, end)
        } else if first == '"' || first == '\'' {
            // quoted text runs to the matching quote
            let end = rest[1..]
                .find(first)
                .ok_or_else(|| format!("unclosed quote at `{rest}`"))?
                + 1;
            let token = if first == '"' {
                Token::Quoted(rest[1..end].to_owned())
            } else {
                Token::Text
            };
            (token, end + 1)
        } else if first == '?' {
            // a bind marker
            (Token::Marker, 1)
        } else {
            // anything else must be a known symbol
            let symbol = SYMBOLS
                .iter()
                .find(|symbol| rest.starts_with(**symbol))
                .ok_or_else(|| format!("unexpected `{first}`"))?;
            (Token::Symbol(symbol), symbol.len())
        };
        tokens.push(token);
        rest = rest[len..].trim_start();
    }
    Ok(tokens)
}

/// A comparison in a `WHERE` clause
#[derive(Debug, Clone, PartialEq, Eq)]
enum Predicate {
    /// A column compared with a value
    Column {
        /// The column's name
        column: String,
        /// The comparison operator, `IN` for an `IN` list
        op: &'static str,
    },
    /// The token of some columns compared with a value
    Token {
        /// The columns whose token is compared
        columns: Vec<String>,
    },
}

/// An assignment in an `UPDATE`'s `SET` clause
#[derive(Debug, Clone, PartialEq, Eq)]
struct Assignment {
    /// The column assigned to
    column: String,
    /// Whether one element of a collection is assigned, as in `children[?] = ?`
    element: bool,
    /// Whether the new value reads the column itself, as in `n = n + 1`
    reads_itself: bool,
}

/// The parts of a statement that validation checks
#[derive(Debug, Default)]
struct Parsed {
    /// The statement's verb, upper-cased
    verb: &'static str,
    /// The object the statement targets, without its keyspace
    target: String,
    /// The columns the statement selects, inserts, or deletes
    columns: Vec<String>,
    /// The assignments of an `UPDATE`
    assignments: Vec<Assignment>,
    /// The `WHERE` clause's comparisons
    predicates: Vec<Predicate>,
    /// The columns of a `GROUP BY` clause
    group_by: Vec<String>,
    /// The columns of an `ORDER BY` clause, with whether each is descending
    order_by: Vec<(String, bool)>,
    /// Whether the statement has a `WHERE` clause
    restricted: bool,
}

/// A cursor over a statement's tokens
struct Parser<'a> {
    /// The statement's tokens
    tokens: &'a [Token],
    /// The position of the next token
    at: usize,
}

impl Parser<'_> {
    /// The next token, if any, without consuming it
    fn peek(&self) -> Option<&Token> {
        // None once every token is consumed
        self.tokens.get(self.at)
    }

    /// Whether the next token is a keyword, compared without case
    ///
    /// # Arguments
    ///
    /// * `keyword` - The keyword to look for
    fn at_keyword(&self, keyword: &str) -> bool {
        // keywords are words compared without case
        matches!(self.peek(), Some(Token::Word(word)) if word.eq_ignore_ascii_case(keyword))
    }

    /// Consume the next token when it's a keyword, returning whether it was
    ///
    /// # Arguments
    ///
    /// * `keyword` - The keyword to consume
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        // only move past the token when it matches
        let found = self.at_keyword(keyword);
        if found {
            self.at += 1;
        }
        found
    }

    /// Consume a keyword that must come next
    ///
    /// # Arguments
    ///
    /// * `keyword` - The keyword to consume
    fn expect_keyword(&mut self, keyword: &str) -> Result<(), String> {
        // fail naming what came instead of the keyword
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(format!("expected {keyword} at {:?}", self.peek()))
        }
    }

    /// Consume the next token when it's a symbol, returning whether it was
    ///
    /// # Arguments
    ///
    /// * `symbol` - The symbol to consume
    fn eat_symbol(&mut self, symbol: &str) -> bool {
        // only move past the token when it matches
        let found = matches!(self.peek(), Some(Token::Symbol(next)) if *next == symbol);
        if found {
            self.at += 1;
        }
        found
    }

    /// Consume a symbol that must come next
    ///
    /// # Arguments
    ///
    /// * `symbol` - The symbol to consume
    fn expect_symbol(&mut self, symbol: &str) -> Result<(), String> {
        // fail naming what came instead of the symbol
        if self.eat_symbol(symbol) {
            Ok(())
        } else {
            Err(format!("expected `{symbol}` at {:?}", self.peek()))
        }
    }

    /// Consume an identifier, returning it as Scylla stores it: lower-cased unless quoted
    fn identifier(&mut self) -> Result<String, String> {
        // read the identifier and step past it
        let name = match self.peek() {
            Some(Token::Word(word)) => word.to_lowercase(),
            Some(Token::Quoted(name)) => name.clone(),
            other => return Err(format!("expected an identifier at {other:?}")),
        };
        self.at += 1;
        Ok(name)
    }

    /// Consume a value: a bind marker or a literal
    fn value(&mut self) -> Result<(), String> {
        // a value is a marker, a literal, or a keyword literal
        let ok = match self.peek() {
            Some(Token::Marker | Token::Text | Token::Number) => true,
            Some(Token::Word(word)) => ["true", "false", "null"]
                .iter()
                .any(|literal| word.eq_ignore_ascii_case(literal)),
            _ => false,
        };
        if !ok {
            return Err(format!("expected a value at {:?}", self.peek()));
        }
        self.at += 1;
        Ok(())
    }

    /// Consume a keyspace-qualified or bare object name, returning the object's name
    fn object(&mut self) -> Result<String, String> {
        // the last identifier of `keyspace.object` names the object
        let first = self.identifier()?;
        if self.eat_symbol(".") {
            self.identifier()
        } else {
            Ok(first)
        }
    }

    /// Consume a parenthesized, comma-separated list of identifiers
    fn identifier_list(&mut self) -> Result<Vec<String>, String> {
        // read identifiers until the closing parenthesis
        self.expect_symbol("(")?;
        let mut names = vec![self.identifier()?];
        while self.eat_symbol(",") {
            names.push(self.identifier()?);
        }
        self.expect_symbol(")")?;
        Ok(names)
    }

    /// Consume a parenthesized, comma-separated list of values
    fn value_list(&mut self) -> Result<usize, String> {
        // read values until the closing parenthesis
        self.expect_symbol("(")?;
        self.value()?;
        let mut count = 1;
        while self.eat_symbol(",") {
            self.value()?;
            count += 1;
        }
        self.expect_symbol(")")?;
        Ok(count)
    }

    /// Consume a `SELECT` list, returning the columns it reads
    fn select_list(&mut self) -> Result<Vec<String>, String> {
        // read comma-separated items until FROM
        let mut columns = Vec::new();
        loop {
            // an item is a column or a function call with an optional alias
            let name = self.identifier()?;
            if self.eat_symbol("(") {
                // a function's arguments are columns, except the `*` of `count(*)`
                if !self.eat_symbol("*") {
                    columns.push(self.identifier()?);
                    while self.eat_symbol(",") {
                        columns.push(self.identifier()?);
                    }
                }
                self.expect_symbol(")")?;
                if self.eat_keyword("AS") {
                    self.identifier()?;
                }
            } else {
                columns.push(name);
            }
            if !self.eat_symbol(",") {
                break;
            }
        }
        Ok(columns)
    }

    /// Consume an optional `USING TTL` clause
    fn using_ttl(&mut self) -> Result<(), String> {
        // a TTL is a marker or a literal
        if self.eat_keyword("USING") {
            self.expect_keyword("TTL")?;
            self.value()?;
        }
        Ok(())
    }

    /// Consume a `SET` clause's assignments
    fn assignments(&mut self) -> Result<Vec<Assignment>, String> {
        // read comma-separated assignments until WHERE
        let mut assignments = Vec::new();
        loop {
            // the column, with an element selector for one entry of a collection
            let column = self.identifier()?;
            let element = self.eat_symbol("[");
            if element {
                self.value()?;
                self.expect_symbol("]")?;
            }
            self.expect_symbol("=")?;
            // the new value, or an expression reading the column itself
            let reads_itself = match self.peek() {
                Some(Token::Word(_) | Token::Quoted(_))
                    if !self.at_keyword("true")
                        && !self.at_keyword("false")
                        && !self.at_keyword("null") =>
                {
                    let read = self.identifier()?;
                    if read != column {
                        return Err(format!("`{column}` is set from another column, `{read}`"));
                    }
                    if !self.eat_symbol("+") && !self.eat_symbol("-") {
                        return Err(format!("expected `+` or `-` after `{read}`"));
                    }
                    self.value()?;
                    true
                }
                _ => {
                    self.value()?;
                    if self.eat_symbol("+") {
                        let read = self.identifier()?;
                        if read != column {
                            return Err(format!("`{column}` is set from another column, `{read}`"));
                        }
                        true
                    } else {
                        false
                    }
                }
            };
            assignments.push(Assignment {
                column,
                element,
                reads_itself,
            });
            if !self.eat_symbol(",") {
                break;
            }
        }
        Ok(assignments)
    }

    /// Consume a `WHERE` clause's comparisons
    fn predicates(&mut self) -> Result<Vec<Predicate>, String> {
        // read comparisons joined by AND
        let mut predicates = Vec::new();
        loop {
            // a comparison of a token or of one column
            if self.at_keyword("token")
                && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("(")))
            {
                self.at += 1;
                let columns = self.identifier_list()?;
                let op = self.operator()?;
                if op == "IN" {
                    return Err("a token compared with IN".to_owned());
                }
                self.value()?;
                predicates.push(Predicate::Token { columns });
            } else {
                let column = self.identifier()?;
                let op = self.operator()?;
                self.value()?;
                predicates.push(Predicate::Column { column, op });
            }
            if !self.eat_keyword("AND") {
                break;
            }
        }
        Ok(predicates)
    }

    /// Consume a comparison operator
    fn operator(&mut self) -> Result<&'static str, String> {
        // IN is a keyword, the rest are symbols
        if self.eat_keyword("IN") {
            return Ok("IN");
        }
        for op in ["=", "<=", ">=", "<", ">"] {
            if self.eat_symbol(op) {
                return Ok(op);
            }
        }
        Err(format!("expected a comparison at {:?}", self.peek()))
    }

    /// Consume the clauses that may follow a `SELECT`'s `WHERE` clause
    ///
    /// # Arguments
    ///
    /// * `parsed` - The statement parsed so far
    fn select_tail(&mut self, parsed: &mut Parsed) -> Result<(), String> {
        // GROUP BY comes first, then ORDER BY, then the limits
        if self.eat_keyword("GROUP") {
            self.expect_keyword("BY")?;
            parsed.group_by.push(self.identifier()?);
            while self.eat_symbol(",") {
                parsed.group_by.push(self.identifier()?);
            }
        }
        // ORDER BY lists columns, each ascending unless marked DESC
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let column = self.identifier()?;
                let descending = if self.eat_keyword("DESC") {
                    true
                } else {
                    self.eat_keyword("ASC");
                    false
                };
                parsed.order_by.push((column, descending));
                if !self.eat_symbol(",") {
                    break;
                }
            }
        }
        // a per-partition limit, then an overall limit
        if self.eat_keyword("PER") {
            self.expect_keyword("PARTITION")?;
            self.expect_keyword("LIMIT")?;
            self.value()?;
        }
        if self.eat_keyword("LIMIT") {
            self.value()?;
        }
        Ok(())
    }
}

/// Parse the parts of a statement validation checks
///
/// # Arguments
///
/// * `tokens` - The statement's tokens
fn parse(tokens: &[Token]) -> Result<Parsed, String> {
    // read the statement verb by verb
    let mut parser = Parser { tokens, at: 0 };
    let mut parsed = Parsed::default();
    if parser.eat_keyword("SELECT") {
        // SELECT [DISTINCT] <items> FROM <object> [WHERE …] [GROUP BY …] [ORDER BY …] [limits]
        parsed.verb = "SELECT";
        parser.eat_keyword("DISTINCT");
        parsed.columns = parser.select_list()?;
        parser.expect_keyword("FROM")?;
        parsed.target = parser.object()?;
        if parser.eat_keyword("WHERE") {
            parsed.restricted = true;
            parsed.predicates = parser.predicates()?;
        }
        parser.select_tail(&mut parsed)?;
    } else if parser.eat_keyword("INSERT") {
        // INSERT INTO <object> (<columns>) VALUES (<values>) [USING TTL …]
        parsed.verb = "INSERT";
        parser.expect_keyword("INTO")?;
        parsed.target = parser.object()?;
        parsed.columns = parser.identifier_list()?;
        parser.expect_keyword("VALUES")?;
        if parser.value_list()? != parsed.columns.len() {
            return Err("the insert's values don't match its columns".to_owned());
        }
        parser.using_ttl()?;
    } else if parser.eat_keyword("UPDATE") {
        // UPDATE <object> [USING TTL …] SET <assignments> WHERE …
        parsed.verb = "UPDATE";
        parsed.target = parser.object()?;
        parser.using_ttl()?;
        parser.expect_keyword("SET")?;
        parsed.assignments = parser.assignments()?;
        parser.expect_keyword("WHERE")?;
        parsed.restricted = true;
        parsed.predicates = parser.predicates()?;
    } else if parser.eat_keyword("DELETE") {
        // DELETE [<columns>] FROM <object> WHERE …
        parsed.verb = "DELETE";
        if !parser.at_keyword("FROM") {
            parsed.columns.push(parser.identifier()?);
            while parser.eat_symbol(",") {
                parsed.columns.push(parser.identifier()?);
            }
        }
        parser.expect_keyword("FROM")?;
        parsed.target = parser.object()?;
        parser.expect_keyword("WHERE")?;
        parsed.restricted = true;
        parsed.predicates = parser.predicates()?;
    } else {
        return Err(format!("unknown statement verb at {:?}", parser.peek()));
    }
    // nothing may follow the clauses the verb allows
    match parser.peek() {
        None => Ok(parsed),
        Some(token) => Err(format!("unexpected {token:?} after the statement")),
    }
}

/// Find a column of an object by name
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `name` - The column's name
fn column<'a>(object: &'a ObjectDef, name: &str) -> Option<&'a ColumnDef> {
    // names are compared as Scylla stores them
    object.columns().iter().find(|column| column.name == name)
}

/// The order a statement reads a clustering column in: its declared order, reversed when the
/// statement's `ORDER BY` reverses the clustering order
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `parsed` - The parsed statement
/// * `declared` - The column's declared order
fn read_order(object: &ObjectDef, parsed: &Parsed, declared: ClusterOrder) -> ClusterOrder {
    // an ORDER BY reverses every clustering column when its first column reads against its
    // declared order
    let reversed = parsed
        .order_by
        .first()
        .and_then(|(name, descending)| {
            column(object, name).map(|column| match column.role {
                ColumnRole::Cluster(_, order) => (order == ClusterOrder::Desc) != *descending,
                _ => false,
            })
        })
        .unwrap_or(false);
    match (declared, reversed) {
        (order, false) => order,
        (ClusterOrder::Asc, true) => ClusterOrder::Desc,
        (ClusterOrder::Desc, true) => ClusterOrder::Asc,
    }
}

/// Check every column a statement names exists in its object
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `parsed` - The parsed statement
fn check_columns(object: &ObjectDef, parsed: &Parsed) -> Result<(), (Rule, String)> {
    // gather every name the statement uses as a column
    let assigned = parsed.assignments.iter().map(|set| set.column.as_str());
    let restricted = parsed
        .predicates
        .iter()
        .flat_map(|predicate| match predicate {
            Predicate::Column { column, .. } => vec![column.as_str()],
            Predicate::Token { columns } => columns.iter().map(String::as_str).collect(),
        });
    let grouped = parsed.group_by.iter().map(String::as_str);
    let ordered = parsed.order_by.iter().map(|(name, _)| name.as_str());
    let mut names = parsed
        .columns
        .iter()
        .map(String::as_str)
        .chain(assigned)
        .chain(restricted)
        .chain(grouped)
        .chain(ordered);
    // the first name the object lacks breaks the rule
    match names.find(|name| column(object, name).is_none()) {
        Some(name) => Err((
            Rule::UnknownColumn,
            format!("{} has no column `{name}`", object.name()),
        )),
        None => Ok(()),
    }
}

/// Check an `UPDATE`'s assignments neither update a counter nor misuse an element selector
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `parsed` - The parsed statement
fn check_assignments(object: &ObjectDef, parsed: &Parsed) -> Result<(), (Rule, String)> {
    // check each assignment against its column's declaration
    for set in &parsed.assignments {
        // the column exists, since the columns were checked first, so a missing one is skipped
        let Some(def) = column(object, &set.column) else {
            continue;
        };
        // a value reading the column itself is a counter update unless the column is a
        // collection being appended to or removed from
        if set.reads_itself && !def.desc.is_collection() {
            return Err((
                Rule::Counter,
                format!("`{}` is updated from its own value", set.column),
            ));
        }
        // only a collection has elements to assign
        if set.element && !def.desc.is_collection() {
            return Err((
                Rule::Syntax,
                format!("`{}` isn't a collection, so it has no elements", set.column),
            ));
        }
    }
    Ok(())
}

/// Check a statement's `WHERE` clause restricts the whole partition key and only key columns,
/// with ranges only on clustering columns in the order the statement reads them
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `parsed` - The parsed statement
fn check_restrictions(object: &ObjectDef, parsed: &Parsed) -> Result<(), (Rule, String)> {
    // an insert has no WHERE clause to check
    if parsed.verb == "INSERT" {
        return Ok(());
    }
    // the partition key's columns, in key order
    let partition = object.partition_key();
    // a token comparison must cover exactly the partition key, in key order
    let mut by_token = false;
    for predicate in &parsed.predicates {
        if let Predicate::Token { columns } = predicate {
            if parsed.verb != "SELECT" || *columns != partition {
                return Err((
                    Rule::PartitionKey,
                    format!("token({}) isn't the partition key", columns.join(", ")),
                ));
            }
            by_token = true;
        }
    }
    // each column comparison must be on a key column, with ranges only on clustering columns
    for predicate in &parsed.predicates {
        let Predicate::Column { column: name, op } = predicate else {
            continue;
        };
        let Some(def) = column(object, name) else {
            continue;
        };
        let range = !matches!(*op, "=" | "IN");
        match def.role {
            ColumnRole::Partition(_) if range => {
                return Err((
                    Rule::RangeColumn,
                    format!("partition column `{name}` is compared with {op}"),
                ));
            }
            ColumnRole::Static | ColumnRole::Regular => {
                return Err((
                    Rule::NonKeyRestriction,
                    format!("`{name}` isn't a key column"),
                ));
            }
            ColumnRole::Cluster(_, declared) if range => {
                // a one-sided range resumes a read, so it must point the way rows are read
                let both_sides = parsed.predicates.iter().any(|other| {
                    matches!(other, Predicate::Column { column, op: other_op }
                        if column == name && is_lower(other_op) != is_lower(op)
                            && !matches!(*other_op, "=" | "IN"))
                });
                let order = read_order(object, parsed, declared);
                let points_down = is_lower(op);
                if !both_sides && points_down != (order == ClusterOrder::Desc) {
                    return Err((
                        Rule::ClusteringOrder,
                        format!(
                            "`{name} {op} ?` reads against its {} read order",
                            if order == ClusterOrder::Desc {
                                "descending"
                            } else {
                                "ascending"
                            }
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
    // without a token range, every partition column needs an equality or IN restriction
    if !by_token {
        let missing = partition
            .iter()
            .filter(|key| {
                !parsed.predicates.iter().any(|predicate| {
                    matches!(predicate, Predicate::Column { column, op }
                        if column == *key && matches!(*op, "=" | "IN"))
                })
            })
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            let detail = if parsed.restricted {
                format!("missing {}", missing.join(", "))
            } else {
                format!("no WHERE clause; missing {}", missing.join(", "))
            };
            return Err((Rule::PartitionKey, detail));
        }
    }
    Ok(())
}

/// Whether a range operator bounds a column from above, so rows are read downward from it
///
/// # Arguments
///
/// * `op` - The comparison operator
fn is_lower(op: &str) -> bool {
    // `<` and `<=` keep the rows below the bound
    matches!(op, "<" | "<=")
}

/// Validate one registered statement against the declared objects
///
/// # Arguments
///
/// * `statement` - The registered statement
/// * `objects` - Every declared object
pub fn validate_statement(
    statement: &RegisteredStatement,
    objects: &[ObjectDef],
) -> Result<(), StatementError> {
    // name the statement and the rule in any error
    let fail = |rule: Rule, detail: String| StatementError {
        statement: statement.name.clone(),
        rule,
        detail,
    };
    let tokens = tokenize(&statement.cql).map_err(|detail| fail(Rule::Syntax, detail))?;
    // a conditional statement is rejected before anything else is read from it
    if tokens
        .iter()
        .any(|token| matches!(token, Token::Word(word) if word.eq_ignore_ascii_case("IF")))
    {
        return Err(fail(
            Rule::Conditional,
            "a tablet keyspace runs no IF, IF EXISTS, or IF NOT EXISTS".to_owned(),
        ));
    }
    let parsed = parse(&tokens).map_err(|detail| fail(Rule::Syntax, detail))?;
    // the statement targets the declared object it's registered with
    let object = objects
        .iter()
        .find(|object| object.name() == parsed.target)
        .ok_or_else(|| {
            fail(
                Rule::UnknownObject,
                format!("`{}` isn't declared", parsed.target),
            )
        })?;
    if parsed.target != statement.object {
        return Err(fail(
            Rule::WrongObject,
            format!(
                "registered with `{}` but targets `{}`",
                statement.object, parsed.target
            ),
        ));
    }
    // then its columns, assignments, and restrictions follow the object's declaration
    check_columns(object, &parsed)
        .and_then(|()| check_assignments(object, &parsed))
        .and_then(|()| check_restrictions(object, &parsed))
        .map_err(|(rule, detail)| fail(rule, detail))
}

/// Validate every statement in a registry, returning each one that breaks a rule
///
/// # Arguments
///
/// * `statements` - The registered statements
/// * `objects` - Every declared object
#[must_use]
pub fn validate_statements(
    statements: &[RegisteredStatement],
    objects: &[ObjectDef],
) -> Vec<StatementError> {
    // keep the first broken rule of each statement
    statements
        .iter()
        .filter_map(|statement| validate_statement(statement, objects).err())
        .collect()
}
