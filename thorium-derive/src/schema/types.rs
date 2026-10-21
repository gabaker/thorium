//! Syntactic analysis of field types
//!
//! A proc-macro sees type spellings only, so these helpers recognize the standard containers
//! and scalars by name; any other type (an alias, a stored enum) takes the general path.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{GenericArgument, PathArguments, Type};

/// The scalar types bound by value
const COPY_SCALARS: [&str; 9] = [
    "i8", "i16", "i32", "i64", "bool", "f32", "f64", "Uuid", "DateTime",
];

/// The unsigned integer types, which need an explicit column type
pub const UNSIGNED: [&str; 6] = ["u8", "u16", "u32", "u64", "u128", "usize"];

/// Get the last path segment's name and its type arguments
///
/// # Arguments
///
/// * `ty` - The type to inspect
pub fn last_segment(ty: &Type) -> Option<(String, Vec<&Type>)> {
    // only plain paths have a last segment
    let Type::Path(path) = ty else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let segment = path.path.segments.last()?;
    // collect the segment's type arguments
    let args = match &segment.arguments {
        PathArguments::AngleBracketed(args) => args
            .args
            .iter()
            .filter_map(|arg| match arg {
                GenericArgument::Type(ty) => Some(ty),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((segment.ident.to_string(), args))
}

/// Get the inner type of an `Option`, if this is one
///
/// # Arguments
///
/// * `ty` - The type to inspect
pub fn option_inner(ty: &Type) -> Option<&Type> {
    match last_segment(ty) {
        Some((name, args)) if name == "Option" && args.len() == 1 => Some(args[0]),
        _ => None,
    }
}

/// Find the first unsigned integer type named anywhere in a type
///
/// # Arguments
///
/// * `ty` - The type to search
pub fn find_unsigned(ty: &Type) -> Option<String> {
    let (name, args) = last_segment(ty)?;
    // this segment may itself be unsigned
    if UNSIGNED.contains(&name.as_str()) {
        return Some(name);
    }
    // otherwise look through its type arguments
    args.into_iter().find_map(find_unsigned)
}

/// The kinds of containers a field's borrowed form depends on
enum Shape {
    /// `String`
    Text,
    /// `Vec<T>`, borrowed as a slice of `T`
    List(TokenStream),
    /// A set or map, borrowed whole
    Collection,
    /// A scalar copied by value
    Copy,
    /// `JsonText<T>`, borrowed as `JsonText<&T>`
    JsonText(TokenStream),
    /// Any other type, borrowed whole
    Other,
}

/// Classify a type by its spelling
///
/// # Arguments
///
/// * `ty` - The type to classify
fn shape(ty: &Type) -> Shape {
    match last_segment(ty) {
        Some((name, args)) => match (name.as_str(), args.as_slice()) {
            ("String", []) => Shape::Text,
            ("Vec", [inner]) => Shape::List(quote! { #inner }),
            ("HashSet" | "BTreeSet" | "HashMap" | "BTreeMap", _) => Shape::Collection,
            ("JsonText", [inner]) => Shape::JsonText(quote! { #inner }),
            (name, _) if COPY_SCALARS.contains(&name) || UNSIGNED.contains(&name) => Shape::Copy,
            _ => Shape::Other,
        },
        None => Shape::Other,
    }
}

/// How a borrowed row's field is built from the owned field
pub enum Borrow {
    /// `&self.field`
    Reference,
    /// `self.field.as_deref()`
    AsDeref,
    /// `self.field.as_ref()`
    AsRef,
    /// `self.field`
    Copy,
    /// `JsonText(&self.field.0)`
    JsonText,
}

/// Map a table field's type to its borrowed row field's type, and how to borrow it
///
/// # Arguments
///
/// * `ty` - The table field's type
/// * `schema` - The path of the schema module
pub fn borrowed(ty: &Type, schema: &TokenStream) -> (TokenStream, Borrow) {
    // optional fields borrow their inner value
    if let Some(inner) = option_inner(ty) {
        return match shape(inner) {
            Shape::Text => (quote! { Option<&'a str> }, Borrow::AsDeref),
            Shape::List(elem) => (quote! { Option<&'a [#elem]> }, Borrow::AsDeref),
            Shape::Copy => (quote! { #ty }, Borrow::Copy),
            Shape::Collection | Shape::JsonText(_) | Shape::Other => {
                (quote! { Option<&'a #inner> }, Borrow::AsRef)
            }
        };
    }
    match shape(ty) {
        Shape::Text => (quote! { &'a str }, Borrow::Reference),
        Shape::List(elem) => (quote! { &'a [#elem] }, Borrow::Reference),
        Shape::Copy => (quote! { #ty }, Borrow::Copy),
        Shape::JsonText(inner) => (quote! { #schema::JsonText<&'a #inner> }, Borrow::JsonText),
        Shape::Collection | Shape::Other => (quote! { &'a #ty }, Borrow::Reference),
    }
}
