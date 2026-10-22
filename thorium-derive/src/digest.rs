//! Derives `DigestValue`, the default-pruned value Thorium hashes for content digests
//!
//! The generated impl mirrors serde's field and variant names (`rename` and `rename_all`) and
//! leaves out every struct field whose digest value equals the value serde fills in when the
//! field is absent: `path()` for `#[serde(default = "path")]`, the field type's `Default` for a
//! plain `#[serde(default)]`, and the same field of `Self::default()` otherwise. Serde keys that
//! change what is stored in ways the digest doesn't model (such as `skip`, `flatten`, `tag`, or
//! `with`) are compile errors, so a digest always matches the stored value.

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, LitStr};

/// The serde rename rules a container can apply with `rename_all`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameRule {
    /// Keep names as they are written
    None,
    /// `lowercase`
    LowerCase,
    /// `UPPERCASE`
    UpperCase,
    /// `PascalCase`
    PascalCase,
    /// `camelCase`
    CamelCase,
    /// `snake_case`
    SnakeCase,
    /// `SCREAMING_SNAKE_CASE`
    ScreamingSnakeCase,
    /// `kebab-case`
    KebabCase,
    /// `SCREAMING-KEBAB-CASE`
    ScreamingKebabCase,
}

impl RenameRule {
    /// Parse a serde `rename_all` value into a rule
    ///
    /// # Arguments
    ///
    /// * `lit` - The string literal given to `rename_all`
    fn parse(lit: &LitStr) -> syn::Result<Self> {
        // map serde's rule names to our rules
        match lit.value().as_str() {
            "lowercase" => Ok(RenameRule::LowerCase),
            "UPPERCASE" => Ok(RenameRule::UpperCase),
            "PascalCase" => Ok(RenameRule::PascalCase),
            "camelCase" => Ok(RenameRule::CamelCase),
            "snake_case" => Ok(RenameRule::SnakeCase),
            "SCREAMING_SNAKE_CASE" => Ok(RenameRule::ScreamingSnakeCase),
            "kebab-case" => Ok(RenameRule::KebabCase),
            "SCREAMING-KEBAB-CASE" => Ok(RenameRule::ScreamingKebabCase),
            unknown => Err(syn::Error::new(
                lit.span(),
                format!("unknown rename_all rule: {unknown}"),
            )),
        }
    }

    /// Apply this rule to a `PascalCase` variant name the way serde does
    ///
    /// # Arguments
    ///
    /// * `variant` - The variant name to rename
    fn apply_to_variant(self, variant: &str) -> String {
        // rename the variant based on our rule
        match self {
            RenameRule::None | RenameRule::PascalCase => variant.to_owned(),
            RenameRule::LowerCase => variant.to_ascii_lowercase(),
            RenameRule::UpperCase => variant.to_ascii_uppercase(),
            RenameRule::CamelCase => {
                // lowercase only the first character
                let mut chars = variant.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_ascii_lowercase().to_string() + chars.as_str()
                })
            }
            RenameRule::SnakeCase => {
                // insert an underscore before every uppercase character after the first
                let mut snake = String::with_capacity(variant.len() + 4);
                for (i, ch) in variant.char_indices() {
                    if i > 0 && ch.is_uppercase() {
                        snake.push('_');
                    }
                    snake.push(ch.to_ascii_lowercase());
                }
                snake
            }
            RenameRule::ScreamingSnakeCase => RenameRule::SnakeCase
                .apply_to_variant(variant)
                .to_ascii_uppercase(),
            RenameRule::KebabCase => RenameRule::SnakeCase
                .apply_to_variant(variant)
                .replace('_', "-"),
            RenameRule::ScreamingKebabCase => RenameRule::ScreamingSnakeCase
                .apply_to_variant(variant)
                .replace('_', "-"),
        }
    }

    /// Apply this rule to a `snake_case` field name the way serde does
    ///
    /// # Arguments
    ///
    /// * `field` - The field name to rename
    fn apply_to_field(self, field: &str) -> String {
        // rename the field based on our rule
        match self {
            RenameRule::None | RenameRule::LowerCase | RenameRule::SnakeCase => field.to_owned(),
            RenameRule::UpperCase | RenameRule::ScreamingSnakeCase => field.to_ascii_uppercase(),
            RenameRule::PascalCase => {
                // capitalize the first character and every character after an underscore
                let mut pascal = String::with_capacity(field.len());
                let mut capitalize = true;
                for ch in field.chars() {
                    if ch == '_' {
                        capitalize = true;
                    } else if capitalize {
                        pascal.push(ch.to_ascii_uppercase());
                        capitalize = false;
                    } else {
                        pascal.push(ch);
                    }
                }
                pascal
            }
            RenameRule::CamelCase => {
                // build the pascal case name and lowercase its first character
                let pascal = RenameRule::PascalCase.apply_to_field(field);
                let mut chars = pascal.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_ascii_lowercase().to_string() + chars.as_str()
                })
            }
            RenameRule::KebabCase => field.replace('_', "-"),
            RenameRule::ScreamingKebabCase => field.to_ascii_uppercase().replace('_', "-"),
        }
    }
}

