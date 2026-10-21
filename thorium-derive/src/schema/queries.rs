//! `#[thorium::table_queries]` and `#[thorium::view_queries]`: one struct of prepared statements
//! and typed methods per table or view
//!
//! A queries struct names columns by their Rust field. The macro can't see the object's fields,
//! so the generated code reaches each column through the object's `ColumnAt` impl for that
//! field's position, found in const context; the column's CQL name, its bind types, and the
//! checks on each statement's restrictions all come from the object's own definition.

use darling::FromMeta;
use darling::ast::NestedMeta;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::Token;
use syn::ext::IdentExt;
use syn::punctuated::Punctuated;

use super::attrs::{ColumnsArg, InsertArgs, QueriesAttrs};
use super::paths::Paths;

/// The statement kinds a queries field can declare, by attribute name
const KINDS: [&str; 7] = [
    "insert", "select", "update", "delete", "cursor", "census", "query",
];

/// The statement kinds that write, which a view can't declare
const WRITES: [&str; 3] = ["insert", "update", "delete"];

/// Which kind of object a queries struct targets
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A base table, which takes every statement kind
    Table,
    /// A materialized view, which is read-only
    View,
}

/// One generated statement: its text, its typed methods, and its compile-time checks
pub(super) struct Statement {
    /// The field holding the prepared statement
    pub(super) field: syn::Ident,
    /// The field's doc attributes
    pub(super) docs: Vec<syn::Attribute>,
    /// The associated function rendering the statement's text for a keyspace `ns`
    pub(super) cql: TokenStream,
    /// The typed methods executing the statement
    pub(super) methods: TokenStream,
    /// Module-level items: const checks of the statement against its object, and any bind
    /// struct it takes
    pub(super) items: TokenStream,
    /// A field holding a generated type of several statements instead of one statement
    pub(super) grouped: Option<Grouped>,
}

/// A queries field that holds a generated type of several statements, such as a cursor's
pub(super) struct Grouped {
    /// The generated type
    pub(super) ty: syn::Ident,
}

/// How a statement restricts one column
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `column = ?`
    Eq,
    /// `column IN ?`
    In,
    /// A one-sided range on a clustering column, such as `column >= ?`
    Range(RangeOp),
}

/// The comparison of a one-sided range
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeOp {
    /// `>=`
    Ge,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `<`
    Lt,
}

/// One restricted column, by its field name
struct Restricted {
    /// The field naming the column
    field: syn::Ident,
    /// How the column is restricted
    op: Op,
}

/// Which key columns a delete covers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coverage {
    /// The partition key and a prefix of the clustering key
    Prefix,
    /// Exactly the partition key
    Partition,
    /// Every key column
    Key,
}

/// The parsed arguments of a `#[select(...)]`, `#[delete(...)]`, `#[update(...)]`, or
/// `#[census(...)]` field
// each flag is one independent statement argument
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct StatementArgs {
    /// The projection a select reads
    project: Option<syn::Path>,
    /// The restricted columns, in declaration order
    restricted: Vec<Restricted>,
    /// The grouped columns
    group_by: Vec<syn::Ident>,
    /// Whether a select only checks that a row exists
    exists: bool,
    /// A literal `LIMIT`
    limit: Option<u32>,
    /// The most values bound per execution of each `IN` list
    chunk: Option<usize>,
    /// Which key columns a delete or update covers
    coverage: Option<Coverage>,
    /// A literal `PER PARTITION LIMIT`
    per_partition_limit: Option<u32>,
    /// Whether a select binds its `PER PARTITION LIMIT`
    per_partition_bound: bool,
    /// The columns an update sets
    set: Vec<syn::Ident>,
    /// Whether a select reads one token range of the partition key instead of restricting keys
    token_range: bool,
    /// Whether a select reads each partition once (`SELECT DISTINCT`)
    distinct: bool,
    /// Whether a select pages through its rows, returning a stream instead of every row
    paged: bool,
}

/// Parse a list of plain field names, such as the `sha256, group` of `eq(sha256, group)`
///
/// # Arguments
///
/// * `list` - The list's tokens
fn idents(list: &syn::MetaList) -> syn::Result<Vec<syn::Ident>> {
    let names = list.parse_args_with(Punctuated::<syn::Ident, Token![,]>::parse_terminated)?;
    Ok(names.into_iter().collect())
}

/// Parse a literal integer value, such as the `1` of `limit = 1`
///
/// # Arguments
///
/// * `value` - The name-value meta
fn int<T: std::str::FromStr>(value: &syn::MetaNameValue) -> syn::Result<T>
where
    T::Err: std::fmt::Display,
{
    match &value.value {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(lit),
            ..
        }) => lit.base10_parse(),
        other => Err(syn::Error::new_spanned(
            other,
            "expected an integer literal",
        )),
    }
}

/// Parse a select's, delete's, or update's arguments, keeping restricted columns in
/// declaration order
///
/// # Arguments
///
/// * `attr` - The field's statement attribute
/// * `kind` - The statement kind, `select`, `delete`, `update`, or `census`
fn statement_args(attr: &syn::Attribute, kind: &str) -> syn::Result<StatementArgs> {
    let metas = attr.parse_args_with(Punctuated::<syn::Meta, Token![,]>::parse_terminated)?;
    let mut args = StatementArgs::default();
    for meta in metas {
        let Some(key) = meta.path().get_ident().map(IdentExt::unraw) else {
            return Err(syn::Error::new_spanned(
                meta.path(),
                "unknown statement argument",
            ));
        };
        let key = key.to_string();
        // each argument is valid only for the kinds that use it
        let allowed = match key.as_str() {
            "project" | "group_by" => kind == "select" || kind == "census",
            _ if kind == "census" => false,
            "eq" => true,
            "in" => kind != "update",
            "range"
            | "token_range"
            | "distinct"
            | "paged"
            | "exists"
            | "limit"
            | "chunk"
            | "per_partition_limit" => kind == "select",
            "by_key" => kind != "select",
            "partition" | "prefix" => kind == "delete",
            "set" => kind == "update",
            _ => false,
        };
        if !allowed {
            return Err(syn::Error::new_spanned(
                meta.path(),
                format!("`{key}` isn't an argument of `{kind}` statements"),
            ));
        }
        match (key.as_str(), &meta) {
            ("eq" | "in", syn::Meta::List(list)) => {
                let op = if key == "eq" { Op::Eq } else { Op::In };
                args.restricted.extend(
                    idents(list)?
                        .into_iter()
                        .map(|field| Restricted { field, op }),
                );
            }
            ("group_by", syn::Meta::List(list)) => args.group_by.extend(idents(list)?),
            ("project", syn::Meta::NameValue(value)) => match &value.value {
                syn::Expr::Path(path) => args.project = Some(path.path.clone()),
                other => return Err(syn::Error::new_spanned(other, "expected a projection path")),
            },
            ("exists", syn::Meta::Path(_)) => args.exists = true,
            ("token_range", syn::Meta::Path(_)) => args.token_range = true,
            ("distinct", syn::Meta::Path(_)) => args.distinct = true,
            ("paged", syn::Meta::Path(_)) => args.paged = true,
            ("limit", syn::Meta::NameValue(value)) => args.limit = Some(int(value)?),
            ("chunk", syn::Meta::NameValue(value)) => args.chunk = Some(int(value)?),
            ("per_partition_limit", syn::Meta::NameValue(value)) => {
                args.per_partition_limit = Some(int(value)?);
            }
            ("per_partition_limit", syn::Meta::Path(_)) => args.per_partition_bound = true,
            ("range", syn::Meta::List(list)) => {
                let parts = idents(list)?;
                // a range names its column and its comparison
                let op = match parts.get(1).map(ToString::to_string).as_deref() {
                    Some("ge") => RangeOp::Ge,
                    Some("gt") => RangeOp::Gt,
                    Some("le") => RangeOp::Le,
                    Some("lt") => RangeOp::Lt,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            list,
                            "a range takes a field and one of ge, gt, le, or lt",
                        ));
                    }
                };
                args.restricted.push(Restricted {
                    field: parts[0].clone(),
                    op: Op::Range(op),
                });
            }
            ("set", syn::Meta::List(list)) => args.set.extend(idents(list)?),
            ("by_key", syn::Meta::Path(_)) => args.coverage = Some(Coverage::Key),
            ("partition", syn::Meta::Path(_)) => args.coverage = Some(Coverage::Partition),
            ("prefix", syn::Meta::Path(_)) => args.coverage = Some(Coverage::Prefix),
            _ => {
                return Err(syn::Error::new_spanned(
                    &meta,
                    format!("malformed `{key}` argument"),
                ));
            }
        }
    }
    Ok(args)
}

