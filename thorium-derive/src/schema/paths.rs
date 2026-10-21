//! The paths generated code uses to reach the api crate's schema runtime

use proc_macro2::TokenStream;
use quote::quote;

/// The roots every generated path starts from
pub struct Paths {
    /// The schema module (`::thorium::models::schema`)
    pub schema: TokenStream,
    /// The re-exports for generated code (`::thorium::models::schema::__private`)
    pub private: TokenStream,
}

impl Paths {
    /// Build the paths, honoring a `crate = "..."` override of the `::thorium` root
    ///
    /// # Arguments
    ///
    /// * `krate` - The overriding root of the api crate, if one was given
    pub fn new(krate: Option<&syn::Path>) -> Self {
        // use the override as the crate root, or the api crate's name
        let root = match krate {
            Some(path) => quote! { #path },
            None => quote! { ::thorium },
        };
        Paths {
            schema: quote! { #root::models::schema },
            private: quote! { #root::models::schema::__private },
        }
    }
}