/// The serde attribute keys the digest derive reads from one field, variant, or container
#[derive(Default)]
struct SerdeAttrs {
    /// The name set with `rename`
    rename: Option<String>,
    /// The rule set with `rename_all`
    rename_all: Option<RenameRule>,
    /// The function path set with `default = "path"`
    default_path: Option<syn::ExprPath>,
    /// Whether a plain `default` without a path is set
    plain_default: bool,
}

/// The serde keys the digest derive accepts without reading them, since they never change the
/// value serde stores or fills in
const IGNORED_SERDE_KEYS: &[&str] = &[
    "deserialize_with",
    "skip_serializing_if",
    "alias",
    "bound",
    "deny_unknown_fields",
    "borrow",
    "expecting",
];

/// Skip the value of a serde attribute key the digest derive doesn't read
///
/// # Arguments
///
/// * `meta` - The nested meta whose value should be consumed
fn skip_meta_value(meta: &syn::meta::ParseNestedMeta) -> syn::Result<()> {
    // consume a `key = value` or `key(...)` value so parsing can continue
    if meta.input.peek(syn::Token![=]) {
        meta.value()?.parse::<syn::Expr>()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.parse_nested_meta(|nested| skip_meta_value(&nested))?;
    }
    Ok(())
}

/// Read the serialize name from a `rename` or `rename_all` value
///
/// Both `key = "value"` and `key(serialize = "value", ...)` are accepted.
///
/// # Arguments
///
/// * `meta` - The nested meta for the `rename` or `rename_all` key
fn serialize_name(meta: &syn::meta::ParseNestedMeta) -> syn::Result<Option<LitStr>> {
    // a plain `key = "value"` names both directions
    if meta.input.peek(syn::Token![=]) {
        return Ok(Some(meta.value()?.parse()?));
    }
    // otherwise look for the serialize name in a list
    let mut name = None;
    meta.parse_nested_meta(|nested| {
        if nested.path.is_ident("serialize") {
            name = Some(nested.value()?.parse()?);
            Ok(())
        } else {
            skip_meta_value(&nested)
        }
    })?;
    Ok(name)
}

/// Merge every `#[serde(...)]` attribute in a list, reading `rename`, `rename_all`, and `default`
///
/// Empty attributes and the keys in [`IGNORED_SERDE_KEYS`] are skipped, and every other key is an
/// error because the digest wouldn't match what serde stores.
///
/// # Arguments
///
/// * `attrs` - The attributes to read
fn parse_serde_attrs(attrs: &[syn::Attribute]) -> syn::Result<SerdeAttrs> {
    // start with no serde settings
    let mut parsed = SerdeAttrs::default();
    // walk every serde attribute
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
        // skip empty `#[serde()]` attributes
        if let syn::Meta::List(list) = &attr.meta
            && list.tokens.is_empty()
        {
            continue;
        }
        // read the keys we care about and skip the rest
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                if let Some(name) = serialize_name(&meta)? {
                    parsed.rename = Some(name.value());
                }
            } else if meta.path.is_ident("rename_all") {
                if let Some(rule) = serialize_name(&meta)? {
                    parsed.rename_all = Some(RenameRule::parse(&rule)?);
                }
            } else if meta.path.is_ident("default") {
                // a default is either a function path or the type's own default
                if meta.input.peek(syn::Token![=]) {
                    let path: LitStr = meta.value()?.parse()?;
                    parsed.default_path = Some(path.parse()?);
                } else {
                    parsed.plain_default = true;
                }
            } else if IGNORED_SERDE_KEYS.iter().any(|key| meta.path.is_ident(key)) {
                skip_meta_value(&meta)?;
            } else {
                // any other key changes what serde stores in a way the digest doesn't model
                let key = meta
                    .path
                    .get_ident()
                    .map_or_else(|| "?".to_owned(), ToString::to_string);
                return Err(meta.error(format!(
                    "DigestValue doesn't support `#[serde({key})]`, so the digest wouldn't match \
                    the stored value"
                )));
            }
            Ok(())
        })?;
    }
    Ok(parsed)
}

