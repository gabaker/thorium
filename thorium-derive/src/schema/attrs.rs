//! The attributes the schema derives parse, with `darling`

// darling's derived parsers end their loops with a `continue` clippy calls redundant
#![allow(clippy::needless_continue)]

use darling::util::Override;
use darling::{FromDeriveInput, FromField, FromMeta};

/// The `#[thorium(...)]` attribute on a table, view, or projection field
// each flag is one independent attribute key
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, FromField)]
#[darling(attributes(thorium), forward_attrs(doc))]
pub struct FieldAttrs {
    /// The field's name
    pub ident: Option<syn::Ident>,
    /// The field's Rust type
    pub ty: syn::Type,
    /// The field's doc comments, kept for the generated docs
    pub attrs: Vec<syn::Attribute>,
    /// A partition key column, in field order
    #[darling(default)]
    pub partition: bool,
    /// A clustering column; a bare `cluster` is ascending
    #[darling(default)]
    pub cluster: Option<Override<ClusterArg>>,
    /// A static column
    #[darling(default, rename = "static")]
    pub is_static: bool,
    /// The CQL column name when it differs from the field name
    #[darling(default)]
    pub column: Option<String>,
    /// The CQL type an unsigned field (or a collection of them) is stored as, such as `"bigint"`
    #[darling(default)]
    pub unsigned: Option<String>,
    /// The year partition column of a time-bucketed table
    #[darling(default)]
    pub year: bool,
    /// The bucket partition column of a time-bucketed table, naming its partition-size setting
    #[darling(default)]
    pub bucket: Option<BucketArg>,
    /// The clustering column a time-bucket cursor ranges over
    #[darling(default)]
    pub timestamp: bool,
    /// On a projection field, the expression it reads, such as `count(*)`
    #[darling(default)]
    pub expr: Option<String>,
    /// A lossy downgrade must fill this column restrictively
    #[darling(default)]
    pub security_sensitive: bool,
    /// Every generated `Debug` prints `<redacted>` for this field
    #[darling(default)]
    pub redact: bool,
}

/// The order given to `#[thorium(cluster = "...")]`
#[derive(Debug, Clone, Copy, FromMeta)]
#[darling(rename_all = "lowercase")]
pub enum ClusterArg {
    /// `"asc"`
    Asc,
    /// `"desc"`
    Desc,
}

/// The arguments of `#[thorium(bucket(...))]`
#[derive(Debug, FromMeta)]
pub struct BucketArg {
    /// The config setting giving this table's partition size, or for the tags tables an
    /// expression on the row's tag type
    pub partition_size: String,
}

/// The `#[thorium_table(...)]` attribute and the struct it's on
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(thorium_table), supports(struct_named))]
pub struct TableAttrs {
    /// The struct's name
    pub ident: syn::Ident,
    /// The struct's generics
    pub generics: syn::Generics,
    /// The struct's fields
    pub data: darling::ast::Data<(), FieldAttrs>,
    /// The CQL table name
    pub name: String,
    /// The config path the table's default TTL is read from
    #[darling(default)]
    pub ttl: Option<String>,
    /// An override of the `::thorium` path root generated code uses
    #[darling(default, rename = "crate")]
    pub krate: Option<syn::Path>,
}

/// The `#[thorium_view(...)]` attribute and the struct it's on
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(thorium_view), supports(struct_named))]
pub struct ViewAttrs {
    /// The struct's name
    pub ident: syn::Ident,
    /// The struct's generics
    pub generics: syn::Generics,
    /// The struct's fields
    pub data: darling::ast::Data<(), FieldAttrs>,
    /// The CQL view name
    pub name: String,
    /// The base table's struct
    pub base: syn::Path,
    /// A condition on one key column that replaces its generated `IS NOT NULL`
    #[darling(default)]
    pub filter: Option<String>,
    /// An override of the `::thorium` path root generated code uses
    #[darling(default, rename = "crate")]
    pub krate: Option<syn::Path>,
}

/// The objects a projection reads from: `of = Object` or `of(First, Second)`
#[derive(Debug, Default)]
pub struct OfList(pub Vec<syn::Path>);

