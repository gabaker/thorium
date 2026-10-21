//! The parsed column model of a table, view, or projection, with the checks each derive can make
//! from its own tokens

use darling::util::Override;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use std::collections::HashSet;

use super::attrs::{ClusterArg, FieldAttrs};
use super::paths::Paths;
use super::types;

/// The CQL integer types an unsigned field can be stored as
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// `tinyint`
    TinyInt,
    /// `smallint`
    SmallInt,
    /// `int`
    Int,
    /// `bigint`
    BigInt,
}

impl Width {
    /// Parse a `#[thorium(unsigned = "...")]` value
    ///
    /// # Arguments
    ///
    /// * `raw` - The value given
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "tinyint" => Some(Width::TinyInt),
            "smallint" => Some(Width::SmallInt),
            "int" => Some(Width::Int),
            "bigint" => Some(Width::BigInt),
            _ => None,
        }
    }

    /// The CQL name of this width
    fn name(self) -> &'static str {
        match self {
            Width::TinyInt => "tinyint",
            Width::SmallInt => "smallint",
            Width::Int => "int",
            Width::BigInt => "bigint",
        }
    }

    /// The narrowest width that holds every value of an unsigned type
    ///
    /// # Arguments
    ///
    /// * `unsigned` - The unsigned type's name
    fn narrowest_for(unsigned: &str) -> Width {
        match unsigned {
            "u8" => Width::SmallInt,
            "u16" => Width::Int,
            _ => Width::BigInt,
        }
    }

    /// The marker type generated code names this width by
    ///
    /// # Arguments
    ///
    /// * `paths` - The generated code's paths
    pub fn marker(self, paths: &Paths) -> TokenStream {
        let schema = &paths.schema;
        match self {
            Width::TinyInt => quote! { #schema::widths::TinyInt },
            Width::SmallInt => quote! { #schema::widths::SmallInt },
            Width::Int => quote! { #schema::widths::Int },
            Width::BigInt => quote! { #schema::widths::BigInt },
        }
    }
}

/// The role a column plays in its object's primary key
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A partition key column at this position
    Partition(u8),
    /// A clustering column at this position, descending when true
    Cluster(u8, bool),
    /// A static column
    Static,
    /// A regular column
    Regular,
}

impl Role {
    /// Whether this column is part of the primary key
    pub fn is_key(self) -> bool {
        matches!(self, Role::Partition(_) | Role::Cluster(..))
    }
}

/// One parsed column
pub struct ColumnModel {
    /// The Rust field
    pub field: syn::Ident,
    /// The CQL column name
    pub column: String,
    /// The field's type
    pub ty: syn::Type,
    /// The column's key role
    pub role: Role,
    /// The width an unsigned field is stored as
    pub unsigned: Option<Width>,
    /// The field's doc attributes
    pub docs: Vec<syn::Attribute>,
    /// Whether a lossy downgrade must fill this column restrictively
    pub security_sensitive: bool,
    /// Whether generated `Debug` impls hide this field
    pub redact: bool,
    /// The aggregate expression a projection field reads
    pub expr: Option<String>,
    /// The partition-size setting a bucket column names
    pub partition_size: Option<String>,
}

impl ColumnModel {
    /// The tokens of this column's CQL type description
    ///
    /// # Arguments
    ///
    /// * `paths` - The generated code's paths
    pub fn desc(&self, paths: &Paths) -> TokenStream {
        let schema = &paths.schema;
        let ty = &self.ty;
        // an unsigned field's type is its declared column width, any other field's its CqlType
        match self.unsigned {
            Some(width) => {
                let marker = width.marker(paths);
                quote! { <#ty as #schema::UnsignedField<#marker>>::DESC }
            }
            None => quote! { <#ty as #schema::CqlType>::DESC },
        }
    }

    /// The tokens of this column's role in a `ColumnDef`
    ///
    /// # Arguments
    ///
    /// * `paths` - The generated code's paths
    pub fn role_tokens(&self, paths: &Paths) -> TokenStream {
        let schema = &paths.schema;
        // name the role, with a clustering column's position and order
        match self.role {
            Role::Partition(position) => quote! { #schema::ColumnRole::Partition(#position) },
            Role::Cluster(position, desc) => {
                let order = if desc {
                    quote! { #schema::ClusterOrder::Desc }
                } else {
                    quote! { #schema::ClusterOrder::Asc }
                };
                quote! { #schema::ColumnRole::Cluster(#position, #order) }
            }
            Role::Static => quote! { #schema::ColumnRole::Static },
            Role::Regular => quote! { #schema::ColumnRole::Regular },
        }
    }

    /// The tokens of this column's `ColumnDef`
    ///
    /// # Arguments
    ///
    /// * `paths` - The generated code's paths
    pub fn column_def(&self, paths: &Paths) -> TokenStream {
        // render each part of the column's definition
        let schema = &paths.schema;
        let name = &self.column;
        let desc = self.desc(paths);
        let role = self.role_tokens(paths);
        let sensitive = self.security_sensitive;
        // build the definition from the column's parts
        quote! {
            #schema::ColumnDef {
                name: #name,
                desc: #desc,
                role: #role,
                security_sensitive: #sensitive,
            }
        }
    }
}

/// Which derive the columns belong to, for the checks that differ between them
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A base table
    Table,
    /// A materialized view
    View,
    /// A projection
    Projection,
}

impl Kind {
    /// The word used for this kind of object in errors
    fn word(self) -> &'static str {
        match self {
            Kind::Table => "table",
            Kind::View => "view",
            Kind::Projection => "projection",
        }
    }
}

/// Check that a struct takes no generic parameters
///
/// # Arguments
///
/// * `generics` - The struct's generics
pub fn no_generics(generics: &syn::Generics) -> darling::Result<()> {
    if generics.params.is_empty() {
        Ok(())
    } else {
        Err(
            darling::Error::custom("schema derives don't support generic structs")
                .with_span(&generics.params),
        )
    }
}

/// Check one field's unsigned annotation against its type
///
/// # Arguments
///
/// * `field` - The field's attributes
/// * `errors` - The errors found so far
fn check_unsigned(field: &FieldAttrs, errors: &mut darling::error::Accumulator) -> Option<Width> {
    let found = types::find_unsigned(&field.ty);
    match (&field.unsigned, found) {
        // an unsigned type with no column type can't be stored
        (None, Some(unsigned)) => {
            errors.push(
                darling::Error::custom(format!(
                    "`{unsigned}` needs #[thorium(unsigned = \"...\")] naming the signed column type it's stored as"
                ))
                .with_span(&field.ty),
            );
            None
        }
        (None, None) => None,
        (Some(raw), found) => {
            // the column type must be one of the integer types
            let Some(width) = Width::parse(raw) else {
                errors.push(
                    darling::Error::custom(format!(
                        "unknown unsigned column type `{raw}`; use tinyint, smallint, int, or bigint"
                    ))
                    .with_span(&field.ty),
                );
                return None;
            };
            // a column narrower than the type's range is declared with a signed type instead
            if let Some(unsigned) = found {
                let narrowest = Width::narrowest_for(&unsigned);
                if (width as u8) < (narrowest as u8) {
                    errors.push(
                        darling::Error::custom(format!(
                            "`{unsigned}` doesn't fit in {}; use {}",
                            width.name(),
                            narrowest.name()
                        ))
                        .with_span(&field.ty),
                    );
                }
            }
            Some(width)
        }
    }
}

/// Parse and check the columns of a table, view, or projection
///
/// # Arguments
///
/// * `kind` - Which derive the columns belong to
/// * `object` - The object's CQL name (or the projection's struct name), for errors
/// * `ident` - The struct's name, for errors without a field
/// * `fields` - The struct's fields
pub fn columns(
    kind: Kind,
    object: &str,
    ident: &syn::Ident,
    fields: Vec<FieldAttrs>,
) -> darling::Result<Vec<ColumnModel>> {
    let mut errors = darling::Error::accumulator();
    let mut columns = Vec::with_capacity(fields.len());
    let mut seen = HashSet::new();
    let (mut partitions, mut clusters) = (0_u8, 0_u8);
    for field in fields {
        let ident = field.ident.clone().expect("named fields have names");
        // the column is named after the field unless renamed
        let column = field.column.clone().unwrap_or_else(|| ident.to_string());
        if !seen.insert(column.clone()) {
            errors.push(
                darling::Error::custom(format!(
                    "duplicate column `{column}` in {} `{object}`",
                    kind.word()
                ))
                .with_span(&ident),
            );
        }
        // only tables and views have keys
        let keyed = field.partition || field.cluster.is_some() || field.is_static;
        if kind == Kind::Projection
            && (keyed || field.year || field.bucket.is_some() || field.timestamp)
        {
            errors.push(
                darling::Error::custom("key attributes are only allowed on tables and views")
                    .with_span(&ident),
            );
        }
        if kind != Kind::Projection && field.expr.is_some() {
            errors.push(
                darling::Error::custom("`expr` is only allowed on projection fields")
                    .with_span(&ident),
            );
        }
        // work out the column's role from its key attributes
        let role = match (field.partition, &field.cluster, field.is_static) {
            (true, None, false) => {
                partitions += 1;
                Role::Partition(partitions - 1)
            }
            (false, Some(order), false) => {
                clusters += 1;
                let desc = matches!(order, Override::Explicit(ClusterArg::Desc));
                Role::Cluster(clusters - 1, desc)
            }
            (false, None, true) => Role::Static,
            (false, None, false) => Role::Regular,
            _ => {
                errors.push(
                    darling::Error::custom(
                        "a column can be only one of partition, cluster, or static",
                    )
                    .with_span(&ident),
                );
                Role::Regular
            }
        };
        // key columns always hold a value
        if role.is_key() && types::option_inner(&field.ty).is_some() {
            errors.push(
                darling::Error::custom(format!(
                    "key column `{column}` of {} `{object}` can't be an Option",
                    kind.word()
                ))
                .with_span(&field.ty),
            );
        }
        let unsigned = check_unsigned(&field, &mut errors);
        columns.push(ColumnModel {
            field: ident,
            column,
            ty: field.ty,
            role,
            unsigned,
            docs: field.attrs,
            security_sensitive: field.security_sensitive,
            redact: field.redact,
            expr: field.expr,
            partition_size: field.bucket.map(|bucket| bucket.partition_size),
        });
    }
    // tables and views need a partition key
    if kind != Kind::Projection && partitions == 0 {
        errors.push(
            darling::Error::custom(format!(
                "{} `{object}` needs at least one #[thorium(partition)] column",
                kind.word()
            ))
            .with_span(ident),
        );
    }
    errors.finish_with(columns)
}

/// The doc comment tokens for generated items, from plain text
///
/// # Arguments
///
/// * `text` - The doc text
pub fn doc(text: &str) -> TokenStream {
    let lit = syn::LitStr::new(text, Span::call_site());
    quote! { #[doc = #lit] }
}

/// The names of the redacted columns, as a slice expression
///
/// # Arguments
///
/// * `columns` - The object's columns
pub fn redacted_names(columns: &[ColumnModel]) -> TokenStream {
    let names = columns
        .iter()
        .filter(|column| column.redact)
        .map(|column| &column.column);
    quote! { &[#(#names),*] }
}

/// Build a generated `Debug` impl that prints `<redacted>` for redacted fields
///
/// # Arguments
///
/// * `target` - The type the impl is for, with any lifetime
/// * `name` - The struct name to print
/// * `columns` - The struct's columns
pub fn debug_impl(target: &TokenStream, name: &str, columns: &[ColumnModel]) -> TokenStream {
    // print each field, or a placeholder for redacted ones
    let fields = columns.iter().map(|column| {
        let field = &column.field;
        let label = field.to_string();
        if column.redact {
            quote! { .field(#label, &::core::format_args!("<redacted>")) }
        } else {
            quote! { .field(#label, &self.#field) }
        }
    });
    quote! {
        impl ::core::fmt::Debug for #target {
            /// Writes the row's fields, showing redacted fields as `<redacted>`
            ///
            /// # Arguments
            ///
            /// * `f` - The formatter to write to
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(#name)
                    #(#fields)*
                    .finish()
            }
        }
    }
}

/// The Rust field names of the columns, as a slice expression
///
/// # Arguments
///
/// * `columns` - The object's columns
pub fn field_names(columns: &[ColumnModel]) -> TokenStream {
    let names = columns.iter().map(|column| column.field.to_string());
    quote! { &[#(#names),*] }
}

/// Emit a `ColumnAt` impl for every column of a table or view, giving each column's CQL name,
/// owned and bind types, and how its binds are written
///
/// # Arguments
///
/// * `ident` - The table or view struct
/// * `object` - The object's CQL name
/// * `columns` - The object's columns
/// * `paths` - The generated code's paths
pub fn column_at_impls(
    ident: &syn::Ident,
    object: &str,
    columns: &[ColumnModel],
    paths: &Paths,
) -> TokenStream {
    let schema = &paths.schema;
    let private = &paths.private;
    let column_type = quote! { #private::scylla::frame::response::result::ColumnType };
    let cell = quote! { #private::scylla::serialize::writers::CellWriter };
    let proof = quote! { #private::scylla::serialize::writers::WrittenCellProof };
    let error = quote! { #private::scylla::serialize::SerializationError };
    let impls = columns.iter().enumerate().map(|(index, column)| {
        let ty = &column.ty;
        let name = &column.column;
        let (bind, _) = types::borrowed(ty, schema);
        // unsigned values are written through their checked conversion, others natively
        let (write_one, write_list) = match column.unsigned {
            Some(width) => {
                let marker = width.marker(paths);
                (
                    quote! {
                        <_ as #schema::UnsignedField<#marker>>::serialize_checked(value, #object, #name, typ, cell)
                    },
                    quote! {
                        <[#ty] as #schema::UnsignedField<#marker>>::serialize_checked(values, #object, #name, typ, cell)
                    },
                )
            }
            None => (
                quote! {
                    #private::scylla::serialize::value::SerializeValue::serialize(value, typ, cell)
                },
                quote! {
                    #private::scylla::serialize::value::SerializeValue::serialize(values, typ, cell)
                },
            ),
        };
        // only key columns can be restricted with an `IN` list or bound by a cursor
        let list = if column.role.is_key() {
            quote! {
                /// Writes a list of this key column's values for an `IN` restriction
                ///
                /// # Arguments
                ///
                /// * `values` - The values to write
                /// * `typ` - The bind marker's CQL type
                /// * `cell` - The writer for the list
                fn serialize_list<'b>(
                    values: &[Self::Owned],
                    typ: &#column_type,
                    cell: #cell<'b>,
                ) -> Result<#proof<'b>, #error> {
                    #write_list
                }
                /// Writes one of this key column's values given as the field's own type
                ///
                /// # Arguments
                ///
                /// * `value` - The value to write
                /// * `typ` - The bind marker's CQL type
                /// * `cell` - The writer for the value
                fn serialize_owned<'b>(
                    value: &Self::Owned,
                    typ: &#column_type,
                    cell: #cell<'b>,
                ) -> Result<#proof<'b>, #error> {
                    #write_one
                }
            }
        } else {
            TokenStream::new()
        };
        quote! {
            impl #schema::ColumnAt<#index> for #ident {
                /// The column's CQL name
                const COLUMN: &'static str = #name;
                /// The field's declared type
                type Owned = #ty;
                /// The field's type in the object's borrowed row
                type Bind<'a> = #bind;
                /// Writes one bound value of this column, through its checked conversion if it's
                /// unsigned
                ///
                /// # Arguments
                ///
                /// * `value` - The value to write
                /// * `typ` - The bind marker's CQL type
                /// * `cell` - The writer for the value
                fn serialize_bind<'b>(
                    value: &Self::Bind<'_>,
                    typ: &#column_type,
                    cell: #cell<'b>,
                ) -> Result<#proof<'b>, #error> {
                    #write_one
                }
                #list
            }
        }
    });
    quote! { #(#impls)* }
}
