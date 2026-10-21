//! `#[derive(ScyllaTable)]`: a base table's definition, borrowed row, and full-row statements

use darling::FromDeriveInput;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use super::attrs::TableAttrs;
use super::model::{self, ColumnModel, Kind};
use super::paths::Paths;
use super::types::{self, Borrow};

/// Emit the borrowed row, its `as_ref`, and by-name serialization for it and the owned row
///
/// # Arguments
///
/// * `ident` - The table struct
/// * `table` - The table's CQL name
/// * `columns` - The table's columns
/// * `paths` - The generated code's paths
fn borrowed_row(
    ident: &syn::Ident,
    table: &str,
    columns: &[ColumnModel],
    paths: &Paths,
) -> TokenStream {
    let schema = &paths.schema;
    let private = &paths.private;
    let row = format_ident!("{}Ref", ident);
    let row_doc = model::doc(&format!(
        "A borrowed `{table}` row, bound by name, so one row is written to many groups without \
         cloning"
    ));
    let mut fields = Vec::with_capacity(columns.len());
    let mut borrows = Vec::with_capacity(columns.len());
    let mut arms = Vec::with_capacity(columns.len());
    let mut borrows_anything = false;
    for column in columns {
        let field = &column.field;
        let docs = &column.docs;
        let (ty, borrow) = types::borrowed(&column.ty, schema);
        // build the borrowed field from the owned one
        let value = match borrow {
            Borrow::Reference => quote! { &self.#field },
            Borrow::AsDeref => quote! { self.#field.as_deref() },
            Borrow::AsRef => quote! { self.#field.as_ref() },
            Borrow::Copy => quote! { self.#field },
            Borrow::JsonText => quote! { #schema::JsonText(&self.#field.0) },
        };
        borrows_anything |= !matches!(borrow, Borrow::Copy);
        fields.push(quote! {
            #(#docs)*
            pub #field: #ty
        });
        borrows.push(quote! { #field: #value });
        // write this field when its column is bound, through its checked conversion if unsigned
        let name = &column.column;
        let write = match column.unsigned {
            Some(width) => {
                let marker = width.marker(paths);
                quote! {
                    <_ as #schema::UnsignedField<#marker>>::serialize_checked(&self.#field, #table, #name, typ, cell)
                }
            }
            None => quote! {
                #private::scylla::serialize::value::SerializeValue::serialize(&self.#field, typ, cell)
            },
        };
        arms.push(quote! { #name => Some(#write), });
    }
    // a row of copied scalars only still carries its lifetime
    if !borrows_anything {
        fields.push(quote! {
            #[doc(hidden)]
            pub __lifetime: ::core::marker::PhantomData<&'a ()>
        });
        borrows.push(quote! { __lifetime: ::core::marker::PhantomData });
    }
    let debug = model::debug_impl(&quote! { #row<'_> }, &row.to_string(), columns);
    let serialize = serialize_rows(ident, &row, table, &arms, paths);
    quote! {
        #row_doc
        #[derive(Clone, Copy)]
        pub struct #row<'a> {
            #(#fields,)*
        }
        #debug
        impl #ident {
            /// Borrow every field of this row for binding, without allocating
            pub fn as_ref(&self) -> #row<'_> {
                #row {
                    #(#borrows,)*
                }
            }
        }
        #serialize
    }
}

/// Emit the by-name `SerializeRow` impls of the borrowed row and the owned row
///
/// # Arguments
///
/// * `ident` - The table struct
/// * `row` - The borrowed row struct
/// * `table` - The table's CQL name
/// * `arms` - The match arms writing each column by name
/// * `paths` - The generated code's paths
fn serialize_rows(
    ident: &syn::Ident,
    row: &syn::Ident,
    table: &str,
    arms: &[TokenStream],
    paths: &Paths,
) -> TokenStream {
    let schema = &paths.schema;
    let private = &paths.private;
    quote! {
        impl #row<'_> {
            /// Write one bind column from the field with that column's name, or return `None`
            /// when this row has no such column, so a wrapper row can answer other bind markers
            ///
            /// # Arguments
            ///
            /// * `name` - The bind column's name, as the prepared statement reports it
            /// * `typ` - The bind column's CQL type
            /// * `cell` - The writer for this column's value
            pub fn serialize_column<'b>(
                &self,
                name: &str,
                typ: &#private::scylla::frame::response::result::ColumnType,
                cell: #private::scylla::serialize::writers::CellWriter<'b>,
            ) -> Option<Result<#private::scylla::serialize::writers::WrittenCellProof<'b>, #private::scylla::serialize::SerializationError>> {
                match name {
                    #(#arms)*
                    _ => None,
                }
            }
        }
        impl #schema::BindColumns for #row<'_> {
            /// The table this borrowed row belongs to
            const TABLE: &'static str = #table;
            /// Writes one bind column through the row's own by-name column writer
            ///
            /// # Arguments
            ///
            /// * `name` - The bind column's name, as the prepared statement reports it
            /// * `typ` - The bind column's CQL type
            /// * `cell` - The writer for this column's value
            fn serialize_column<'b>(
                &self,
                name: &str,
                typ: &#private::scylla::frame::response::result::ColumnType,
                cell: #private::scylla::serialize::writers::CellWriter<'b>,
            ) -> Option<Result<#private::scylla::serialize::writers::WrittenCellProof<'b>, #private::scylla::serialize::SerializationError>> {
                // write through the row's own by-name column writer
                #row::serialize_column(self, name, typ, cell)
            }
        }
        impl #private::scylla::serialize::row::SerializeRow for #row<'_> {
            /// Writes each bind column in the statement's order, by name, refusing a column the row
            /// lacks
            ///
            /// # Arguments
            ///
            /// * `ctx` - The statement's bind column specs
            /// * `writer` - The writer for the row's values
            fn serialize(
                &self,
                ctx: &#private::scylla::serialize::row::RowSerializationContext<'_>,
                writer: &mut #private::scylla::serialize::writers::RowWriter,
            ) -> Result<(), #private::scylla::serialize::SerializationError> {
                // write each bind column in the statement's order, by name
                for spec in ctx.columns() {
                    let cell = writer.make_cell_writer();
                    match self.serialize_column(spec.name(), spec.typ(), cell) {
                        Some(written) => {
                            written?;
                        }
                        None => return Err(#private::unknown_bind_column(#table, spec.name())),
                    }
                }
                Ok(())
            }
            /// A borrowed row always binds columns, so it is never empty
            fn is_empty(&self) -> bool {
                false
            }
        }
        impl #private::scylla::serialize::row::SerializeRow for #ident {
            /// Binds the borrowed form of this row, so there is one binding body per table
            ///
            /// # Arguments
            ///
            /// * `ctx` - The statement's bind column specs
            /// * `writer` - The writer for the row's values
            fn serialize(
                &self,
                ctx: &#private::scylla::serialize::row::RowSerializationContext<'_>,
                writer: &mut #private::scylla::serialize::writers::RowWriter,
            ) -> Result<(), #private::scylla::serialize::SerializationError> {
                // bind the borrowed form of this row, so there is one binding body per table
                #private::scylla::serialize::row::SerializeRow::serialize(&self.as_ref(), ctx, writer)
            }
            /// A row always binds columns, so it is never empty
            fn is_empty(&self) -> bool {
                false
            }
        }
    }
}