impl FromMeta for OfList {
    /// Parse `of = Object`
    ///
    /// # Arguments
    ///
    /// * `expr` - The value given to `of`
    fn from_expr(expr: &syn::Expr) -> darling::Result<Self> {
        match expr {
            syn::Expr::Path(path) => Ok(OfList(vec![path.path.clone()])),
            _ => Err(darling::Error::unexpected_expr_type(expr)),
        }
    }

    /// Parse `of(First, Second)`
    ///
    /// # Arguments
    ///
    /// * `items` - The objects listed
    fn from_list(items: &[darling::ast::NestedMeta]) -> darling::Result<Self> {
        let mut paths = Vec::with_capacity(items.len());
        // every listed object must be a plain path
        for item in items {
            match item {
                darling::ast::NestedMeta::Meta(syn::Meta::Path(path)) => paths.push(path.clone()),
                other => {
                    return Err(darling::Error::unsupported_format("non-path").with_span(other));
                }
            }
        }
        Ok(OfList(paths))
    }
}

/// The `#[thorium_projection(...)]` attribute and the struct it's on
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(thorium_projection), supports(struct_named))]
pub struct ProjectionAttrs {
    /// The struct's name
    pub ident: syn::Ident,
    /// The struct's generics
    pub generics: syn::Generics,
    /// The struct's fields
    pub data: darling::ast::Data<(), FieldAttrs>,
    /// The tables or views whose columns this row reads
    pub of: OfList,
    /// An override of the `::thorium` path root generated code uses
    #[darling(default, rename = "crate")]
    pub krate: Option<syn::Path>,
}

/// The arguments of `#[thorium::keyspace(...)]`
#[derive(Debug, Default, FromMeta)]
pub struct KeyspaceArgs {
    /// An override of the `::thorium` path root generated code uses
    #[darling(default, rename = "crate")]
    pub krate: Option<syn::Path>,
}

/// The arguments of `#[thorium::table_queries(table = ...)]` or
/// `#[thorium::view_queries(view = ...)]`
#[derive(Debug, FromMeta)]
pub struct QueriesAttrs {
    /// The table struct, on `table_queries`
    #[darling(default)]
    pub table: Option<syn::Path>,
    /// The view struct, on `view_queries`
    #[darling(default)]
    pub view: Option<syn::Path>,
    /// An override of the `::thorium` path root generated code uses
    #[darling(default, rename = "crate")]
    pub krate: Option<syn::Path>,
}

/// The columns an insert writes
#[derive(Debug)]
pub enum ColumnsArg {
    /// Every column (`columns = all`)
    All,
    /// A subset (`columns = [a, b, c]`), naming each column by its field
    List(Vec<syn::Ident>),
}

impl FromMeta for ColumnsArg {
    /// Parse `columns = all` or `columns = [a, b, c]`
    ///
    /// # Arguments
    ///
    /// * `expr` - The value given to `columns`
    fn from_expr(expr: &syn::Expr) -> darling::Result<Self> {
        match expr {
            syn::Expr::Path(path) if path.path.is_ident("all") => Ok(ColumnsArg::All),
            syn::Expr::Array(array) => {
                // every listed column must be a plain field name
                let mut fields = Vec::with_capacity(array.elems.len());
                for elem in &array.elems {
                    match elem {
                        syn::Expr::Path(path) if path.path.get_ident().is_some() => {
                            fields.extend(path.path.get_ident().cloned());
                        }
                        _ => return Err(darling::Error::unexpected_expr_type(elem)),
                    }
                }
                Ok(ColumnsArg::List(fields))
            }
            _ => Err(darling::Error::unexpected_expr_type(expr)),
        }
    }
}

/// The arguments of an `#[insert(...)]` field
#[derive(Debug, FromMeta)]
pub struct InsertArgs {
    /// `all`, or the list of columns for a subset insert
    pub columns: ColumnsArg,
    /// How the insert sets the row's TTL: `none` leaves the table's default TTL in effect
    pub ttl: syn::Ident,
}