/// Get the name of an identifier without a raw identifier prefix
///
/// # Arguments
///
/// * `ident` - The identifier to name
fn unraw(ident: &syn::Ident) -> String {
    // strip any `r#` prefix like serde does
    let name = ident.to_string();
    name.strip_prefix("r#")
        .map_or(name.clone(), ToOwned::to_owned)
}

/// Build the statements that insert each named field that differs from its default into `map`
///
/// # Arguments
///
/// * `fields` - The named fields to digest
/// * `rule` - The container's rename rule
/// * `access` - Builds the expression that reads a field's current value
/// * `default` - Builds the expression for a field's default when it has no default of its own
fn named_field_inserts<A, D>(
    fields: &syn::FieldsNamed,
    rule: RenameRule,
    access: A,
    default: D,
) -> syn::Result<Vec<TokenStream>>
where
    A: Fn(&syn::Field) -> TokenStream,
    D: Fn(&syn::Field) -> TokenStream,
{
    // build one insert per field
    let mut inserts = Vec::with_capacity(fields.named.len());
    for field in &fields.named {
        // get this field's serde settings
        let attrs = parse_serde_attrs(&field.attrs)?;
        // get the name serde uses for this field
        let ident = field
            .ident
            .as_ref()
            .ok_or_else(|| syn::Error::new(field.span(), "expected a named field"))?;
        let key = attrs
            .rename
            .unwrap_or_else(|| rule.apply_to_field(&unraw(ident)));
        // compare against the value serde fills in when this field is absent
        let default_expr = match &attrs.default_path {
            Some(path) => quote! { #path() },
            None if attrs.plain_default => {
                let ty = &field.ty;
                quote! { <#ty as ::core::default::Default>::default() }
            }
            None => default(field),
        };
        let current = access(field);
        inserts.push(quote! {
            let value = crate::models::DigestValue::digest_value(#current);
            if value != crate::models::DigestValue::digest_value(&#default_expr) {
                map.insert(#key.to_owned(), value);
            }
        });
    }
    Ok(inserts)
}

/// Build the body of `digest_value` for a struct
///
/// # Arguments
///
/// * `fields` - The struct's fields
/// * `rule` - The container's rename rule
fn struct_body(fields: &Fields, rule: RenameRule) -> syn::Result<TokenStream> {
    match fields {
        Fields::Named(named) => {
            // compare each field against the same field of our default
            let inserts = named_field_inserts(
                named,
                rule,
                |field| {
                    let ident = &field.ident;
                    quote! { &self.#ident }
                },
                |field| {
                    let ident = &field.ident;
                    quote! { defaults.#ident }
                },
            )?;
            // a struct without fields digests as an empty object
            if inserts.is_empty() {
                return Ok(quote! { serde_json::Value::Object(serde_json::Map::new()) });
            }
            Ok(quote! {
                let defaults = <Self as ::core::default::Default>::default();
                let mut map = serde_json::Map::new();
                #(#inserts)*
                serde_json::Value::Object(map)
            })
        }
        Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
            // a newtype struct digests as its inner value like serde serializes it
            Ok(quote! { crate::models::DigestValue::digest_value(&self.0) })
        }
        Fields::Unnamed(unnamed) => {
            // a tuple struct digests as an array of its values
            let indexes = (0..unnamed.unnamed.len()).map(syn::Index::from);
            Ok(quote! {
                serde_json::Value::Array(vec![#(crate::models::DigestValue::digest_value(&self.#indexes)),*])
            })
        }
        Fields::Unit => Ok(quote! { serde_json::Value::Object(serde_json::Map::new()) }),
    }
}

/// Build the body of `digest_value` for an enum
///
/// # Arguments
///
/// * `data` - The enum's variants
/// * `rule` - The container's rename rule
fn enum_body(data: &syn::DataEnum, rule: RenameRule) -> syn::Result<TokenStream> {
    // build one match arm per variant
    let mut arms = Vec::with_capacity(data.variants.len());
    for variant in &data.variants {
        // get the name serde uses for this variant
        let attrs = parse_serde_attrs(&variant.attrs)?;
        let ident = &variant.ident;
        let name = attrs
            .rename
            .unwrap_or_else(|| rule.apply_to_variant(&unraw(ident)));
        // a variant's own rename_all applies to its fields
        let field_rule = attrs.rename_all.unwrap_or(RenameRule::None);
        let arm = match &variant.fields {
            Fields::Unit => quote! {
                Self::#ident => serde_json::Value::String(#name.to_owned()),
            },
            Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => quote! {
                Self::#ident(inner) => {
                    let mut map = serde_json::Map::new();
                    map.insert(#name.to_owned(), crate::models::DigestValue::digest_value(inner));
                    serde_json::Value::Object(map)
                }
            },
            Fields::Unnamed(unnamed) => {
                // bind each tuple value and digest them as an array
                let binds: Vec<syn::Ident> = (0..unnamed.unnamed.len())
                    .map(|i| quote::format_ident!("field_{i}"))
                    .collect();
                quote! {
                    Self::#ident(#(#binds),*) => {
                        let mut map = serde_json::Map::new();
                        let values = vec![#(crate::models::DigestValue::digest_value(#binds)),*];
                        map.insert(#name.to_owned(), serde_json::Value::Array(values));
                        serde_json::Value::Object(map)
                    }
                }
            }
            Fields::Named(named) => {
                // prune each struct variant field against its type's default
                let binds: Vec<&Option<syn::Ident>> =
                    named.named.iter().map(|field| &field.ident).collect();
                let inserts = named_field_inserts(
                    named,
                    field_rule,
                    |field| {
                        let ident = &field.ident;
                        quote! { #ident }
                    },
                    |field| {
                        let ty = &field.ty;
                        quote! { <#ty as ::core::default::Default>::default() }
                    },
                )?;
                // a variant without fields carries an empty object
                let declare = if inserts.is_empty() {
                    quote! { let map = serde_json::Map::new(); }
                } else {
                    quote! { let mut map = serde_json::Map::new(); }
                };
                quote! {
                    Self::#ident { #(#binds),* } => {
                        #declare
                        #(#inserts)*
                        let mut outer = serde_json::Map::new();
                        outer.insert(#name.to_owned(), serde_json::Value::Object(map));
                        serde_json::Value::Object(outer)
                    }
                }
            }
        };
        arms.push(arm);
    }
    Ok(quote! {
        match self {
            #(#arms)*
        }
    })
}

/// Derive `DigestValue` for a struct or enum
///
/// # Arguments
///
/// * `ast` - The parsed item to derive for
pub fn derive(ast: &DeriveInput) -> syn::Result<TokenStream> {
    // get the container's serde settings
    let container = parse_serde_attrs(&ast.attrs)?;
    // fields are pruned against `Self::default()`, so a container default function isn't modeled
    if container.default_path.is_some() {
        return Err(syn::Error::new(
            ast.span(),
            "DigestValue doesn't support a container `#[serde(default = \"path\")]`; use \
            `#[serde(default)]` with a `Default` impl",
        ));
    }
    // get the container's rename rule
    let rule = container.rename_all.unwrap_or(RenameRule::None);
    // build the body for this kind of item
    let body = match &ast.data {
        Data::Struct(data) => struct_body(&data.fields, rule)?,
        Data::Enum(data) => enum_body(data, rule)?,
        Data::Union(_) => {
            return Err(syn::Error::new(
                ast.span(),
                "DigestValue can't be derived for unions",
            ));
        }
    };
    // wrap the body in an impl
    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics crate::models::DigestValue for #name #ty_generics #where_clause {
            /// Builds this value's serde-shaped digest value, leaving out each field that holds
            /// what serde fills in when the field is absent
            fn digest_value(&self) -> serde_json::Value {
                #body
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{RenameRule, derive};

    /// Serde keys that change what is stored in ways the digest doesn't model are errors
    #[test]
    fn unmodeled_serde_keys_are_errors() {
        // field, variant, and container keys that change what serde stores
        let refused: Vec<syn::DeriveInput> = vec![
            syn::parse_quote! { struct S { #[serde(skip)] a: u64 } },
            syn::parse_quote! { struct S { #[serde(skip_serializing)] a: u64 } },
            syn::parse_quote! { struct S { #[serde(skip_deserializing)] a: u64 } },
            syn::parse_quote! { struct S { #[serde(flatten)] a: Inner } },
            syn::parse_quote! { struct S { #[serde(with = "module")] a: u64 } },
            syn::parse_quote! { struct S { #[serde(serialize_with = "func")] a: u64 } },
            syn::parse_quote! { #[serde(tag = "type")] enum E { A } },
            syn::parse_quote! { #[serde(tag = "t", content = "c")] enum E { A(u64) } },
            syn::parse_quote! { #[serde(untagged)] enum E { A(u64) } },
            syn::parse_quote! { #[serde(transparent)] struct S { a: u64 } },
            syn::parse_quote! { #[serde(rename_all_fields = "camelCase")] enum E { A { b: u64 } } },
            syn::parse_quote! { #[serde(default = "make")] struct S { a: u64 } },
            syn::parse_quote! { enum E { #[serde(skip)] A } },
        ];
        for input in &refused {
            assert!(derive(input).is_err(), "accepted {}", quote::quote!(#input));
        }
        // keys that never change the stored value are accepted
        let accepted: Vec<syn::DeriveInput> = vec![
            syn::parse_quote! {
                #[serde(rename_all = "snake_case", deny_unknown_fields)]
                struct S {
                    #[serde(default, deserialize_with = "func", alias = "b")]
                    a: u64,
                    #[serde(default = "make", skip_serializing_if = "Option::is_none")]
                    c: Option<u64>,
                    #[serde(rename = "dee")]
                    d: u64,
                }
            },
            syn::parse_quote! { #[serde(default)] struct S { a: u64 } },
        ];
        for input in &accepted {
            assert!(derive(input).is_ok(), "refused {}", quote::quote!(#input));
        }
    }

    /// Variant renames match serde's rules
    #[test]
    fn variant_rules() {
        // check every rule against a multi word variant
        let cases = [
            (RenameRule::None, "OutputFiles"),
            (RenameRule::LowerCase, "outputfiles"),
            (RenameRule::UpperCase, "OUTPUTFILES"),
            (RenameRule::PascalCase, "OutputFiles"),
            (RenameRule::CamelCase, "outputFiles"),
            (RenameRule::SnakeCase, "output_files"),
            (RenameRule::ScreamingSnakeCase, "OUTPUT_FILES"),
            (RenameRule::KebabCase, "output-files"),
            (RenameRule::ScreamingKebabCase, "OUTPUT-FILES"),
        ];
        for (rule, expected) in cases {
            assert_eq!(rule.apply_to_variant("OutputFiles"), expected, "{rule:?}");
        }
    }

    /// Field renames match serde's rules
    #[test]
    fn field_rules() {
        // check every rule against a multi word field
        let cases = [
            (RenameRule::None, "output_files"),
            (RenameRule::LowerCase, "output_files"),
            (RenameRule::UpperCase, "OUTPUT_FILES"),
            (RenameRule::PascalCase, "OutputFiles"),
            (RenameRule::CamelCase, "outputFiles"),
            (RenameRule::SnakeCase, "output_files"),
            (RenameRule::ScreamingSnakeCase, "OUTPUT_FILES"),
            (RenameRule::KebabCase, "output-files"),
            (RenameRule::ScreamingKebabCase, "OUTPUT-FILES"),
        ];
        for (rule, expected) in cases {
            assert_eq!(rule.apply_to_field("output_files"), expected, "{rule:?}");
        }
    }
}