/// Emit the table's full-row statements: insert, select-all, token-range scan, and census
///
/// # Arguments
///
/// * `ident` - The table struct
/// * `table` - The table's CQL name
/// * `columns` - The table's columns
/// * `paths` - The generated code's paths
fn level_one_statements(
    ident: &syn::Ident,
    table: &str,
    columns: &[ColumnModel],
    paths: &Paths,
) -> TokenStream {
    let schema = &paths.schema;
    // every column in declaration order, and the partition key in key order
    let names = columns
        .iter()
        .map(|column| column.column.as_str())
        .collect::<Vec<_>>();
    let partition = columns
        .iter()
        .filter(|column| matches!(column.role, model::Role::Partition(_)))
        .map(|column| column.column.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let list = names.join(", ");
    let binds = vec!["?"; names.len()].join(", ");
    let range = format!("token({partition}) >= ? AND token({partition}) <= ?");
    let insert = format!("INSERT INTO {{ns}}.{table} ({list}) VALUES ({binds})");
    let select_all = format!("SELECT {list} FROM {{ns}}.{table}");
    let scan = format!("SELECT {list} FROM {{ns}}.{table} WHERE {range}");
    let census = format!(
        "SELECT {partition}, count(*) AS count FROM {{ns}}.{table} WHERE {range} GROUP BY {partition}"
    );
    let row = format_ident!("{}Ref", ident);
    quote! {
        impl #schema::TableStatements for #ident {
            /// The table's borrowed row
            type Ref<'a> = #row<'a>;
            /// The table's full-row insert
            const INSERT: &'static str = #insert;
            /// The table's select of every column of every row
            const SELECT_ALL: &'static str = #select_all;
            /// The table's select of every column in a token range
            const SCAN: &'static str = #scan;
            /// The table's per-partition row count in a token range
            const CENSUS: &'static str = #census;
        }
    }
}

