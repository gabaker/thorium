//! `#[derive(ScyllaView)]`: a materialized view's definition and its checks against its base

use darling::FromDeriveInput;
use proc_macro2::TokenStream;
use quote::quote;

use super::attrs::ViewAttrs;
use super::model::{self, Kind};
use super::paths::Paths;

/// Expand `#[derive(ScyllaView)]`
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

/// Build the view's generated items
///
/// # Arguments
///
/// * `ast` - The struct the derive is on
fn expand(ast: &syn::DeriveInput) -> darling::Result<TokenStream> {
    let attrs = ViewAttrs::from_derive_input(ast)?;
    model::no_generics(&attrs.generics)?;
    let paths = Paths::new(attrs.krate.as_ref());
    let schema = &paths.schema;
    let private = &paths.private;
    let ident = &attrs.ident;
    let view = &attrs.name;
    let base = &attrs.base;
    let fields = attrs.data.take_struct().expect("only named structs").fields;
    let columns = model::columns(Kind::View, view, ident, fields)?;
    // build the view's definition from its columns
    let defs = columns.iter().map(|column| column.column_def(&paths));
    let filter = match &attrs.filter {
        Some(filter) => quote! { Some(#filter) },
        None => quote! { None },
    };
    // every column must exist in the base with the same type, found through the base's
    // definition so the base can be named by any path
    let column_checks = columns.iter().map(|column| {
        let name = &column.column;
        let desc = column.desc(&paths);
        let redact = column.redact;
        let label = format!("view {view}.");
        quote! {
            const _: () = #private::assert_view_column(
                #desc,
                &<#base as #schema::ScyllaObject>::DEF,
                #view,
                #name,
            );
            const _: () = #private::assert_redacted(
                #redact,
                &[<#base as #schema::ScyllaObject>::REDACTED],
                #name,
                #label,
            );
        }
    });
    // a view with a redacted field generates its own Debug, like a table
    let owned_debug = if columns.iter().any(|column| column.redact) {
        model::debug_impl(&quote! { #ident }, &ident.to_string(), &columns)
    } else {
        TokenStream::new()
    };
    let redacted = model::redacted_names(&columns);
    let fields = model::field_names(&columns);
    let column_at = model::column_at_impls(ident, view, &columns, &paths);
    Ok(quote! {
        impl #schema::ScyllaObject for #ident {
            const NAME: &'static str = #view;
            const DEF: #schema::ObjectDef = #schema::ObjectDef::View(#schema::ViewDef {
                name: #view,
                base: <#base as #schema::ScyllaObject>::NAME,
                columns: &[#(#defs),*],
                filter: #filter,
            });
            const REDACTED: &'static [&'static str] = #redacted;
            const FIELDS: &'static [&'static str] = #fields;
        }
        #column_at
        #(#column_checks)*
        // the view's key must hold every key column of its base
        const _: () = #private::assert_view_key(
            &<#base as #schema::ScyllaObject>::DEF,
            &<#ident as #schema::ScyllaObject>::DEF,
        );
        #owned_debug
    })
}
