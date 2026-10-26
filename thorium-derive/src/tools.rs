//! A proc-macro for registering ThorPal's compiled in tools
//!
//! ThorPal tools are plain functions turned into [`rig::tool::Tool`] impls by
//! rig's `#[rig::tool_macro]` attribute. This module adds a second attribute,
//! [`thorpal_tool`], stacked above rig's, that registers the generated tool
//! struct into ThorPal's compile-time tool registry
//! (`crate::thorpal::tools::registry`) along with its group, requirements, log
//! routing, and coverage metadata. Annotating the function is the only step
//! needed to add a new compiled in tool to ThorPal.

use darling::FromMeta;
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{ItemFn, LitStr};

/// The arguments to the [`thorpal_tool`] attribute
#[derive(Debug, Clone, Default, FromMeta)]
#[darling(derive_syn_parse, default)]
struct ThorpalToolArgs {
    /// The tool group this tool belongs to (e.g. `ghidra`)
    group: Option<LitStr>,
    /// The tracing target prefix this tool's streaming logs are emitted under
    logs: Option<LitStr>,
    /// Whether calling this tool counts as examining a function for coverage
    examining: bool,
    /// Whether this tool takes a `target` arg naming a single function
    function_scoped: bool,
    /// Whether this tool mutates the analyzed project (write-side)
    mutating: bool,
    /// The external requirement this tool needs before it can run (e.g. `ghidra`)
    requires: Option<LitStr>,
    /// The path to this tool's Rhai binding registrar, when scripts can call it
    rhai_bind: Option<LitStr>,
    /// The model-visible Rhai signature and one-line doc for the binding
    rhai_doc: Option<LitStr>,
    /// The full model-visible reference for the binding (args + return shape)
    rhai_reference: Option<LitStr>,
}

/// Register a ThorPal tool into the compile-time tool registry
///
/// # Arguments
///
/// * `args_raw` - The raw attribute arguments to parse
/// * `input` - The tool function this attribute was applied to
pub fn thorpal_tool(args_raw: TokenStream, input: TokenStream) -> TokenStream {
    // parse our attribute arguments
    let args: ThorpalToolArgs = match syn::parse(args_raw) {
        Ok(args) => args,
        Err(err) => return err.into_compile_error().into(),
    };
    // this attribute only makes sense on the tool functions rig expands
    let item_fn = match syn::parse::<ItemFn>(input) {
        Ok(item_fn) => item_fn,
        Err(_) => {
            return syn::Error::new(
                Span::call_site(),
                "`thorpal_tool` attribute can only be applied to functions",
            )
            .into_compile_error()
            .into();
        }
    };
    // build the registration for this tool
    let registration = match build_registration(&args, &item_fn) {
        Ok(registration) => registration,
        Err(err) => return err.into_compile_error().into(),
    };
    // re-emit the untouched function (rig's attribute still on it) + registration
    quote! {
        #item_fn

        #registration
    }
    .into()
}

/// Build the `inventory::submit!` registration for a tool function
///
/// # Arguments
///
/// * `args` - The parsed attribute arguments
/// * `item_fn` - The tool function being registered
fn build_registration(
    args: &ThorpalToolArgs,
    item_fn: &ItemFn,
) -> Result<proc_macro2::TokenStream, syn::Error> {
    // a tool must always declare the group it belongs to
    let Some(group) = &args.group else {
        return Err(syn::Error::new(
            Span::call_site(),
            "`thorpal_tool` requires a group (e.g. `group = \"ghidra\"`)",
        ));
    };
    // rig's attribute must still be below us or no tool struct will be generated
    let has_rig_attr = item_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "tool_macro")
    });
    if !has_rig_attr {
        return Err(syn::Error::new(
            Span::call_site(),
            "`thorpal_tool` must be placed above `#[rig::tool_macro]` on a rig tool function",
        ));
    }
    // derive the tool struct ident the same way rig's macro does (PascalCase)
    let fn_name = item_fn.sig.ident.to_string();
    let struct_name = format_ident!(
        "{}",
        convert_case::Casing::to_case(&fn_name, convert_case::Case::Pascal)
    );
    // map the requirement name onto the registry's Requirement enum
    let requirement = match &args.requires {
        Some(requires) => match requires.value().as_str() {
            "ghidra" => quote! { Ghidra },
            unknown => {
                return Err(syn::Error::new(
                    requires.span(),
                    format!("unknown `thorpal_tool` requirement '{unknown}'; expected `ghidra`"),
                ));
            }
        },
        None => quote! { None },
    };
    // an absent logs arg means this tool never streams logs
    let log_targets = match &args.logs {
        Some(logs) => quote! { &[#logs] },
        None => quote! { &[] },
    };
    // a rhai binding is only usable when the model can also see its signature
    let (rhai_bind, rhai_signature) = match (&args.rhai_bind, &args.rhai_doc) {
        // a bound tool carries both its registrar path and its signature doc
        (Some(bind), Some(doc)) => {
            // the registrar is given as a string but must parse to a fn path
            let path: syn::Path = syn::parse_str(&bind.value()).map_err(|_| {
                syn::Error::new(
                    bind.span(),
                    "`rhai_bind` must be a path to a binding registrar fn (e.g. \
                     `crate::thorpal::tools::scripts::bindings::bind_read`)",
                )
            })?;
            (quote! { Some(#path) }, quote! { Some(#doc) })
        }
        // an unbound tool carries neither
        (None, None) => (quote! { None }, quote! { None }),
        // one without the other is a mistake worth failing the build over
        _ => {
            return Err(syn::Error::new(
                Span::call_site(),
                "`rhai_bind` and `rhai_doc` must be given together",
            ));
        }
    };
    // the full reference is optional but only meaningful on a bound tool
    let rhai_reference = match &args.rhai_reference {
        Some(reference) => {
            // a reference without a binding documents a function scripts
            // cannot call, so reject it as a likely mistake
            if args.rhai_bind.is_none() {
                return Err(syn::Error::new(
                    reference.span(),
                    "`rhai_reference` requires `rhai_bind` and `rhai_doc`",
                ));
            }
            quote! { Some(#reference) }
        }
        None => quote! { None },
    };
    // pull out the coverage and mutation flags
    let examining = args.examining;
    let function_scoped = args.function_scoped;
    let mutating = args.mutating;
    // register this tool into ThorPal's compile-time registry
    Ok(quote! {
        ::inventory::submit! {
            crate::thorpal::tools::registry::ToolEntry {
                name: <#struct_name as ::rig::tool::Tool>::NAME,
                group: #group,
                requires: crate::thorpal::tools::registry::Requirement::#requirement,
                log_targets: #log_targets,
                examining: #examining,
                function_scoped: #function_scoped,
                mutating: #mutating,
                rhai_bind: #rhai_bind,
                rhai_signature: #rhai_signature,
                rhai_reference: #rhai_reference,
                apply: |agent| agent.tool(#struct_name),
            }
        }
    })
}
