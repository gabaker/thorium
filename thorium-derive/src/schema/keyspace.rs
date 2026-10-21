//! `#[thorium::keyspace]`: every declared object in one ordered registry

use darling::FromMeta;
use darling::ast::NestedMeta;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use super::attrs::KeyspaceArgs;
use super::paths::Paths;

/// Expand `#[thorium::keyspace]` on a struct whose field types are the declared objects
///
/// # Arguments
///
/// * `args` - The attribute's arguments
/// * `input` - The struct the attribute is on
pub fn expand(args: TokenStream, input: TokenStream) -> TokenStream {
    match build(args, input) {
        Ok(tokens) => tokens,
        Err(errors) => errors.write_errors(),
    }
}

/// Build the keyspace's generated items
///
/// # Arguments
///
/// * `args` - The attribute's arguments
/// * `input` - The struct the attribute is on
fn build(args: TokenStream, input: TokenStream) -> darling::Result<TokenStream> {
    let args = KeyspaceArgs::from_list(&NestedMeta::parse_meta_list(args)?)?;
    let item: syn::ItemStruct = syn::parse2(input)?;
    let paths = Paths::new(args.krate.as_ref());
    let schema = &paths.schema;
    let private = &paths.private;
    let ident = &item.ident;
    // every field's type is one object, listed in field order
    let syn::Fields::Named(fields) = &item.fields else {
        return Err(darling::Error::custom("a keyspace needs named fields").with_span(&item));
    };
    let test = format_ident!("{}_validates", ident.to_string().to_lowercase());
    let objects = fields.named.iter().map(|field| {
        let ty = &field.ty;
        quote! { <#ty as #schema::ScyllaObject>::DEF }
    });
    Ok(quote! {
        #[allow(dead_code)]
        #item
        impl #schema::Keyspace for #ident {
            /// Every table and view the keyspace declares, in declaration order
            const OBJECTS: &'static [#schema::ObjectDef] = &[#(#objects),*];
        }
        // two objects can't share a name
        const _: () = #private::assert_unique_objects(<#ident as #schema::Keyspace>::OBJECTS);
        /// The keyspace follows every rule of the declared schema
        #[cfg(test)]
        #[test]
        fn #test() {
            if let Err(errors) = #schema::validate(<#ident as #schema::Keyspace>::OBJECTS) {
                let errors = errors.iter().map(ToString::to_string).collect::<Vec<_>>();
                panic!("the keyspace breaks schema rules: {}", errors.join("; "));
            }
        }
    })
}