/// Get a field's statement attribute, if it has one
///
/// # Arguments
///
/// * `field` - The queries struct's field
fn statement_attr(field: &syn::Field) -> Option<(String, &syn::Attribute)> {
    field.attrs.iter().find_map(|attr| {
        let name = attr.path().get_ident()?.to_string();
        KINDS.contains(&name.as_str()).then_some((name, attr))
    })
}

/// Get a field's doc attributes
///
/// # Arguments
///
/// * `field` - The field to read
pub(super) fn doc_attrs(field: &syn::Field) -> Vec<syn::Attribute> {
    field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .cloned()
        .collect()
}

/// The paths and names every generated statement of one queries struct shares
pub(super) struct Context<'a> {
    /// The queries struct's name
    pub(super) queries: &'a syn::Ident,
    /// The table or view struct
    pub(super) object: &'a syn::Path,
    /// The generated code's paths
    pub(super) paths: &'a Paths,
}

impl Context<'_> {
    /// The const expression giving a field's position among the object's fields
    ///
    /// # Arguments
    ///
    /// * `field` - The field naming the column
    pub(super) fn index(&self, field: &syn::Ident) -> TokenStream {
        let object = self.object;
        let schema = &self.paths.schema;
        let private = &self.paths.private;
        let name = field.to_string();
        quote! {
            { #private::field_index(
                <#object as #schema::ScyllaObject>::FIELDS,
                #name,
                <#object as #schema::ScyllaObject>::NAME,
            ) }
        }
    }

    /// The `ColumnAt` impl of the column a field names
    ///
    /// # Arguments
    ///
    /// * `field` - The field naming the column
    pub(super) fn column(&self, field: &syn::Ident) -> TokenStream {
        let object = self.object;
        let schema = &self.paths.schema;
        let index = self.index(field);
        quote! { <#object as #schema::ColumnAt<#index>> }
    }

    /// The object's definition
    pub(super) fn def(&self) -> TokenStream {
        let object = self.object;
        let schema = &self.paths.schema;
        quote! { <#object as #schema::ScyllaObject>::DEF }
    }

    /// The label naming a statement in check failures
    ///
    /// # Arguments
    ///
    /// * `field` - The statement's field
    pub(super) fn label(&self, field: &syn::Ident) -> String {
        format!("{}.{field}", self.queries)
    }
}

/// The parameter name of a restricted column: the field's name, or its plural for a list
///
/// # Arguments
///
/// * `restricted` - The restricted column
fn param_name(restricted: &Restricted) -> syn::Ident {
    match restricted.op {
        Op::Eq | Op::Range(_) => restricted.field.clone(),
        Op::In => format_ident!("{}s", restricted.field),
    }
}

/// Build the `Restriction` of each restricted column, in declaration order
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `restricted` - The restricted columns, in declaration order
fn restriction_ops(ctx: &Context<'_>, restricted: &[Restricted]) -> Vec<TokenStream> {
    let schema = &ctx.paths.schema;
    // name each op's variant in the runtime's enum
    restricted
        .iter()
        .map(|item| match item.op {
            Op::Eq => quote! { #schema::Restriction::Eq },
            Op::In => quote! { #schema::Restriction::In },
            Op::Range(RangeOp::Ge) => quote! { #schema::Restriction::Ge },
            Op::Range(RangeOp::Gt) => quote! { #schema::Restriction::Gt },
            Op::Range(RangeOp::Le) => quote! { #schema::Restriction::Le },
            Op::Range(RangeOp::Lt) => quote! { #schema::Restriction::Lt },
        })
        .collect()
}

/// The pieces of a statement with restrictions that its methods and text share
struct Restrictions {
    /// The const items declaring each restricted column's name and op, and their key order
    consts: TokenStream,
    /// The parameters of the method running one execution
    params: Vec<TokenStream>,
    /// The values bound, in declaration order, as `let` statements
    values: Vec<TokenStream>,
    /// The names of the bound values, in declaration order
    value_names: Vec<syn::Ident>,
    /// The parameter names, in declaration order
    arg_names: Vec<syn::Ident>,
    /// The restricted columns that take `IN` lists, with their parameter names
    lists: Vec<(syn::Ident, TokenStream)>,
    /// The names of values bound after the restrictions, such as a bound limit
    trailing: Vec<syn::Ident>,
}

/// Build the shared pieces of a statement's restrictions
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `restricted` - The restricted columns, in declaration order
fn restrictions(ctx: &Context<'_>, restricted: &[Restricted]) -> Restrictions {
    let private = &ctx.paths.private;
    let schema = &ctx.paths.schema;
    let object = ctx.object;
    let def = ctx.def();
    let count = restricted.len();
    let names = restricted.iter().map(|item| {
        let column = ctx.column(&item.field);
        quote! { #column::COLUMN }
    });
    let ops = restriction_ops(ctx, restricted);
    // the restricted columns and their key order are consts of each function that needs them
    let consts = quote! {
        const COLUMNS: [&str; #count] = [#(#names),*];
        const OPS: [#schema::Restriction; #count] = [#(#ops),*];
        const ORDER: [usize; #count] = #private::key_order(&#def, COLUMNS);
    };
    let mut params = Vec::with_capacity(count);
    let mut values = Vec::with_capacity(count);
    let mut value_names = Vec::with_capacity(count);
    let mut arg_names = Vec::with_capacity(count);
    let mut lists = Vec::new();
    for (position, item) in restricted.iter().enumerate() {
        let param = param_name(item);
        let column = ctx.column(&item.field);
        let index = ctx.index(&item.field);
        let value = format_ident!("__value{position}");
        // a list binds the column's owned values, a single value the column's bind type
        match item.op {
            Op::Eq | Op::Range(_) => {
                params.push(quote! { #param: #column::Bind<'_> });
                values.push(quote! {
                    let #value = #private::BindValue::<#object, #index>(&#param);
                });
            }
            Op::In => {
                params.push(quote! { #param: &[#column::Owned] });
                values.push(quote! {
                    let #value = #private::BindList::<#object, #index>(#param, ::core::marker::PhantomData);
                });
                lists.push((param.clone(), quote! { #column::COLUMN }));
            }
        }
        value_names.push(value);
        arg_names.push(param);
    }
    Restrictions {
        consts,
        params,
        values,
        value_names,
        arg_names,
        lists,
        trailing: Vec::new(),
    }
}

/// What one execution of a select returns
#[derive(Clone, Copy, PartialEq, Eq)]
enum Returns {
    /// Every row, as a projection
    Rows,
    /// The first row, if any
    First,
    /// Whether any row matched
    Any,
}

/// Build the function giving a statement's `IN` lists for planning its chunks
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The statement's field
/// * `parts` - The statement's restrictions
/// * `chunk` - The most values bound per execution of each list
fn in_lists_fn(
    ctx: &Context<'_>,
    field: &syn::Ident,
    parts: &Restrictions,
    chunk: usize,
) -> TokenStream {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let def = ctx.def();
    let lists_fn = format_ident!("{field}_in_lists");
    let count = parts.lists.len();
    // each list's chunk plan entry takes its kind from the column's role
    let list_entries = parts
        .lists
        .iter()
        .enumerate()
        .map(|(position, (_, column))| {
            quote! {
                #schema::InList {
                    len: lens[#position],
                    kind: #private::in_kind(&#def, #column),
                    chunk: #chunk,
                }
            }
        });
    let lists_doc = format!(
        "The `IN` lists of `{field}` for lists of these lengths, in parameter order, for \
         planning its chunks"
    );
    quote! {
        #[doc = #lists_doc]
        ///
        /// # Arguments
        ///
        /// * `lens` - The length of each list
        #[must_use]
        pub fn #lists_fn(lens: [usize; #count]) -> [#schema::InList; #count] {
            [#(#list_entries),*]
        }
    }
}

/// Build the methods that run a statement whose `IN` lists are chunked: the single-execution
/// method, a method running every chunk, and the chunk plan's lists
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The statement's field
/// * `parts` - The statement's restrictions
/// * `chunk` - The most values bound per execution of each list
/// * `returns` - What one execution returns, with its tokens, for the combined methods
/// * `docs` - The statement's doc attributes
fn chunked_methods(
    ctx: &Context<'_>,
    field: &syn::Ident,
    parts: &Restrictions,
    chunk: usize,
    returns: (Option<Returns>, &TokenStream),
    docs: &[syn::Attribute],
) -> TokenStream {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let chunk_method = format_ident!("{field}_chunk");
    let lists_fn = format_ident!("{field}_in_lists");
    let any_method = format_ident!("{field}_any");
    let params = &parts.params;
    let session = quote! { session: &#private::scylla::client::session::Session };
    let list_names = parts.lists.iter().map(|(name, _)| name).collect::<Vec<_>>();
    // every call binds one range of each list and the same single values
    let call_args = parts
        .arg_names
        .iter()
        .map(
            |arg| match parts.lists.iter().position(|(name, _)| name == arg) {
                Some(position) => quote! { &#arg[ranges[#position].clone()] },
                None => quote! { #arg },
            },
        )
        .collect::<Vec<_>>();
    let (returns, ty) = returns;
    let lists = in_lists_fn(ctx, field, parts, chunk);
    let plan = quote! {
        let plan = #schema::plan_in_chunks(&Self::#lists_fn([#(#list_names.len()),*]));
    };
    let error = quote! { #schema::QueryError };
    let combined = match returns {
        // a delete runs every chunk in turn
        None => quote! {
            #(#docs)*
            ///
            /// Each `IN` list is bound in chunks, run one after another.
            #[allow(clippy::too_many_arguments)]
            pub async fn #field(&self, #session, #(#params),*) -> Result<(), #error> {
                #plan
                for ranges in &plan {
                    self.#chunk_method(session, #(#call_args),*).await?;
                }
                Ok(())
            }
        },
        Some(Returns::Rows) => quote! {
            #(#docs)*
            ///
            /// Each `IN` list is bound in chunks, run one after another, and their rows are
            /// concatenated.
            #[allow(clippy::too_many_arguments)]
            pub async fn #field(&self, #session, #(#params),*) -> Result<#ty, #error> {
                #plan
                #schema::run_chunks(&plan, |ranges| self.#chunk_method(session, #(#call_args),*)).await
            }
            #(#docs)*
            ///
            /// Returns whether any row matched, running chunks only until one finds a row.
            #[allow(clippy::too_many_arguments)]
            pub async fn #any_method(&self, #session, #(#params),*) -> Result<bool, #error> {
                #plan
                #schema::run_chunks_any(&plan, |ranges| {
                    let rows = self.#chunk_method(session, #(#call_args),*);
                    async move { Ok(!rows.await?.is_empty()) }
                })
                .await
            }
        },
        Some(Returns::Any) => quote! {
            #(#docs)*
            ///
            /// Each `IN` list is bound in chunks, run only until one finds a row.
            #[allow(clippy::too_many_arguments)]
            pub async fn #field(&self, #session, #(#params),*) -> Result<bool, #error> {
                #plan
                #schema::run_chunks_any(&plan, |ranges| self.#chunk_method(session, #(#call_args),*)).await
            }
        },
        Some(Returns::First) => quote! {
            #(#docs)*
            ///
            /// Each `IN` list is bound in chunks, run only until one finds a row.
            #[allow(clippy::too_many_arguments)]
            pub async fn #field(&self, #session, #(#params),*) -> Result<#ty, #error> {
                #plan
                for ranges in &plan {
                    if let Some(row) = self.#chunk_method(session, #(#call_args),*).await? {
                        return Ok(Some(row));
                    }
                }
                Ok(None)
            }
        },
    };
    quote! {
        #lists
        #combined
    }
}

/// The columns an update sets, bound before its restrictions
#[derive(Default)]
struct Sets {
    /// The method parameters, one per set column
    params: Vec<TokenStream>,
    /// The values bound, as `let` statements
    values: Vec<TokenStream>,
    /// The names of the bound values
    names: Vec<syn::Ident>,
}

/// Build the parameters and bound values of the columns an update sets
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `fields` - The fields naming the set columns
fn sets(ctx: &Context<'_>, fields: &[syn::Ident]) -> Sets {
    let private = &ctx.paths.private;
    let object = ctx.object;
    let mut sets = Sets::default();
    for (position, field) in fields.iter().enumerate() {
        let column = ctx.column(field);
        let index = ctx.index(field);
        let value = format_ident!("__set{position}");
        // name each set parameter apart from the restriction parameters, which take the column names
        let param = format_ident!("new_{}", field);
        // each set column takes its column's bind type
        sets.params.push(quote! { #param: #column::Bind<'_> });
        sets.values.push(quote! {
            let #value = #private::BindValue::<#object, #index>(&#param);
        });
        sets.names.push(value);
    }
    sets
}

/// How a statement's single execution reads its result
#[derive(Clone, Copy)]
enum Read<'a> {
    /// Run unpaged and return nothing
    Nothing,
    /// Run unpaged and read the result with these tokens, which name it `result`
    Unpaged(&'a TokenStream),
    /// Page through the rows, returning a stream of this projection
    Paged(&'a syn::Path),
}

/// Build the method running one execution of a statement with restrictions, binding every
/// value in key order; with `IN` lists it's the `_chunk` method, which binds them as given
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `ident` - The statement's field
/// * `parts` - The statement's restrictions
/// * `sets` - The columns an update sets, bound before the restrictions
/// * `docs` - The statement's doc attributes
/// * `ty` - What the execution returns
/// * `read` - How the result is read
fn single_method(
    ctx: &Context<'_>,
    ident: &syn::Ident,
    parts: &Restrictions,
    sets: &Sets,
    docs: &[syn::Attribute],
    ty: &TokenStream,
    read: Read<'_>,
) -> TokenStream {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let consts = &parts.consts;
    let params = sets.params.iter().chain(&parts.params);
    let values = sets.values.iter().chain(&parts.values);
    let value_names = &parts.value_names;
    let set_names = &sets.names;
    let trailing = &parts.trailing;
    let count = value_names.len();
    // the restrictions bind in key order, after any columns an update sets and before any
    // bound limit
    let binds = if set_names.is_empty() && trailing.is_empty() {
        quote! { #private::in_key_order::<#count>(ORDER, [#(&#value_names),*]) }
    } else {
        quote! {
            #private::chain_binds(
                &[#(&#set_names),*],
                #private::in_key_order::<#count>(ORDER, [#(&#value_names),*]),
                &[#(&#trailing),*],
            )
        }
    };
    // a statement with lists runs once per chunk through its `_chunk` method
    let (run, run_doc) = if parts.lists.is_empty() {
        (ident.clone(), TokenStream::new())
    } else {
        (
            format_ident!("{ident}_chunk"),
            quote! {
                ///
                /// Runs once with the lists as given, so they must fit in one statement; callers
                /// that keep their own fan-out use this.
            },
        )
    };
    let finish = match read {
        Read::Unpaged(read) => quote! {
            let result = session.execute_unpaged(&self.#ident, &binds[..]).await?;
            #read
        },
        Read::Paged(project) => quote! {
            let pager = session.execute_iter(self.#ident.clone(), &binds[..]).await?;
            Ok(pager.rows_stream::<#project>()?)
        },
        Read::Nothing => quote! {
            session.execute_unpaged(&self.#ident, &binds[..]).await?;
            Ok(())
        },
    };
    quote! {
        #(#docs)*
        #run_doc
        #[allow(clippy::too_many_arguments)]
        pub async fn #run(
            &self,
            session: &#private::scylla::client::session::Session,
            #(#params),*
        ) -> Result<#ty, #schema::QueryError> {
            #consts
            // bind each value through its column, then put them in key order
            #(#values)*
            let binds = #binds;
            #finish
        }
    }
}

/// Decide what one execution of a select returns from its projection and existence flags
///
/// # Arguments
///
/// * `args` - The select's arguments
/// * `attr` - The select's attribute, for errors
fn select_returns(args: &StatementArgs, attr: &syn::Attribute) -> syn::Result<Returns> {
    match (&args.project, args.exists, args.limit) {
        (None, true, None) => Ok(Returns::Any),
        (Some(_), true, None) | (Some(_), false, Some(1)) => Ok(Returns::First),
        (Some(_), false, None) => Ok(Returns::Rows),
        (None, false, _) => Err(syn::Error::new_spanned(
            attr,
            "a select needs `project = <Row>` or `exists`",
        )),
        _ => Err(syn::Error::new_spanned(
            attr,
            "a select takes `exists` or `limit = 1`, not both, and other limits aren't generated \
             yet",
        )),
    }
}

/// Build a select's compile-time checks of its restrictions, grouping, and projection
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `ident` - The statement's field
/// * `args` - The select's arguments
/// * `group_by` - The grouped columns' name expressions
fn select_checks(
    ctx: &Context<'_>,
    ident: &syn::Ident,
    args: &StatementArgs,
    group_by: &[TokenStream],
) -> TokenStream {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let def = ctx.def();
    let label = ctx.label(ident);
    let restricted_columns = args.restricted.iter().map(|item| {
        let column = ctx.column(&item.field);
        quote! { #column::COLUMN }
    });
    let restricted_ops = restriction_ops(ctx, &args.restricted);
    let object = ctx.object;
    // the projection's columns must exist in, and be redacted as, the object read
    let project_check = args.project.as_ref().map(|project| {
        quote! {
            const _: () = #private::assert_selects_from(<#project as #schema::Projection>::COLUMNS, &#def, #label);
            const _: () = #private::assert_selects_redacted(
                <#project as #schema::Projection>::COLUMNS,
                <#project as #schema::Projection>::REDACTED,
                &#def,
                <#object as #schema::ScyllaObject>::REDACTED,
                #label,
            );
        }
    });
    quote! {
        const _: () = #private::assert_restrictions(
            &#def,
            &[#(#restricted_columns),*],
            &[#(#restricted_ops),*],
            #private::Coverage::Prefix,
            #label,
        );
        const _: () = #private::assert_group_by(&#def, &[#(#group_by),*], #label);
        #project_check
    }
}

/// Build one select
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the statement is declared on
/// * `attr` - The field's `#[select(...)]` attribute
// the select's arguments, text, checks, and methods are built together
#[allow(clippy::too_many_lines)]
fn select(ctx: &Context<'_>, field: &syn::Field, attr: &syn::Attribute) -> syn::Result<Statement> {
    let args = statement_args(attr, "select")?;
    let ident = field.ident.clone().expect("named fields have names");
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let object = ctx.object;
    let docs = doc_attrs(field);
    // a token-range select reads a slice of the ring instead of restricting keys
    if args.token_range {
        return token_range_select(ctx, field, attr, &args);
    }
    if args.restricted.is_empty() {
        return Err(syn::Error::new_spanned(
            attr,
            "a select restricts at least one key column",
        ));
    }
    // a distinct select enumerates partitions, which only a token-range select does
    if args.distinct {
        return Err(syn::Error::new_spanned(
            attr,
            "`distinct` is generated only with `token_range`",
        ));
    }
    let returns = select_returns(&args, attr)?;
    // a paged select streams every row of a projection
    let paged = match (&args.project, returns, args.paged) {
        (_, _, false) => None,
        (Some(project), Returns::Rows, true) => Some(project),
        _ => {
            return Err(syn::Error::new_spanned(
                attr,
                "`paged` streams the rows of a projection, without `exists` or a limit",
            ));
        }
    };
    // an existence check reads one row, unless a per-partition limit already bounds it
    let limit = match (returns, args.per_partition_limit) {
        (Returns::Rows, _) | (Returns::Any, Some(_)) => quote! { None },
        (Returns::First, _) | (Returns::Any, None) => quote! { Some(1) },
    };
    let per_partition = match args.per_partition_limit {
        Some(limit) => quote! { Some(#limit) },
        None => quote! { None },
    };
    let per_partition_bound = args.per_partition_bound;
    let mut parts = restrictions(ctx, &args.restricted);
    // a bound per-partition limit is the last bind
    if args.per_partition_bound {
        let name = format_ident!("per_partition_limit");
        let value = format_ident!("__limit");
        parts.params.push(quote! { #name: i32 });
        parts.values.push(quote! { let #value = #name; });
        parts.arg_names.push(name);
        parts.trailing.push(value);
    }
    let consts = &parts.consts;
    // an existence check selects its restricted columns, a projection its own columns
    let columns = match &args.project {
        Some(project) => quote! { <#project as #schema::Projection>::COLUMNS },
        None => quote! { &COLUMNS },
    };
    let group_by = args
        .group_by
        .iter()
        .map(|field| {
            let column = ctx.column(field);
            quote! { #column::COLUMN }
        })
        .collect::<Vec<_>>();
    let cql_fn = format_ident!("__cql_{ident}");
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            #consts
            // render the restrictions in key order
            let restrictions = ORDER.map(|index| (COLUMNS[index], OPS[index]));
            #private::select_cql(
                ns,
                <#object as #schema::ScyllaObject>::NAME,
                #columns,
                &restrictions,
                &[#(#group_by),*],
                #schema::Limits {
                    per_partition: #per_partition,
                    per_partition_bound: #per_partition_bound,
                    limit: #limit,
                },
            )
        }
    };
    // check the restrictions, grouping, and projection against the object
    let checks = select_checks(ctx, &ident, &args, &group_by);
    // the single execution binds every value in key order
    let ty = match (&args.project, returns, paged) {
        (Some(project), Returns::Rows, Some(_)) => {
            quote! { #private::scylla::client::pager::TypedRowStream<#project> }
        }
        (Some(project), Returns::Rows, None) => quote! { Vec<#schema::RowResult<#project>> },
        (Some(project), Returns::First, _) => quote! { Option<#project> },
        _ => quote! { bool },
    };
    let read_rows = match returns {
        Returns::Rows => quote! { #private::rows(result) },
        Returns::First => quote! { #private::first_row(result) },
        Returns::Any => quote! { #private::any_row(result) },
    };
    let read = match paged {
        Some(project) => Read::Paged(project),
        None => Read::Unpaged(&read_rows),
    };
    let single = single_method(ctx, &ident, &parts, &Sets::default(), &docs, &ty, read);
    // a paged select leaves chunking its lists to the caller, which streams each chunk
    let chunked = if parts.lists.is_empty() || paged.is_some() {
        TokenStream::new()
    } else {
        let chunk = args.chunk.unwrap_or(100);
        chunked_methods(ctx, &ident, &parts, chunk, (Some(returns), &ty), &docs)
    };
    Ok(Statement {
        field: ident,
        docs: docs.clone(),
        cql: quote! { #cql },
        methods: quote! { #single #chunked },
        items: checks,
        grouped: None,
    })
}

/// Build the method reading one token range of a statement: every row, or a stream of them
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `ident` - The statement's field
/// * `docs` - The statement's doc attributes
/// * `project` - The projection the statement reads
/// * `paged` - Whether to page through the rows instead of reading them at once
fn token_range_method(
    ctx: &Context<'_>,
    ident: &syn::Ident,
    docs: &[syn::Attribute],
    project: &syn::Path,
    paged: bool,
) -> TokenStream {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    // the range's first and last token bind in the order the statement takes them
    let (ty, run) = if paged {
        (
            quote! { #private::scylla::client::pager::TypedRowStream<#project> },
            quote! {
                let pager = session.execute_iter(self.#ident.clone(), (start, end)).await?;
                Ok(pager.rows_stream::<#project>()?)
            },
        )
    } else {
        (
            quote! { Vec<#schema::RowResult<#project>> },
            quote! {
                let result = session.execute_unpaged(&self.#ident, (start, end)).await?;
                #private::rows(result)
            },
        )
    };
    quote! {
        #(#docs)*
        ///
        /// # Arguments
        ///
        /// * `session` - The Scylla session to execute on
        /// * `start` - The first token of the range
        /// * `end` - The last token of the range
        pub async fn #ident(
            &self,
            session: &#private::scylla::client::session::Session,
            start: i64,
            end: i64,
        ) -> Result<#ty, #schema::QueryError> {
            #run
        }
    }
}

/// Build the checks of a projection read by a token-range statement against its object
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `ident` - The statement's field
/// * `project` - The projection the statement reads
fn projection_checks(ctx: &Context<'_>, ident: &syn::Ident, project: &syn::Path) -> TokenStream {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let object = ctx.object;
    let def = ctx.def();
    let label = ctx.label(ident);
    quote! {
        const _: () = #private::assert_selects_from(<#project as #schema::Projection>::COLUMNS, &#def, #label);
        const _: () = #private::assert_selects_redacted(
            <#project as #schema::Projection>::COLUMNS,
            <#project as #schema::Projection>::REDACTED,
            &#def,
            <#object as #schema::ScyllaObject>::REDACTED,
            #label,
        );
    }
}

/// Build one select over a token range of the partition key, as scans and partition
/// enumerations read
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the statement is declared on
/// * `attr` - The field's `#[select(...)]` attribute
/// * `args` - The select's arguments
fn token_range_select(
    ctx: &Context<'_>,
    field: &syn::Field,
    attr: &syn::Attribute,
    args: &StatementArgs,
) -> syn::Result<Statement> {
    let ident = field.ident.clone().expect("named fields have names");
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let def = ctx.def();
    let docs = doc_attrs(field);
    // a token range is the statement's only restriction, and it reads a projection
    let plain = args.restricted.is_empty()
        && args.group_by.is_empty()
        && !args.exists
        && args.limit.is_none()
        && args.chunk.is_none()
        && args.per_partition_limit.is_none()
        && !args.per_partition_bound;
    let (Some(project), true) = (&args.project, plain) else {
        return Err(syn::Error::new_spanned(
            attr,
            "a `token_range` select takes `project = <Row>`, and optionally `distinct` and `paged`",
        ));
    };
    // render the statement's text over the projection's columns, distinct when asked
    let distinct = args.distinct;
    let cql_fn = format_ident!("__cql_{ident}");
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            #private::token_range_cql(ns, &#def, <#project as #schema::Projection>::COLUMNS, #distinct, &[])
        }
    };
    // a distinct select reads only partition key columns
    let label = ctx.label(&ident);
    let distinct_check = distinct.then(|| {
        quote! {
            const _: () = #private::assert_distinct(<#project as #schema::Projection>::COLUMNS, &#def, #label);
        }
    });
    // check the projection against the object, and build the method reading one range
    let checks = projection_checks(ctx, &ident, project);
    let methods = token_range_method(ctx, &ident, &docs, project, args.paged);
    Ok(Statement {
        field: ident,
        docs,
        cql,
        methods,
        items: quote! { #checks #distinct_check },
        grouped: None,
    })
}

/// Build one census: a count of the rows of each partition in a token range, grouped by the
/// partition key or by the columns named
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the statement is declared on
/// * `attr` - The field's `#[census(...)]` attribute
fn census(ctx: &Context<'_>, field: &syn::Field, attr: &syn::Attribute) -> syn::Result<Statement> {
    let args = statement_args(attr, "census")?;
    let ident = field.ident.clone().expect("named fields have names");
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let def = ctx.def();
    let docs = doc_attrs(field);
    // a census reads its counts into a projection
    let Some(project) = &args.project else {
        return Err(syn::Error::new_spanned(
            attr,
            "a census takes `project = <Row>` and optionally `group_by(...)`",
        ));
    };
    // name the column of each grouped field
    let group_by = args
        .group_by
        .iter()
        .map(|field| {
            let column = ctx.column(field);
            quote! { #column::COLUMN }
        })
        .collect::<Vec<_>>();
    // with no grouped columns the census groups by the whole partition key
    let groups = if group_by.is_empty() {
        quote! { #def.partition_key() }
    } else {
        quote! { vec![#(#group_by),*] }
    };
    // render the census text over the projection's columns and the grouped columns
    let cql_fn = format_ident!("__cql_{ident}");
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            let group_by: Vec<&str> = #groups;
            #private::token_range_cql(ns, &#def, <#project as #schema::Projection>::COLUMNS, false, &group_by)
        }
    };
    // check the projection against the object, the grouped columns against the key, and the
    // selected columns against the grouped ones
    let label = ctx.label(&ident);
    let projection = projection_checks(ctx, &ident, project);
    let checks = quote! {
        #projection
        const _: () = #private::assert_group_by(&#def, &[#(#group_by),*], #label);
        const _: () = #private::assert_census_columns(
            <#project as #schema::Projection>::COLUMNS,
            &#def,
            &[#(#group_by),*],
            #label,
        );
    };
    // build the method reading one range's counts
    let methods = token_range_method(ctx, &ident, &docs, project, false);
    Ok(Statement {
        field: ident,
        docs,
        cql,
        methods,
        items: checks,
        grouped: None,
    })
}

/// Build one delete
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the statement is declared on
/// * `attr` - The field's `#[delete(...)]` attribute
fn delete(ctx: &Context<'_>, field: &syn::Field, attr: &syn::Attribute) -> syn::Result<Statement> {
    let args = statement_args(attr, "delete")?;
    let ident = field.ident.clone().expect("named fields have names");
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let object = ctx.object;
    let def = ctx.def();
    let docs = doc_attrs(field);
    // a delete says which part of the key it covers
    let Some(coverage) = args.coverage else {
        return Err(syn::Error::new_spanned(
            attr,
            "a delete takes `by_key`, `partition`, or `prefix`",
        ));
    };
    let parts = restrictions(ctx, &args.restricted);
    let consts = &parts.consts;
    let cql_fn = format_ident!("__cql_{ident}");
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            #consts
            // render the restrictions in key order
            let restrictions = ORDER.map(|index| (COLUMNS[index], OPS[index]));
            #private::delete_cql(ns, <#object as #schema::ScyllaObject>::NAME, &restrictions)
        }
    };
    let label = ctx.label(&ident);
    let coverage = match coverage {
        Coverage::Key => quote! { #private::Coverage::Key },
        Coverage::Partition => quote! { #private::Coverage::Partition },
        Coverage::Prefix => quote! { #private::Coverage::Prefix },
    };
    let restricted_columns = args.restricted.iter().map(|item| {
        let column = ctx.column(&item.field);
        quote! { #column::COLUMN }
    });
    let restricted_ops = restriction_ops(ctx, &args.restricted);
    let checks = quote! {
        const _: () = #private::assert_restrictions(
            &#def,
            &[#(#restricted_columns),*],
            &[#(#restricted_ops),*],
            #coverage,
            #label,
        );
    };
    let single = single_method(
        ctx,
        &ident,
        &parts,
        &Sets::default(),
        &docs,
        &quote! { () },
        Read::Nothing,
    );
    let none = TokenStream::new();
    let chunked = if parts.lists.is_empty() {
        TokenStream::new()
    } else {
        chunked_methods(ctx, &ident, &parts, 100, (None, &none), &docs)
    };
    Ok(Statement {
        field: ident,
        docs,
        cql,
        methods: quote! { #single #chunked },
        items: checks,
        grouped: None,
    })
}

/// The name of a subset insert's bind struct: the table's name without its `Table` suffix,
/// then the statement's name unless it's `insert`, then `Insert`
///
/// # Arguments
///
/// * `table` - The table struct
/// * `field` - The statement's field
fn insert_struct_name(table: &syn::Path, field: &syn::Ident) -> syn::Ident {
    let table = table
        .segments
        .last()
        .map_or_else(String::new, |segment| segment.ident.to_string());
    let base = table.strip_suffix("Table").unwrap_or(&table);
    // name the struct after its statement when a table has more than the plain insert
    let statement = if field == "insert" {
        String::new()
    } else {
        field
            .to_string()
            .split('_')
            .map(|part| {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(chars).collect::<String>()
                })
            })
            .collect()
    };
    format_ident!("{base}{statement}Insert")
}

/// Build a subset insert's bind struct, with a `Debug` that hides redacted columns
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `name` - The bind struct's name
/// * `fields` - The fields naming the inserted columns
fn insert_struct(ctx: &Context<'_>, name: &syn::Ident, fields: &[syn::Ident]) -> TokenStream {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let object = ctx.object;
    let label = name.to_string();
    let doc = "The columns one insert writes, borrowed for binding; the other columns of the \
               row keep their values";
    let defs = fields.iter().map(|field| {
        let column = ctx.column(field);
        let field_doc = format!("The value written to the column the `{field}` field names");
        quote! {
            #[doc = #field_doc]
            pub #field: #column::Bind<'a>
        }
    });
    // print each value, or a placeholder for a column the table redacts
    let prints = fields.iter().map(|field| {
        let column = ctx.column(field);
        let label = field.to_string();
        quote! {
            if #private::is_redacted(<#object as #schema::ScyllaObject>::REDACTED, #column::COLUMN) {
                out.field(#label, &::core::format_args!("<redacted>"));
            } else {
                out.field(#label, &self.#field);
            }
        }
    });
    quote! {
        #[doc = #doc]
        #[derive(Clone, Copy)]
        pub struct #name<'a> {
            #(#defs,)*
        }
        impl ::core::fmt::Debug for #name<'_> {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                let mut out = f.debug_struct(#label);
                #(#prints)*
                out.finish()
            }
        }
    }
}

/// How an insert sets the TTL of the row it writes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ttl {
    /// The table's default TTL stays in effect
    Default,
    /// `USING TTL 0`: the row never expires
    Zero,
    /// `USING TTL ?`: the TTL is bound with the row
    Bind,
}

impl Ttl {
    /// Parse an insert's `ttl = none | zero | bind`
    ///
    /// # Arguments
    ///
    /// * `ttl` - The value given to `ttl`
    fn parse(ttl: &syn::Ident) -> darling::Result<Self> {
        match ttl.to_string().as_str() {
            "none" => Ok(Ttl::Default),
            "zero" => Ok(Ttl::Zero),
            "bind" => Ok(Ttl::Bind),
            other => Err(darling::Error::custom(format!(
                "unknown `ttl = {other}`; use none, zero, or bind"
            ))
            .with_span(ttl)),
        }
    }

    /// The clause the statement ends with
    fn clause(self) -> &'static str {
        match self {
            Ttl::Default => "",
            Ttl::Zero => " USING TTL 0",
            Ttl::Bind => " USING TTL ?",
        }
    }

    /// The method parameter and its argument doc, for a bound TTL
    fn param(self) -> (TokenStream, TokenStream) {
        match self {
            Ttl::Bind => (
                quote! { ttl: i32, },
                quote! { #[doc = " * `ttl` - The seconds until the row expires, or 0 for never"] },
            ),
            Ttl::Default | Ttl::Zero => (TokenStream::new(), TokenStream::new()),
        }
    }
}

/// Build one subset insert, which binds a generated struct of only its columns
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `ident` - The statement's field
/// * `docs` - The statement's doc attributes
/// * `fields` - The fields naming the inserted columns
/// * `ttl` - How the insert sets the row's TTL
fn subset_insert(
    ctx: &Context<'_>,
    ident: syn::Ident,
    docs: Vec<syn::Attribute>,
    fields: &[syn::Ident],
    ttl: Ttl,
) -> Statement {
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let object = ctx.object;
    let def = ctx.def();
    let name = insert_struct_name(object, &ident);
    let count = fields.len();
    let columns = fields
        .iter()
        .map(|field| {
            let column = ctx.column(field);
            quote! { #column::COLUMN }
        })
        .collect::<Vec<_>>();
    let cql_fn = format_ident!("__cql_{ident}");
    let clause = ttl.clause();
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            let insert = #private::insert_cql(ns, <#object as #schema::ScyllaObject>::NAME, &[#(#columns),*]);
            format!("{insert}{}", #clause)
        }
    };
    // each value binds through its column, in the order the statement lists them
    let values = fields.iter().enumerate().map(|(position, field)| {
        let index = ctx.index(field);
        let value = format_ident!("__value{position}");
        quote! { let #value = #private::BindValue::<#object, #index>(&row.#field); }
    });
    let mut names = (0..count)
        .map(|position| format_ident!("__value{position}"))
        .collect::<Vec<_>>();
    // a bound TTL follows the values
    let (ttl_param, ttl_doc) = ttl.param();
    if ttl == Ttl::Bind {
        names.push(format_ident!("ttl"));
    }
    let bound = names.len();
    let methods = quote! {
        #(#docs)*
        ///
        /// # Arguments
        ///
        /// * `session` - The Scylla session to execute on
        /// * `row` - The columns to write
        #ttl_doc
        pub async fn #ident(
            &self,
            session: &#private::scylla::client::session::Session,
            row: &#name<'_>,
            #ttl_param
        ) -> Result<#private::scylla::response::query_result::QueryResult, #schema::QueryError> {
            // bind each column's value in the statement's order
            #(#values)*
            let binds: [&(dyn #private::scylla::serialize::value::SerializeValue + Sync); #bound] =
                [#(&#names),*];
            Ok(session.execute_unpaged(&self.#ident, &binds[..]).await?)
        }
    };
    let label = ctx.label(&ident);
    let bind_struct = insert_struct(ctx, &name, fields);
    let items = quote! {
        const _: () = #private::assert_insert_columns(&#def, &[#(#columns),*], #label);
        #bind_struct
    };
    Statement {
        field: ident,
        docs,
        cql,
        methods,
        items,
        grouped: None,
    }
}

/// Build one insert: a full-row insert binding the table's borrowed row, or a subset insert
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the statement is declared on
/// * `attr` - The field's `#[insert(...)]` attribute
fn insert(
    ctx: &Context<'_>,
    field: &syn::Field,
    attr: &syn::Attribute,
) -> darling::Result<Statement> {
    let args = InsertArgs::from_meta(&attr.meta)?;
    let ident = field.ident.clone().expect("named fields have names");
    let ttl = Ttl::parse(&args.ttl)?;
    let docs = doc_attrs(field);
    if let ColumnsArg::List(fields) = &args.columns {
        return Ok(subset_insert(ctx, ident, docs, fields, ttl));
    }
    let table = ctx.object;
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let cql_fn = format_ident!("__cql_{ident}");
    let clause = ttl.clause();
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            let insert = #schema::with_keyspace(<#table as #schema::TableStatements>::INSERT, ns);
            format!("{insert}{}", #clause)
        }
    };
    // a bound TTL answers the statement's `[ttl]` marker beside the row's columns
    let (ttl_param, ttl_doc) = ttl.param();
    let execute = match ttl {
        Ttl::Bind => quote! {
            session.execute_unpaged(&self.#ident, &#schema::WithTtl { row, ttl }).await?
        },
        Ttl::Default | Ttl::Zero => quote! { session.execute_unpaged(&self.#ident, row).await? },
    };
    let methods = quote! {
        #(#docs)*
        ///
        /// # Arguments
        ///
        /// * `session` - The Scylla session to execute on
        /// * `row` - The row to write
        #ttl_doc
        pub async fn #ident(
            &self,
            session: &#private::scylla::client::session::Session,
            row: &<#table as #schema::TableStatements>::Ref<'_>,
            #ttl_param
        ) -> Result<#private::scylla::response::query_result::QueryResult, #schema::QueryError> {
            Ok(#execute)
        }
    };
    Ok(Statement {
        field: ident,
        docs,
        cql,
        methods,
        items: TokenStream::new(),
        grouped: None,
    })
}

/// Build one update of named columns by key
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the statement is declared on
/// * `attr` - The field's `#[update(...)]` attribute
fn update(ctx: &Context<'_>, field: &syn::Field, attr: &syn::Attribute) -> syn::Result<Statement> {
    let args = statement_args(attr, "update")?;
    let ident = field.ident.clone().expect("named fields have names");
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let object = ctx.object;
    let def = ctx.def();
    let docs = doc_attrs(field);
    // an update names the columns it sets and the whole key of the row it changes
    if args.set.is_empty() || args.coverage != Some(Coverage::Key) {
        return Err(syn::Error::new_spanned(
            attr,
            "an update takes `set(...)` and `by_key` with every key column",
        ));
    }
    let parts = restrictions(ctx, &args.restricted);
    let consts = &parts.consts;
    let set_columns = args
        .set
        .iter()
        .map(|field| {
            let column = ctx.column(field);
            quote! { #column::COLUMN }
        })
        .collect::<Vec<_>>();
    let cql_fn = format_ident!("__cql_{ident}");
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            #consts
            // render the restrictions in key order
            let restrictions = ORDER.map(|index| (COLUMNS[index], OPS[index]));
            #private::update_cql(
                ns,
                <#object as #schema::ScyllaObject>::NAME,
                &[#(#set_columns),*],
                &restrictions,
            )
        }
    };
    let label = ctx.label(&ident);
    let restricted_columns = args.restricted.iter().map(|item| {
        let column = ctx.column(&item.field);
        quote! { #column::COLUMN }
    });
    let restricted_ops = restriction_ops(ctx, &args.restricted);
    let checks = quote! {
        const _: () = #private::assert_restrictions(
            &#def,
            &[#(#restricted_columns),*],
            &[#(#restricted_ops),*],
            #private::Coverage::Key,
            #label,
        );
        const _: () = #private::assert_set_columns(&#def, &[#(#set_columns),*], #label);
    };
    let single = single_method(
        ctx,
        &ident,
        &parts,
        &sets(ctx, &args.set),
        &docs,
        &quote! { () },
        Read::Nothing,
    );
    Ok(Statement {
        field: ident,
        docs,
        cql,
        methods: single,
        items: checks,
        grouped: None,
    })
}

/// One typed bind of a hand-written statement: `name: Type`
struct RawBind {
    /// The parameter name
    name: syn::Ident,
    /// The parameter's type
    ty: syn::Type,
}

impl syn::parse::Parse for RawBind {
    /// Parse `name: Type`
    ///
    /// # Arguments
    ///
    /// * `input` - The tokens to parse
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let name = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty = input.parse()?;
        Ok(RawBind { name, ty })
    }
}

/// The parsed arguments of a `#[query("...", binds(...))]` field
struct RawArgs {
    /// The statement's text, with a `{ns}` placeholder for the keyspace
    cql: syn::LitStr,
    /// The binds, in the order the statement's markers take them
    binds: Vec<RawBind>,
}

impl syn::parse::Parse for RawArgs {
    /// Parse the statement's text and its `binds(...)`
    ///
    /// # Arguments
    ///
    /// * `input` - The tokens to parse
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let cql = input.parse()?;
        let mut binds = Vec::new();
        // the text may be followed by its binds
        while input.parse::<Option<Token![,]>>()?.is_some() && !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            if key != "binds" {
                return Err(syn::Error::new_spanned(
                    &key,
                    format!("`{key}` isn't an argument of `query` statements"),
                ));
            }
            let content;
            syn::parenthesized!(content in input);
            binds.extend(Punctuated::<RawBind, Token![,]>::parse_terminated(
                &content,
            )?);
        }
        Ok(RawArgs { cql, binds })
    }
}

/// Build one hand-written statement, prepared and typed like the generated ones
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the statement is declared on
/// * `attr` - The field's `#[query(...)]` attribute
fn raw(ctx: &Context<'_>, field: &syn::Field, attr: &syn::Attribute) -> syn::Result<Statement> {
    let args: RawArgs = attr.parse_args()?;
    let ident = field.ident.clone().expect("named fields have names");
    let schema = &ctx.paths.schema;
    let private = &ctx.paths.private;
    let docs = doc_attrs(field);
    let text = &args.cql;
    let cql_fn = format_ident!("__cql_{ident}");
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            #schema::with_keyspace(#text, ns)
        }
    };
    let params = args.binds.iter().map(|bind| {
        let (name, ty) = (&bind.name, &bind.ty);
        quote! { #name: #ty }
    });
    let names = args.binds.iter().map(|bind| &bind.name);
    let methods = quote! {
        #(#docs)*
        #[allow(clippy::too_many_arguments)]
        pub async fn #ident(
            &self,
            session: &#private::scylla::client::session::Session,
            #(#params),*
        ) -> Result<(), #schema::QueryError> {
            // bind each value in the order the statement's markers take them
            session.execute_unpaged(&self.#ident, &(#(#names,)*)).await?;
            Ok(())
        }
    };
    Ok(Statement {
        field: ident,
        docs,
        cql,
        methods,
        items: TokenStream::new(),
        grouped: None,
    })
}

/// Build the statement one field declares
///
/// # Arguments
///
/// * `target` - Whether the struct targets a table or a view
/// * `ctx` - The queries struct's context
/// * `field` - The field
fn statement(target: Target, ctx: &Context<'_>, field: &syn::Field) -> darling::Result<Statement> {
    let Some((kind, attr)) = statement_attr(field) else {
        return Err(darling::Error::custom(
            "each queries field needs a statement attribute: insert, select, update, delete, \
             cursor, census, or query",
        )
        .with_span(field));
    };
    // a view is written only through its base table
    if target == Target::View && WRITES.contains(&kind.as_str()) {
        return Err(darling::Error::custom(format!(
            "`{kind}` statements can't be declared on a view; a view is written through its base \
             table"
        ))
        .with_span(attr));
    }
    match kind.as_str() {
        "insert" => insert(ctx, field, attr),
        "select" => select(ctx, field, attr).map_err(darling::Error::from),
        "delete" => delete(ctx, field, attr).map_err(darling::Error::from),
        "update" => update(ctx, field, attr).map_err(darling::Error::from),
        "query" => raw(ctx, field, attr).map_err(darling::Error::from),
        "cursor" => super::cursor::cursor(ctx, field, attr).map_err(darling::Error::from),
        "census" => census(ctx, field, attr).map_err(darling::Error::from),
        _ => Err(
            darling::Error::custom(format!("`{kind}` statements aren't generated yet"))
                .with_span(attr),
        ),
    }
}

/// Expand a queries attribute
///
/// # Arguments
///
/// * `target` - Whether the struct targets a table or a view
/// * `args` - The attribute's arguments
/// * `input` - The struct the attribute is on
pub fn expand(target: Target, args: TokenStream, input: TokenStream) -> TokenStream {
    match build(target, args, input) {
        Ok(tokens) => tokens,
        Err(errors) => errors.write_errors(),
    }
}

/// Build the rewritten queries struct
///
/// # Arguments
///
/// * `target` - Whether the struct targets a table or a view
/// * `args` - The attribute's arguments
/// * `input` - The struct the attribute is on
// the struct, its preparation, and its statement listing are generated together
#[allow(clippy::too_many_lines)]
fn build(target: Target, args: TokenStream, input: TokenStream) -> darling::Result<TokenStream> {
    let args = QueriesAttrs::from_list(&NestedMeta::parse_meta_list(args)?)?;
    let item: syn::ItemStruct = syn::parse2(input)?;
    // each macro names the object kind it targets
    let object = match (target, &args.table, &args.view) {
        (Target::Table, Some(table), None) => table.clone(),
        (Target::View, None, Some(view)) => view.clone(),
        (Target::Table, ..) => {
            return Err(
                darling::Error::custom("table_queries takes `table = <Table>`")
                    .with_span(&item.ident),
            );
        }
        (Target::View, ..) => {
            return Err(
                darling::Error::custom("view_queries takes `view = <View>`").with_span(&item.ident)
            );
        }
    };
    let paths = Paths::new(args.krate.as_ref());
    let private = &paths.private;
    let syn::Fields::Named(fields) = &item.fields else {
        return Err(
            darling::Error::custom("a queries struct needs named fields").with_span(&item.ident),
        );
    };
    let ctx = Context {
        queries: &item.ident,
        object: &object,
        paths: &paths,
    };
    // build every field's statement, collecting every error
    let mut errors = darling::Error::accumulator();
    let statements = fields
        .named
        .iter()
        .filter_map(|field| errors.handle(statement(target, &ctx, field)))
        .collect::<Vec<_>>();
    errors.finish()?;
    let struct_attrs = &item.attrs;
    let vis = &item.vis;
    let ident = &item.ident;
    // a field holds one prepared statement, or a generated type of several
    let field_defs = statements.iter().map(|statement| {
        let field = &statement.field;
        let docs = &statement.docs;
        let ty = match &statement.grouped {
            Some(grouped) => {
                let ty = &grouped.ty;
                quote! { #ty }
            }
            None => quote! { #private::scylla::statement::prepared::PreparedStatement },
        };
        quote! {
            #(#docs)*
            pub #field: #ty
        }
    });
    let prepares = statements.iter().map(|statement| {
        let field = &statement.field;
        match &statement.grouped {
            Some(grouped) => {
                let ty = &grouped.ty;
                quote! { let #field = #ty::prepare(session, ns).await?; }
            }
            None => {
                let cql_fn = format_ident!("__cql_{field}");
                quote! { let #field = session.prepare(Self::#cql_fn(ns)).await?; }
            }
        }
    });
    let listed = statements.iter().map(|statement| {
        let field = &statement.field;
        match &statement.grouped {
            Some(grouped) => {
                let ty = &grouped.ty;
                quote! { statements.extend(#ty::statements(ns)); }
            }
            None => {
                let name = field.to_string();
                let cql_fn = format_ident!("__cql_{field}");
                quote! { statements.push((#name, Self::#cql_fn(ns))); }
            }
        }
    });
    // each single statement's text is public, for callers that prepare it on its own
    let texts = statements
        .iter()
        .filter(|statement| statement.grouped.is_none())
        .map(|statement| {
            let field = &statement.field;
            let cql_fn = format_ident!("__cql_{field}");
            let text_fn = format_ident!("{field}_cql");
            let doc = format!(
                "The text of `{field}` for a keyspace, for callers that prepare it on its own"
            );
            quote! {
                #[doc = #doc]
                ///
                /// # Arguments
                ///
                /// * `ns` - The keyspace the statement targets
                #[must_use]
                pub fn #text_fn(ns: &str) -> String {
                    Self::#cql_fn(ns)
                }
            }
        });
    let names = statements.iter().map(|statement| &statement.field);
    let cqls = statements.iter().map(|statement| &statement.cql);
    let methods = statements.iter().map(|statement| &statement.methods);
    let items = statements.iter().map(|statement| &statement.items);
    Ok(quote! {
        #(#struct_attrs)*
        #vis struct #ident {
            #(#field_defs,)*
        }
        impl #ident {
            /// Prepare every statement of this struct
            ///
            /// # Arguments
            ///
            /// * `session` - The Scylla session to prepare on
            /// * `ns` - The keyspace the statements target
            pub async fn prepare(
                session: &#private::scylla::client::session::Session,
                ns: &str,
            ) -> Result<Self, #private::scylla::errors::PrepareError> {
                #(#prepares)*
                Ok(Self { #(#names,)* })
            }
            /// Every statement's text for a keyspace, named after its field, in declaration
            /// order
            ///
            /// # Arguments
            ///
            /// * `ns` - The keyspace the statements target
            #[must_use]
            pub fn statements(ns: &str) -> Vec<(&'static str, String)> {
                let mut statements = Vec::new();
                #(#listed)*
                statements
            }
            #(#cqls)*
            #(#texts)*
            #(#methods)*
        }
        #(#items)*
    })
}
