//! `#[derive(ScyllaProjection)]`: the columns one query reads, read by name and checked against
//! their objects at compile time

use darling::FromDeriveInput;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use super::attrs::ProjectionAttrs;
use super::model::{self, ColumnModel, Kind};
use super::paths::Paths;

/// Emit the compile-time check of one field against the objects the projection reads
///
/// The column is looked up by name in each object's definition, reached through its
/// `ScyllaObject` impl, so the objects can be named by any path.
///
/// # Arguments
///
/// * `ident` - The projection struct
/// * `column` - The field's column
/// * `of` - The objects the projection reads
/// * `paths` - The generated code's paths
fn column_check(
    ident: &syn::Ident,
    column: &ColumnModel,
    of: &[syn::Path],
    paths: &Paths,
) -> TokenStream {
    let schema = &paths.schema;
    let private = &paths.private;
    let name = &column.column;
    let desc = column.desc(paths);
    let label = format!("projection {ident}.{} -> ", column.field);
    // look the column up in every object by name
    let defs = of
        .iter()
        .map(|object| quote! { &<#object as #schema::ScyllaObject>::DEF });
    // a field reading a column any of its objects redacts must be redacted too
    let redact = column.redact;
    let redacted = of
        .iter()
        .map(|object| quote! { <#object as #schema::ScyllaObject>::REDACTED });
    quote! {
        const _: () = #private::assert_projection_in_any(#desc, &[#(#defs),*], #name, #label);
        const _: () = #private::assert_redacted(#redact, &[#(#redacted),*], #name, #label);
    }
}

/// Emit the by-name `DeserializeRow` impl
///
/// # Arguments
///
/// * `ident` - The projection struct
/// * `columns` - The projection's columns
/// * `source` - The object named in conversion errors
/// * `paths` - The generated code's paths
fn deserialize_row(
    ident: &syn::Ident,
    columns: &[ColumnModel],
    source: &TokenStream,
    paths: &Paths,
) -> TokenStream {
    let schema = &paths.schema;
    let private = &paths.private;
    let struct_name = ident.to_string();
    let mut checks = Vec::with_capacity(columns.len());
    let mut locals = Vec::with_capacity(columns.len());
    let mut arms = Vec::with_capacity(columns.len());
    let mut builds = Vec::with_capacity(columns.len());
    for column in columns {
        let field = &column.field;
        let ty = &column.ty;
        let name = &column.column;
        let local = format_ident!("__{}", field);
        // unsigned fields are read as their signed column type, then converted; a cell the
        // driver can't decode is named by projection and column, as conversion errors already are
        let (stored, read) = match column.unsigned {
            Some(width) => {
                let marker = width.marker(paths);
                let stored = quote! { <#ty as #schema::UnsignedRead<#marker>>::Stored };
                let read = quote! {{
                    let stored = <#stored as #private::scylla::deserialize::value::DeserializeValue<'frame, 'metadata>>::deserialize(column.spec.typ(), column.slice)
                        .map_err(|err| #private::column_value_error(#struct_name, #name, err))?;
                    <#ty as #schema::UnsignedRead<#marker>>::from_stored(stored, #source, #name)
                        .map_err(#private::scylla::deserialize::DeserializationError::new)?
                }};
                (stored, read)
            }
            None => (
                quote! { #ty },
                quote! {
                    <#ty as #private::scylla::deserialize::value::DeserializeValue<'frame, 'metadata>>::deserialize(column.spec.typ(), column.slice)
                        .map_err(|err| #private::column_value_error(#struct_name, #name, err))?
                },
            ),
        };
        checks.push(quote! {
            match specs.iter().find(|spec| spec.name() == #name) {
                Some(spec) => {
                    <#stored as #private::scylla::deserialize::value::DeserializeValue<'frame, 'metadata>>::type_check(spec.typ())
                        .map_err(|err| #private::column_type_error(#struct_name, #name, err))?;
                }
                None => return Err(#private::missing_column(#struct_name, #name)),
            }
        });
        locals.push(quote! { let mut #local: Option<#ty> = None; });
        arms.push(quote! { #name => #local = Some(#read), });
        builds.push(quote! {
            #field: #local.ok_or_else(|| #private::missing_value(#struct_name, #name))?
        });
    }
    quote! {
        impl<'frame, 'metadata> #private::scylla::deserialize::row::DeserializeRow<'frame, 'metadata> for #ident {
            fn type_check(
                specs: &[#private::scylla::frame::response::result::ColumnSpec],
            ) -> Result<(), #private::scylla::deserialize::TypeCheckError> {
                // every field's column must be in the result, with a type the field reads
                #(#checks)*
                Ok(())
            }
            fn deserialize(
                row: #private::scylla::deserialize::row::ColumnIterator<'frame, 'metadata>,
            ) -> Result<Self, #private::scylla::deserialize::DeserializationError> {
                // read each column into the field with its name, whatever the column order
                #(#locals)*
                for column in row {
                    let column = column?;
                    match column.spec.name() {
                        #(#arms)*
                        _ => {}
                    }
                }
                Ok(Self {
                    #(#builds,)*
                })
            }
        }
    }
}

/// Expand `#[derive(ScyllaProjection)]`
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

/// Build the projection's generated items
///
/// # Arguments
///
/// * `ast` - The struct the derive is on
fn expand(ast: &syn::DeriveInput) -> darling::Result<TokenStream> {
    let attrs = ProjectionAttrs::from_derive_input(ast)?;
    model::no_generics(&attrs.generics)?;
    let paths = Paths::new(attrs.krate.as_ref());
    let schema = &paths.schema;
    let ident = &attrs.ident;
    let of = &attrs.of.0;
    let Some(first) = of.first() else {
        return Err(
            darling::Error::custom("a projection reads at least one object").with_span(ident),
        );
    };
    let fields = attrs.data.take_struct().expect("only named structs").fields;
    let columns = model::columns(Kind::Projection, &ident.to_string(), ident, fields)?;
    // select each column by name, or an aggregate under the field's column name
    let selected = columns.iter().map(|column| match &column.expr {
        Some(expr) => format!("{expr} AS {}", column.column),
        None => column.column.clone(),
    });
    // check every column read directly, but not aggregates, against its object
    let checks = columns
        .iter()
        .filter(|column| column.expr.is_none())
        .map(|column| column_check(ident, column, of, &paths));
    let source = quote! { <#first as #schema::ScyllaObject>::NAME };
    let deserialize = deserialize_row(ident, &columns, &source, &paths);
    // a projection with a redacted field generates its own Debug
    let debug = if columns.iter().any(|column| column.redact) {
        model::debug_impl(&quote! { #ident }, &ident.to_string(), &columns)
    } else {
        TokenStream::new()
    };
    // name the columns of the redacted fields, so each select can check them against the object it reads
    let redacted = columns
        .iter()
        .filter(|column| column.redact)
        .map(|column| &column.column);
    // give each field's value by its position, so generated cursors read their key columns
    let projected = columns.iter().enumerate().map(|(index, column)| {
        let field = &column.field;
        let ty = &column.ty;
        quote! {
            impl #schema::ProjectedAt<#index> for #ident {
                type Value = #ty;
                #[allow(clippy::misnamed_getters)]
                fn value(&self) -> &Self::Value {
                    &self.#field
                }
            }
        }
    });
    Ok(quote! {
        impl #schema::Projection for #ident {
            const COLUMNS: &'static [&'static str] = &[#(#selected),*];
            const REDACTED: &'static [&'static str] = &[#(#redacted),*];
        }
        #(#projected)*
        #(#checks)*
        #deserialize
        #debug
    })
}