/// Expand `#[derive(ScyllaTable)]`
///
/// # Arguments
///
/// * `ast` - The struct the derive is on
pub fn derive(ast: &syn::DeriveInput) -> TokenStream {
    match expand(ast) {
        Ok(tokens) => tokens,
        Err(errors) => errors.write_errors(),
    }
}

/// Build the table's generated items
///
/// # Arguments
///
/// * `ast` - The struct the derive is on
fn expand(ast: &syn::DeriveInput) -> darling::Result<TokenStream> {
    let attrs = TableAttrs::from_derive_input(ast)?;
    model::no_generics(&attrs.generics)?;
    let paths = Paths::new(attrs.krate.as_ref());
    let schema = &paths.schema;
    let ident = &attrs.ident;
    let table = &attrs.name;
    let fields = attrs.data.take_struct().expect("only named structs").fields;
    let columns = model::columns(Kind::Table, table, ident, fields)?;
    // build the table's definition from its columns
    let defs = columns.iter().map(|column| column.column_def(&paths));
    let ttl = match &attrs.ttl {
        Some(path) => quote! { Some(#schema::TtlSource { config_path: #path }) },
        None => quote! { None },
    };
    let partition_size = match columns
        .iter()
        .find_map(|column| column.partition_size.as_ref())
    {
        Some(setting) => quote! { Some(#setting) },
        None => quote! { None },
    };
    let row = borrowed_row(ident, table, &columns, &paths);
    let statements = level_one_statements(ident, table, &columns, &paths);
    // a table with a redacted field generates its own Debug, so it can't also derive one
    let owned_debug = if columns.iter().any(|column| column.redact) {
        model::debug_impl(&quote! { #ident }, &ident.to_string(), &columns)
    } else {
        TokenStream::new()
    };
    let redacted = model::redacted_names(&columns);
    let fields = model::field_names(&columns);
    let column_at = model::column_at_impls(ident, table, &columns, &paths);
    Ok(quote! {
        impl #schema::ScyllaObject for #ident {
            /// The table's CQL name
            const NAME: &'static str = #table;
            /// The table's definition: its columns, TTL, and partition size
            const DEF: #schema::ObjectDef = #schema::ObjectDef::Table(#schema::TableDef {
                name: #table,
                columns: &[#(#defs),*],
                ttl: #ttl,
                partition_size: #partition_size,
            });
            /// The table's redacted columns
            const REDACTED: &'static [&'static str] = #redacted;
            /// The Rust field name of each of the table's columns, in declaration order
            const FIELDS: &'static [&'static str] = #fields;
        }
        #column_at
        #row
        #statements
        #owned_debug
    })
}
