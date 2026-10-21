//! `#[cursor(...)]` fields of a queries struct: the statements of one cursor and a type holding
//! them that implements its family's statements trait
//!
//! Like the other statements, a cursor names columns by field and reaches each through the
//! object's `ColumnAt` impl. The comparisons and any `ORDER BY` follow the declared clustering
//! order, which is only known from the object's definition, so the statement text is rendered
//! from it when the statements are prepared.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::Token;
use syn::ext::IdentExt;
use syn::punctuated::Punctuated;

use super::queries::{Context, Grouped, Statement, doc_attrs};

/// The cursor families a `#[cursor(...)]` field can declare
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Rows bucketed by time, listed newest first across groups
    Time,
    /// The clustering key of one partition at a time
    Simple,
    /// Groups sorted by a clustering column
    Grouped,
    /// Whether any row with a key exists in some buckets
    Exists,
}

/// The limit a cursor's statements end with
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitKind {
    /// Pulls end with `PER PARTITION LIMIT ?`
    PerPartition,
    /// Statements end with `LIMIT ?`
    Limit,
    /// No limit
    None,
}

/// How a cursor filters one partition column besides its roles
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterOp {
    /// `column = ?`
    Eq,
    /// `column IN ?`
    In,
}

/// One extra partition filter of a time cursor
struct Filter {
    /// The field naming the column
    field: syn::Ident,
    /// How the column is filtered
    op: FilterOp,
}

/// The parsed arguments of a `#[cursor(...)]` field
#[derive(Default)]
struct CursorArgs {
    /// The cursor family
    kind: Option<Kind>,
    /// The projection the statements read
    project: Option<syn::Path>,
    /// The partition column a time or grouped cursor fans out over
    group_by: Option<syn::Ident>,
    /// The extra partition filters, in declaration order
    filters: Vec<Filter>,
    /// The year partition column
    year: Option<syn::Ident>,
    /// The bucket partition column
    bucket: Option<syn::Ident>,
    /// The clustering column a time cursor ranges over
    range: Option<syn::Ident>,
    /// The clustering columns after the range column, or the column a simple cursor resumes
    /// after
    tie: Vec<syn::Ident>,
    /// The partition column a simple cursor lists within
    partition: Option<syn::Ident>,
    /// The clustering column a grouped cursor sorts by
    sort: Option<syn::Ident>,
    /// The column that tells apart a grouped cursor's rows sharing a sort value
    tie_breaker: Option<syn::Ident>,
    /// The column an exists cursor checks for
    key: Option<syn::Ident>,
    /// The order a time cursor consumes its range column in, when it isn't the declared one
    order: Option<(bool, Span)>,
    /// The limit the statements end with
    limit: Option<LimitKind>,
    /// Whether a time cursor pages through its rows without a limit
    paged: Option<Span>,
}

/// Read a name-value argument's value as one identifier
///
/// # Arguments
///
/// * `value` - The argument
fn ident_value(value: &syn::MetaNameValue) -> syn::Result<syn::Ident> {
    match &value.value {
        syn::Expr::Path(path) => path
            .path
            .get_ident()
            .map(IdentExt::unraw)
            .ok_or_else(|| syn::Error::new_spanned(path, "expected a field name")),
        other => Err(syn::Error::new_spanned(other, "expected a field name")),
    }
}

/// Read the field names of a `tie = id` or `tie = [target, direction]` argument
///
/// # Arguments
///
/// * `value` - The argument
fn tie_value(value: &syn::MetaNameValue) -> syn::Result<Vec<syn::Ident>> {
    match &value.value {
        syn::Expr::Array(array) => array
            .elems
            .iter()
            .map(|elem| match elem {
                syn::Expr::Path(path) => path
                    .path
                    .get_ident()
                    .map(IdentExt::unraw)
                    .ok_or_else(|| syn::Error::new_spanned(path, "expected a field name")),
                other => Err(syn::Error::new_spanned(other, "expected a field name")),
            })
            .collect(),
        _ => Ok(vec![ident_value(value)?]),
    }
}

/// Parse a `#[cursor(...)]` attribute, keeping its filters in declaration order
///
/// # Arguments
///
/// * `attr` - The field's attribute
// one arm per argument keeps the vocabulary in one place
#[allow(clippy::too_many_lines)]
fn cursor_args(attr: &syn::Attribute) -> syn::Result<CursorArgs> {
    // read the attribute's comma-separated arguments
    let metas = attr.parse_args_with(Punctuated::<syn::Meta, Token![,]>::parse_terminated)?;
    let mut args = CursorArgs::default();
    for meta in metas {
        // every argument is named by a plain identifier
        let Some(key) = meta.path().get_ident().map(IdentExt::unraw) else {
            return Err(syn::Error::new_spanned(
                meta.path(),
                "unknown cursor argument",
            ));
        };
        // set the argument's value, refusing an unknown argument or the wrong form of a known one
        match (key.to_string().as_str(), &meta) {
            ("kind", syn::Meta::NameValue(value)) => {
                let kind = ident_value(value)?;
                args.kind = Some(match kind.to_string().as_str() {
                    "time" => Kind::Time,
                    "simple" => Kind::Simple,
                    "grouped" => Kind::Grouped,
                    "exists" => Kind::Exists,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            kind,
                            "a cursor's kind is time, simple, grouped, or exists",
                        ));
                    }
                });
            }
            ("project", syn::Meta::NameValue(value)) => match &value.value {
                syn::Expr::Path(path) => args.project = Some(path.path.clone()),
                other => return Err(syn::Error::new_spanned(other, "expected a projection path")),
            },
            ("eq" | "in", syn::Meta::List(list)) => {
                let op = if key == "eq" {
                    FilterOp::Eq
                } else {
                    FilterOp::In
                };
                let names =
                    list.parse_args_with(Punctuated::<syn::Ident, Token![,]>::parse_terminated)?;
                args.filters
                    .extend(names.into_iter().map(|field| Filter { field, op }));
            }
            ("group_by", syn::Meta::NameValue(value)) => args.group_by = Some(ident_value(value)?),
            ("year", syn::Meta::NameValue(value)) => args.year = Some(ident_value(value)?),
            ("bucket", syn::Meta::NameValue(value)) => args.bucket = Some(ident_value(value)?),
            ("range", syn::Meta::NameValue(value)) => args.range = Some(ident_value(value)?),
            ("tie", syn::Meta::NameValue(value)) => args.tie = tie_value(value)?,
            ("partition", syn::Meta::NameValue(value)) => {
                args.partition = Some(ident_value(value)?);
            }
            ("sort", syn::Meta::NameValue(value)) => args.sort = Some(ident_value(value)?),
            ("tie_breaker", syn::Meta::NameValue(value)) => {
                args.tie_breaker = Some(ident_value(value)?);
            }
            ("key", syn::Meta::NameValue(value)) => args.key = Some(ident_value(value)?),
            ("order", syn::Meta::NameValue(value)) => match &value.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(lit),
                    ..
                }) => {
                    let descending = match lit.value().as_str() {
                        "desc" => true,
                        "asc" => false,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                lit,
                                "a cursor's order is \"asc\" or \"desc\"",
                            ));
                        }
                    };
                    args.order = Some((descending, lit.span()));
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected \"asc\" or \"desc\"",
                    ));
                }
            },
            ("limit", syn::Meta::NameValue(value)) => {
                let limit = ident_value(value)?;
                args.limit = Some(match limit.to_string().as_str() {
                    "per_partition" => LimitKind::PerPartition,
                    "limit" => LimitKind::Limit,
                    "none" => LimitKind::None,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            limit,
                            "a cursor's limit is per_partition, limit, or none",
                        ));
                    }
                });
            }
            ("paged", syn::Meta::Path(path)) => {
                args.paged = Some(syn::spanned::Spanned::span(path));
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &meta,
                    format!("`{key}` isn't an argument of `cursor` fields"),
                ));
            }
        }
    }
    Ok(args)
}

/// Get a required argument, or fail naming it
///
/// # Arguments
///
/// * `value` - The argument, if given
/// * `name` - The argument's name
/// * `attr` - The attribute, for the error
fn required<'a, T>(value: Option<&'a T>, name: &str, attr: &syn::Attribute) -> syn::Result<&'a T> {
    value.ok_or_else(|| syn::Error::new_spanned(attr, format!("this cursor needs `{name}`")))
}

/// Turn a field name such as `list_details` into a type name part such as `ListDetails`
///
/// # Arguments
///
/// * `name` - The field name
fn camel(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect::<String>()
            })
        })
        .collect()
}

/// The paths and names one cursor's generated code shares
struct CursorCtx<'a> {
    /// The queries struct's context
    ctx: &'a Context<'a>,
    /// The cursor's field
    field: syn::Ident,
    /// The generated type holding the cursor's statements
    ty: syn::Ident,
    /// The projection the statements read
    project: syn::Path,
    /// The label naming the cursor in check failures
    label: String,
}

impl CursorCtx<'_> {
    /// The CQL name of the column a field names
    ///
    /// # Arguments
    ///
    /// * `field` - The field
    fn name(&self, field: &syn::Ident) -> TokenStream {
        let column = self.ctx.column(field);
        quote! { #column::COLUMN }
    }

    /// The value of the column a field names in a row of the projection
    ///
    /// # Arguments
    ///
    /// * `field` - The field naming the column
    /// * `row` - The row expression
    fn row_value(&self, field: &syn::Ident, row: &TokenStream) -> TokenStream {
        let schema = &self.ctx.paths.schema;
        let private = &self.ctx.paths.private;
        let project = &self.project;
        let name = self.name(field);
        let label = &self.label;
        quote! {
            <#project as #schema::ProjectedAt<{
                #private::column_index(<#project as #schema::Projection>::COLUMNS, #name, #label)
            }>>::value(#row)
        }
    }

    /// A value bound to the column a field names, given as the column's own type
    ///
    /// # Arguments
    ///
    /// * `field` - The field naming the column
    /// * `value` - A reference to the value
    fn bind_owned(&self, field: &syn::Ident, value: &TokenStream) -> TokenStream {
        let private = &self.ctx.paths.private;
        let object = self.ctx.object;
        let index = self.ctx.index(field);
        quote! { #private::BindOwned::<#object, #index>(#value) }
    }

    /// A list of values bound to the column a field names
    ///
    /// # Arguments
    ///
    /// * `field` - The field naming the column
    /// * `values` - A slice of the values
    fn bind_list(&self, field: &syn::Ident, values: &TokenStream) -> TokenStream {
        let private = &self.ctx.paths.private;
        let object = self.ctx.object;
        let index = self.ctx.index(field);
        quote! { #private::BindList::<#object, #index>(#values, ::core::marker::PhantomData) }
    }

    /// The column's own type for the column a field names
    ///
    /// # Arguments
    ///
    /// * `field` - The field naming the column
    fn owned(&self, field: &syn::Ident) -> TokenStream {
        let column = self.ctx.column(field);
        quote! { #column::Owned }
    }

    /// The const checks of the projection against the object it reads
    fn project_checks(&self) -> TokenStream {
        let schema = &self.ctx.paths.schema;
        let private = &self.ctx.paths.private;
        let object = self.ctx.object;
        let project = &self.project;
        let def = self.ctx.def();
        let label = &self.label;
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

    /// The statement-holding type, its preparation, and its statement listing
    ///
    /// # Arguments
    ///
    /// * `docs` - The cursor field's doc attributes
    /// * `statements` - Each statement's field, doc, and associated function rendering it
    fn holder(
        &self,
        docs: &[syn::Attribute],
        statements: &[(syn::Ident, &str, TokenStream)],
    ) -> TokenStream {
        let private = &self.ctx.paths.private;
        let ty = &self.ty;
        let doc = format!("The statements of `{}::{}`", self.ctx.queries, self.field);
        let fields = statements.iter().map(|(name, doc, _)| {
            quote! {
                #[doc = #doc]
                pub #name: #private::scylla::statement::prepared::PreparedStatement
            }
        });
        let cqls = statements.iter().map(|(_, _, cql)| cql);
        let prepares = statements.iter().map(|(name, _, _)| {
            let cql_fn = format_ident!("__cql_{name}");
            quote! { let #name = session.prepare(Self::#cql_fn(ns)).await?; }
        });
        let names = statements.iter().map(|(name, _, _)| name);
        let listed = statements.iter().map(|(name, _, _)| {
            let text = name.to_string();
            let cql_fn = format_ident!("__cql_{name}");
            quote! { (#text, Self::#cql_fn(ns)) }
        });
        quote! {
            #[doc = #doc]
            #(#docs)*
            pub struct #ty {
                #(#fields,)*
            }
            impl #ty {
                /// Prepare every statement of this cursor
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
                /// Every statement's text for a keyspace, named after its field
                ///
                /// # Arguments
                ///
                /// * `ns` - The keyspace the statements target
                #[must_use]
                pub fn statements(ns: &str) -> Vec<(&'static str, String)> {
                    vec![#(#listed),*]
                }
                #(#cqls)*
            }
        }
    }
}

/// A tuple type, or the single type itself when there is only one
///
/// # Arguments
///
/// * `types` - The element types
fn tuple_type(types: &[TokenStream]) -> TokenStream {
    match types {
        [single] => single.clone(),
        _ => quote! { (#(#types,)*) },
    }
}

/// The expression reading each element of a tuple, or the value itself when there is only one
///
/// # Arguments
///
/// * `base` - The tuple expression
/// * `len` - The number of elements
fn tuple_access(base: &TokenStream, len: usize) -> Vec<TokenStream> {
    // a single value is the referenced value itself
    if len == 1 {
        return vec![quote! { (*#base) }];
    }
    (0..len)
        .map(|index| {
            let index = syn::Index::from(index);
            quote! { #base.#index }
        })
        .collect()
}

/// The consumption order a cursor names, as a const expression
///
/// # Arguments
///
/// * `args` - The cursor's arguments
/// * `cursor` - The cursor's context
fn order_override(args: &CursorArgs, cursor: &CursorCtx<'_>) -> TokenStream {
    let schema = &cursor.ctx.paths.schema;
    match args.order {
        Some((true, _)) => quote! { Some(#schema::Order::Desc) },
        Some((false, _)) => quote! { Some(#schema::Order::Asc) },
        None => quote! { None },
    }
}

/// The parts of a time cursor shared by its statements and its trait impl
struct TimeParts<'a> {
    /// The fanned-out partition column
    group: &'a syn::Ident,
    /// The year partition column
    year: &'a syn::Ident,
    /// The bucket partition column
    bucket: &'a syn::Ident,
    /// The range clustering column
    range: &'a syn::Ident,
    /// The tie clustering columns
    ties: &'a [syn::Ident],
    /// The extra partition filters
    filters: &'a [Filter],
    /// The consumption order the cursor names, as a const expression
    order: TokenStream,
    /// The limit the statements end with
    limit: LimitKind,
    /// The statements reading tied rows, one per tie column: the first compares the first tie
    /// column, and each later one fixes the tie columns before its own
    ties_stmts: Vec<syn::Ident>,
    /// The statement reading a window of buckets
    pull_stmt: syn::Ident,
}

/// Render a time cursor's statements: its tie queries and its pull
///
/// # Arguments
///
/// * `cursor` - The cursor's context
/// * `parts` - The cursor's columns and options
fn time_cql(cursor: &CursorCtx<'_>, parts: &TimeParts<'_>) -> (Vec<TokenStream>, TokenStream) {
    let schema = &cursor.ctx.paths.schema;
    let private = &cursor.ctx.paths.private;
    let object = cursor.ctx.object;
    let project = &cursor.project;
    let def = cursor.ctx.def();
    let order = &parts.order;
    let partition = [parts.group, parts.year, parts.bucket]
        .into_iter()
        .chain(parts.filters.iter().map(|filter| &filter.field))
        .map(|field| cursor.name(field))
        .collect::<Vec<_>>();
    let count = partition.len();
    // a pull reads a list of buckets and its `IN` filters as lists; a tie query one partition
    let pull_ops = [quote! { Eq }, quote! { Eq }, quote! { In }]
        .into_iter()
        .chain(parts.filters.iter().map(|filter| match filter.op {
            FilterOp::Eq => quote! { Eq },
            FilterOp::In => quote! { In },
        }))
        .map(|op| quote! { #schema::Restriction::#op })
        .collect::<Vec<_>>();
    let range = cursor.name(parts.range);
    let ties = parts
        .ties
        .iter()
        .map(|tie| cursor.name(tie))
        .collect::<Vec<_>>();
    let first_tie = &ties[0];
    let (pull_limit, ties_limit) = match parts.limit {
        LimitKind::PerPartition => (
            quote! { #schema::CursorLimitClause::PerPartition },
            quote! { #schema::CursorLimitClause::Limit },
        ),
        LimitKind::Limit => (
            quote! { #schema::CursorLimitClause::Limit },
            quote! { #schema::CursorLimitClause::Limit },
        ),
        LimitKind::None => (
            quote! { #schema::CursorLimitClause::None },
            quote! { #schema::CursorLimitClause::None },
        ),
    };
    let pull_fn = format_ident!("__cql_{}", parts.pull_stmt);
    // each tie query reads the rows from the tie onward that fix the tie columns before its own
    let ties_cqls = parts
        .ties_stmts
        .iter()
        .enumerate()
        .map(|(index, stmt)| {
            let ties_fn = format_ident!("__cql_{stmt}");
            quote! {
                fn #ties_fn(ns: &str) -> String {
                    const COLUMNS: [&str; #count] = [#(#partition),*];
                    const KEY_ORDER: [usize; #count] = #private::key_order(&#def, COLUMNS);
                    // every partition column is restricted to one value
                    let restrictions =
                        KEY_ORDER.map(|index| (COLUMNS[index], #schema::Restriction::Eq));
                    let ties = [#(#ties),*];
                    // read from the tie onward in the order the tie columns are consumed
                    let order = #private::tie_order(&#def, #range, #first_tie, #order);
                    let clustering = ::core::iter::once(format!("{} = ?", #range))
                        .chain(#private::tie_predicates(&ties, #index, order))
                        .collect::<Vec<_>>();
                    let order_by = #private::order_by(&#def, #range, &ties, #order);
                    #private::cursor_cql(
                        ns,
                        <#object as #schema::ScyllaObject>::NAME,
                        <#project as #schema::Projection>::COLUMNS,
                        &restrictions,
                        &clustering,
                        order_by.as_deref(),
                        #ties_limit,
                    )
                }
            }
        })
        .collect::<Vec<_>>();
    let pull_cql = quote! {
        fn #pull_fn(ns: &str) -> String {
            const COLUMNS: [&str; #count] = [#(#partition),*];
            const OPS: [#schema::Restriction; #count] = [#(#pull_ops),*];
            const KEY_ORDER: [usize; #count] = #private::key_order(&#def, COLUMNS);
            // render the partition restrictions in key order
            let restrictions = KEY_ORDER.map(|index| (COLUMNS[index], OPS[index]));
            let ties = [#(#ties),*];
            // read the window strictly between the two timestamps
            let clustering = [format!("{} < ?", #range), format!("{} > ?", #range)];
            let order_by = #private::order_by(&#def, #range, &ties, #order);
            #private::cursor_cql(
                ns,
                <#object as #schema::ScyllaObject>::NAME,
                <#project as #schema::Projection>::COLUMNS,
                &restrictions,
                &clustering,
                order_by.as_deref(),
                #pull_limit,
            )
        }
    };
    (ties_cqls, pull_cql)
}

/// The binds of a time cursor's two statements, as `let` statements and the names bound in
/// declaration order
struct TimeBinds {
    /// The values bound to a pull's partition columns
    pull: Vec<TokenStream>,
    /// The values bound to a tie query's partition columns
    ties: Vec<TokenStream>,
    /// The names of the partition values, in declaration order
    names: Vec<syn::Ident>,
}

/// Build the partition binds of a time cursor's statements
///
/// # Arguments
///
/// * `cursor` - The cursor's context
/// * `parts` - The cursor's columns and options
fn time_binds(cursor: &CursorCtx<'_>, parts: &TimeParts<'_>) -> TimeBinds {
    let extra_access = tuple_access(&quote! { extra }, parts.filters.len());
    let in_count = parts
        .filters
        .iter()
        .filter(|filter| filter.op == FilterOp::In)
        .count();
    let scope_access = tuple_access(&quote! { scope }, 1 + in_count);
    let names = (0..3 + parts.filters.len())
        .map(|position| format_ident!("__value{position}"))
        .collect::<Vec<_>>();
    // the group, year, and bucket come first
    let mut pull = vec![
        cursor.bind_owned(parts.group, &quote! { group }),
        cursor.bind_owned(parts.year, &quote! { &year }),
        cursor.bind_list(parts.bucket, &quote! { &__buckets }),
    ];
    let scope_group = &scope_access[0];
    let mut ties = vec![
        cursor.bind_owned(parts.group, &quote! { &#scope_group }),
        cursor.bind_owned(parts.year, &quote! { &year }),
        cursor.bind_owned(parts.bucket, &quote! { &__bucket }),
    ];
    // each filter binds from the listing's filters, and a tie query's `IN` filters from its
    // scope
    let mut in_index = 1;
    for (filter, access) in parts.filters.iter().zip(&extra_access) {
        match filter.op {
            FilterOp::Eq => {
                pull.push(cursor.bind_owned(&filter.field, &quote! { &#access }));
                ties.push(cursor.bind_owned(&filter.field, &quote! { &#access }));
            }
            FilterOp::In => {
                pull.push(cursor.bind_list(&filter.field, &quote! { &#access[..] }));
                let scoped = &scope_access[in_index];
                ties.push(cursor.bind_owned(&filter.field, &quote! { &#scoped }));
                in_index += 1;
            }
        }
    }
    let pull = names
        .iter()
        .zip(pull)
        .map(|(name, value)| quote! { let #name = #value; })
        .collect();
    let ties = names
        .iter()
        .zip(ties)
        .map(|(name, value)| quote! { let #name = #value; })
        .collect();
    TimeBinds { pull, ties, names }
}

/// Build a time cursor
///
/// # Arguments
///
/// * `cursor` - The cursor's context
/// * `args` - The cursor's arguments
/// * `attr` - The cursor's attribute, for errors
/// * `docs` - The cursor field's doc attributes
// a time cursor's statements, checks, and trait impl are generated together
#[allow(clippy::too_many_lines)]
fn time(
    cursor: &CursorCtx<'_>,
    args: &CursorArgs,
    attr: &syn::Attribute,
    docs: &[syn::Attribute],
) -> syn::Result<(TokenStream, TokenStream)> {
    let schema = &cursor.ctx.paths.schema;
    let private = &cursor.ctx.paths.private;
    let object = cursor.ctx.object;
    let project = &cursor.project;
    let def = cursor.ctx.def();
    let label = &cursor.label;
    if args.tie.is_empty() {
        return Err(syn::Error::new_spanned(attr, "this cursor needs `tie`"));
    }
    // a paged cursor reads every row of its window, so it takes no limit and no order
    let limit = args.limit.unwrap_or(LimitKind::PerPartition);
    if let Some(span) = args.paged {
        if args.tie.len() > 1 {
            return Err(syn::Error::new(
                span,
                "a `paged` cursor resumes after one tie column",
            ));
        }
        if let Some((_, order)) = args.order {
            let mut error = syn::Error::new(
                order,
                "a cursor with `order` runs unpaged, because Scylla can't page `ORDER BY` with \
                 `IN` on the partition key, so it can't also be `paged`",
            );
            error.combine(syn::Error::new(span, "`paged` is given here"));
            return Err(error);
        }
        if limit != LimitKind::None {
            return Err(syn::Error::new(
                span,
                "a `paged` cursor takes `limit = none`",
            ));
        }
        if args.filters.iter().any(|filter| filter.op == FilterOp::In) {
            return Err(syn::Error::new(
                span,
                "a `paged` cursor's filters are `eq` only",
            ));
        }
    } else if limit == LimitKind::None {
        return Err(syn::Error::new_spanned(
            attr,
            "a cursor without a limit must be `paged`",
        ));
    }
    let parts = TimeParts {
        group: required(args.group_by.as_ref(), "group_by", attr)?,
        year: required(args.year.as_ref(), "year", attr)?,
        bucket: required(args.bucket.as_ref(), "bucket", attr)?,
        range: required(args.range.as_ref(), "range", attr)?,
        ties: &args.tie,
        filters: &args.filters,
        order: order_override(args, cursor),
        limit,
        ties_stmts: std::iter::once(format_ident!("{}_ties", cursor.field))
            .chain(
                args.tie
                    .iter()
                    .take(args.tie.len().saturating_sub(1))
                    .map(|tie| format_ident!("{}_ties_{}", cursor.field, tie)),
            )
            .collect(),
        pull_stmt: format_ident!("{}_pull", cursor.field),
    };
    let order = &parts.order;
    let (ties_cqls, pull_cql) = time_cql(cursor, &parts);
    // the tie queries come first, then the pull
    let statements = parts
        .ties_stmts
        .iter()
        .zip(ties_cqls)
        .map(|(stmt, cql)| {
            (
                stmt.clone(),
                "Read the rows of one partition at a timestamp from a tie onward",
                cql,
            )
        })
        .chain(std::iter::once((
            parts.pull_stmt.clone(),
            "Read a window of buckets strictly between two timestamps",
            pull_cql,
        )))
        .collect::<Vec<_>>();
    let holder = cursor.holder(docs, &statements);
    // the checks name the cursor's columns
    let partition = [parts.group, parts.year, parts.bucket]
        .into_iter()
        .chain(args.filters.iter().map(|filter| &filter.field))
        .map(|field| cursor.name(field))
        .collect::<Vec<_>>();
    let range = cursor.name(parts.range);
    let ties = parts
        .ties
        .iter()
        .map(|tie| cursor.name(tie))
        .collect::<Vec<_>>();
    let newest_first = if args.paged.is_some() {
        TokenStream::new()
    } else {
        quote! { const _: () = #private::assert_newest_first(&#def, #range, #order, #label); }
    };
    let project_checks = cursor.project_checks();
    let checks = quote! {
        const _: () = #private::assert_cursor_partition(&#def, &[#(#partition),*], #label);
        const _: () = #private::assert_cursor_clustering(&#def, &[#range, #(#ties),*], #label);
        const _: () = #private::assert_tie_orders(&#def, #range, &[#(#ties),*], #order, #label);
        #newest_first
        #project_checks
    };
    // the associated types follow the declared columns
    let extra_types = args
        .filters
        .iter()
        .map(|filter| {
            let owned = cursor.owned(&filter.field);
            match filter.op {
                FilterOp::Eq => owned,
                FilterOp::In => quote! { Vec<#owned> },
            }
        })
        .collect::<Vec<_>>();
    let extra_ty = if extra_types.is_empty() {
        quote! { () }
    } else {
        tuple_type(&extra_types)
    };
    let group_ty = cursor.owned(parts.group);
    let tie_ty = tuple_type(
        &parts
            .ties
            .iter()
            .map(|tie| cursor.owned(tie))
            .collect::<Vec<_>>(),
    );
    let scope_fields = std::iter::once(parts.group)
        .chain(
            args.filters
                .iter()
                .filter(|filter| filter.op == FilterOp::In)
                .map(|filter| &filter.field),
        )
        .collect::<Vec<_>>();
    let scope_ty = tuple_type(
        &scope_fields
            .iter()
            .map(|field| cursor.owned(field))
            .collect::<Vec<_>>(),
    );
    let binds = time_binds(cursor, &parts);
    let names = &binds.names;
    let count = names.len();
    let pull_ops_count = count;
    let range_index = cursor.ctx.index(parts.range);
    let tie_access = tuple_access(&quote! { tie }, parts.ties.len());
    let tie_names = (0..parts.ties.len())
        .map(|position| format_ident!("__tie{position}"))
        .collect::<Vec<_>>();
    let tie_binds = parts
        .ties
        .iter()
        .zip(&tie_access)
        .zip(&tie_names)
        .map(|((tie, access), name)| {
            let bind = cursor.bind_owned(tie, &quote! { &#access });
            quote! { let #name = #bind; }
        })
        .collect::<Vec<_>>();
    let row = quote! { row };
    let range_value = cursor.row_value(parts.range, &row);
    let tie_values = parts
        .ties
        .iter()
        .map(|tie| {
            let value = cursor.row_value(tie, &row);
            quote! { ::core::clone::Clone::clone(#value) }
        })
        .collect::<Vec<_>>();
    let row_tie = tuple_type(&tie_values);
    let scope_values = scope_fields
        .iter()
        .map(|field| {
            let value = cursor.row_value(field, &row);
            quote! { ::core::clone::Clone::clone(#value) }
        })
        .collect::<Vec<_>>();
    let row_scope = tuple_type(&scope_values);
    // the group is always the scope's first column, and the whole scope when it is alone
    let scope_group = if scope_fields.len() == 1 {
        quote! { scope }
    } else {
        quote! { &scope.0 }
    };
    // compare ties column by column in their consumption order
    let left_ties = tuple_access(&quote! { left }, parts.ties.len());
    let right_ties = tuple_access(&quote! { right }, parts.ties.len());
    let tie_cmps = parts
        .ties
        .iter()
        .zip(left_ties.iter().zip(&right_ties))
        .map(|(tie, (left, right))| {
            let index = cursor.ctx.index(tie);
            let name = cursor.name(tie);
            quote! {
                #private::cmp_consumed::<#object, #index>(
                    &#left,
                    &#right,
                    #private::tie_order(&#def, #range, #name, #order),
                )
            }
        })
        .collect::<Vec<_>>();
    // compare scopes column by column in key order
    let scope_count = scope_fields.len();
    let left_scope = tuple_access(&quote! { left }, scope_count);
    let right_scope = tuple_access(&quote! { right }, scope_count);
    let scope_names = scope_fields
        .iter()
        .map(|field| cursor.name(field))
        .collect::<Vec<_>>();
    let scope_cmps = scope_fields
        .iter()
        .zip(left_scope.iter().zip(&right_scope))
        .map(|(field, (left, right))| {
            let index = cursor.ctx.index(field);
            quote! { #private::cmp_column::<#object, #index>(&#left, &#right) }
        })
        .collect::<Vec<_>>();
    // a `IN` filter's lists multiply the partitions each bucket reads
    let in_lens = args
        .filters
        .iter()
        .zip(tuple_access(&quote! { extra }, args.filters.len()))
        .filter(|(filter, _)| filter.op == FilterOp::In)
        .map(|(_, access)| quote! { #access.len() })
        .collect::<Vec<_>>();
    let pull_binds = &binds.pull;
    let tie_query_binds = &binds.ties;
    let ties_stmt = &parts.ties_stmts[0];
    // each tie query binds the tie columns up to its own
    let tie_runs = parts
        .ties_stmts
        .iter()
        .enumerate()
        .map(|(index, stmt)| {
            let fixed = &tie_names[..=index];
            quote! {
                let binds = #private::chain_binds(
                    &[],
                    #private::in_key_order::<#count>(KEY_ORDER, [#(&#names),*]),
                    &[&__at, #(&#fixed,)* &__limit],
                );
                let result = session.execute_unpaged(&self.#stmt, &binds[..]).await?;
                rows.extend(#private::rows(result)?);
            }
        })
        .collect::<Vec<_>>();
    let pull_stmt = &parts.pull_stmt;
    let ty = &cursor.ty;
    let session = quote! { #private::scylla::client::session::Session };
    let time = quote! { #private::chrono::DateTime<#private::chrono::Utc> };
    let partition_consts = quote! {
        const COLUMNS: [&str; #pull_ops_count] = [#(#partition),*];
        const KEY_ORDER: [usize; #pull_ops_count] = #private::key_order(&#def, COLUMNS);
    };
    let range_bind = |name: &str, value: &TokenStream| {
        let name = format_ident!("{name}");
        quote! { let #name = #private::BindOwned::<#object, #range_index>(#value); }
    };
    let before_bind = range_bind("__before", &quote! { &before });
    let after_bind = range_bind("__after", &quote! { &after });
    let at_bind = range_bind("__at", &quote! { &at });
    let common = quote! {
        fn row_time(row: &#project) -> #time {
            *#range_value
        }
        fn row_tie(row: &#project) -> Self::Tie {
            #row_tie
        }
    };
    let imp = if args.paged.is_some() {
        quote! {
            impl #schema::PagedTimeCursorStatements for #ty {
                type Extra = #extra_ty;
                type Group = #group_ty;
                type Row = #project;
                type Tie = #tie_ty;
                #[allow(clippy::too_many_arguments)]
                fn pull<'a>(
                    &'a self,
                    session: &'a #session,
                    extra: &'a Self::Extra,
                    group: &'a Self::Group,
                    year: i32,
                    buckets: &'a [#schema::Bucket],
                    before: #time,
                    after: #time,
                ) -> #schema::CursorFuture<'a, #private::scylla::client::pager::TypedRowStream<#project>> {
                    ::std::boxed::Box::pin(async move {
                        #partition_consts
                        // bind the partition values, then the window's two timestamps
                        let __buckets = #private::bucket_ints(buckets)?;
                        let _ = extra;
                        #(#pull_binds)*
                        #before_bind
                        #after_bind
                        let binds = #private::chain_binds(
                            &[],
                            #private::in_key_order::<#count>(KEY_ORDER, [#(&#names),*]),
                            &[&__before, &__after],
                        );
                        let pager = session.execute_iter(self.#pull_stmt.clone(), &binds[..]).await?;
                        Ok(pager.rows_stream::<#project>()?)
                    })
                }
                #[allow(clippy::too_many_arguments)]
                fn ties<'a>(
                    &'a self,
                    session: &'a #session,
                    extra: &'a Self::Extra,
                    scope: &'a Self::Group,
                    year: i32,
                    bucket: #schema::Bucket,
                    at: #time,
                    tie: &'a Self::Tie,
                ) -> #schema::CursorFuture<'a, #private::scylla::client::pager::TypedRowStream<#project>> {
                    ::std::boxed::Box::pin(async move {
                        #partition_consts
                        // bind the partition values, then the timestamp and the tie
                        let __bucket = bucket.to_cql()?;
                        let _ = extra;
                        #(#tie_query_binds)*
                        #at_bind
                        #(#tie_binds)*
                        let binds = #private::chain_binds(
                            &[],
                            #private::in_key_order::<#count>(KEY_ORDER, [#(&#names),*]),
                            &[&__at, #(&#tie_names),*],
                        );
                        let pager = session.execute_iter(self.#ties_stmt.clone(), &binds[..]).await?;
                        Ok(pager.rows_stream::<#project>()?)
                    })
                }
                #common
            }
        }
    } else {
        let scope_order = if scope_count == 1 {
            quote! { #(#scope_cmps)* }
        } else {
            quote! {
                const KEY_ORDER: [usize; #scope_count] = #private::key_order(&#def, [#(#scope_names),*]);
                let cmps: [&dyn Fn() -> ::core::cmp::Ordering; #scope_count] = [#(&|| #scope_cmps),*];
                KEY_ORDER
                    .iter()
                    .map(|index| cmps[*index]())
                    .find(|ordering| ordering.is_ne())
                    .unwrap_or(::core::cmp::Ordering::Equal)
            }
        };
        quote! {
            impl #schema::TimeCursorStatements for #ty {
                type Extra = #extra_ty;
                type Group = #group_ty;
                type Row = #project;
                type Tie = #tie_ty;
                type TieScope = #scope_ty;
                const ORDER: #schema::Order = #private::cursor_order(&#def, #range, #order);
                fn bucket_limit(extra: &Self::Extra) -> Result<u32, #schema::BucketLimitError> {
                    // the filter is unused when the cursor has no `IN` filters
                    let _ = extra;
                    #schema::bucket_limit(&[#(#in_lens),*])
                }
                #[allow(clippy::too_many_arguments)]
                fn pull<'a>(
                    &'a self,
                    session: &'a #session,
                    extra: &'a Self::Extra,
                    group: &'a Self::Group,
                    year: i32,
                    buckets: &'a [#schema::Bucket],
                    before: #time,
                    after: #time,
                    limit: #schema::CursorLimit,
                ) -> #schema::CursorFuture<'a, Vec<#schema::RowResult<#project>>> {
                    ::std::boxed::Box::pin(async move {
                        #partition_consts
                        // bind the partition values, then the window's timestamps and the limit
                        let __buckets = #private::bucket_ints(buckets)?;
                        let _ = extra;
                        #(#pull_binds)*
                        #before_bind
                        #after_bind
                        let __limit = limit.to_cql();
                        let binds = #private::chain_binds(
                            &[],
                            #private::in_key_order::<#count>(KEY_ORDER, [#(&#names),*]),
                            &[&__before, &__after, &__limit],
                        );
                        let result = session.execute_unpaged(&self.#pull_stmt, &binds[..]).await?;
                        #private::rows(result)
                    })
                }
                #[allow(clippy::too_many_arguments)]
                fn ties<'a>(
                    &'a self,
                    session: &'a #session,
                    extra: &'a Self::Extra,
                    scope: &'a Self::TieScope,
                    year: i32,
                    bucket: #schema::Bucket,
                    at: #time,
                    tie: &'a Self::Tie,
                    limit: #schema::CursorLimit,
                ) -> #schema::CursorFuture<'a, Vec<#schema::RowResult<#project>>> {
                    ::std::boxed::Box::pin(async move {
                        #partition_consts
                        // bind the partition values, then the timestamp, the tie, and the limit
                        let __bucket = bucket.to_cql()?;
                        let _ = extra;
                        #(#tie_query_binds)*
                        #at_bind
                        #(#tie_binds)*
                        let __limit = limit.to_cql();
                        // run each tie query, keeping every row it reads
                        let mut rows = Vec::new();
                        #(#tie_runs)*
                        Ok(rows)
                    })
                }
                #common
                fn row_scope(row: &#project) -> Self::TieScope {
                    #row_scope
                }
                fn cmp_ties(left: &Self::Tie, right: &Self::Tie) -> ::core::cmp::Ordering {
                    ::core::cmp::Ordering::Equal #(.then_with(|| #tie_cmps))*
                }
                fn cmp_scopes(left: &Self::TieScope, right: &Self::TieScope) -> ::core::cmp::Ordering {
                    #scope_order
                }
                fn scope_group(scope: &Self::TieScope) -> &Self::Group {
                    #scope_group
                }
            }
        }
    };
    Ok((quote! { #holder #imp }, checks))
}

/// Build a simple cursor: the first rows of a partition, and the rows after a tie
///
/// # Arguments
///
/// * `cursor` - The cursor's context
/// * `args` - The cursor's arguments
/// * `attr` - The cursor's attribute, for errors
/// * `docs` - The cursor field's doc attributes
// a simple cursor's two statements and trait impl are generated together
#[allow(clippy::too_many_lines)]
fn simple(
    cursor: &CursorCtx<'_>,
    args: &CursorArgs,
    attr: &syn::Attribute,
    docs: &[syn::Attribute],
) -> syn::Result<(TokenStream, TokenStream)> {
    let schema = &cursor.ctx.paths.schema;
    let private = &cursor.ctx.paths.private;
    let object = cursor.ctx.object;
    let project = &cursor.project;
    let def = cursor.ctx.def();
    let label = &cursor.label;
    let partition = required(args.partition.as_ref(), "partition", attr)?;
    let [tie] = args.tie.as_slice() else {
        return Err(syn::Error::new_spanned(
            attr,
            "a simple cursor resumes after one `tie` column",
        ));
    };
    if args.limit.unwrap_or(LimitKind::Limit) != LimitKind::Limit {
        return Err(syn::Error::new_spanned(
            attr,
            "a simple cursor takes `limit = limit`",
        ));
    }
    let first_stmt = cursor.field.clone();
    let after_stmt = format_ident!("{}_ties", cursor.field);
    let partition_name = cursor.name(partition);
    let tie_name = cursor.name(tie);
    let first_fn = format_ident!("__cql_{first_stmt}");
    let after_fn = format_ident!("__cql_{after_stmt}");
    let render = |clustering: TokenStream| {
        quote! {
            #private::cursor_cql(
                ns,
                <#object as #schema::ScyllaObject>::NAME,
                <#project as #schema::Projection>::COLUMNS,
                &[(#partition_name, #schema::Restriction::Eq)],
                &#clustering,
                None,
                #schema::CursorLimitClause::Limit,
            )
        }
    };
    let first_render = render(quote! { [] });
    let after_render = render(quote! {
        [format!(
            "{} {} ?",
            #tie_name,
            #private::resume_after(#private::declared_order(&#def, #tie_name)),
        )]
    });
    let holder = cursor.holder(
        docs,
        &[
            (
                first_stmt.clone(),
                "Read the first rows of a partition",
                quote! { fn #first_fn(ns: &str) -> String { #first_render } },
            ),
            (
                after_stmt.clone(),
                "Read the rows of a partition after a tie",
                quote! { fn #after_fn(ns: &str) -> String { #after_render } },
            ),
        ],
    );
    let project_checks = cursor.project_checks();
    let checks = quote! {
        const _: () = #private::assert_cursor_partition(&#def, &[#partition_name], #label);
        const _: () = #private::assert_cursor_clustering(&#def, &[#tie_name], #label);
        #project_checks
    };
    let ty = &cursor.ty;
    let session = quote! { #private::scylla::client::session::Session };
    let partition_ty = cursor.owned(partition);
    let tie_ty = cursor.owned(tie);
    let bind_partition = cursor.bind_owned(partition, &quote! { partition });
    let bind_tie = cursor.bind_owned(tie, &quote! { tie });
    let tie_value = cursor.row_value(tie, &quote! { row });
    let imp = quote! {
        impl #schema::SimpleCursorStatements for #ty {
            type Partition = #partition_ty;
            type Row = #project;
            type Tie = #tie_ty;
            fn first<'a>(
                &'a self,
                session: &'a #session,
                partition: &'a Self::Partition,
                limit: #schema::CursorLimit,
            ) -> #schema::CursorFuture<'a, Vec<#schema::RowResult<#project>>> {
                ::std::boxed::Box::pin(async move {
                    // bind the partition, then the limit
                    let __partition = #bind_partition;
                    let __limit = limit.to_cql();
                    let binds: [&(dyn #private::scylla::serialize::value::SerializeValue + Sync); 2] =
                        [&__partition, &__limit];
                    let result = session.execute_unpaged(&self.#first_stmt, &binds[..]).await?;
                    #private::rows(result)
                })
            }
            fn after<'a>(
                &'a self,
                session: &'a #session,
                partition: &'a Self::Partition,
                tie: &'a Self::Tie,
                limit: #schema::CursorLimit,
            ) -> #schema::CursorFuture<'a, Vec<#schema::RowResult<#project>>> {
                ::std::boxed::Box::pin(async move {
                    // bind the partition, then the tie, then the limit
                    let __partition = #bind_partition;
                    let __tie = #bind_tie;
                    let __limit = limit.to_cql();
                    let binds: [&(dyn #private::scylla::serialize::value::SerializeValue + Sync); 3] =
                        [&__partition, &__tie, &__limit];
                    let result = session.execute_unpaged(&self.#after_stmt, &binds[..]).await?;
                    #private::rows(result)
                })
            }
            fn row_tie(row: &#project) -> Self::Tie {
                ::core::clone::Clone::clone(#tie_value)
            }
        }
    };
    Ok((quote! { #holder #imp }, checks))
}

/// Build a grouped cursor: the first rows of a group, the rows after a sort value, and the rows
/// at a sort value
///
/// # Arguments
///
/// * `cursor` - The cursor's context
/// * `args` - The cursor's arguments
/// * `attr` - The cursor's attribute, for errors
/// * `docs` - The cursor field's doc attributes
// a grouped cursor's three statements and trait impl are generated together
#[allow(clippy::too_many_lines)]
fn grouped(
    cursor: &CursorCtx<'_>,
    args: &CursorArgs,
    attr: &syn::Attribute,
    docs: &[syn::Attribute],
) -> syn::Result<(TokenStream, TokenStream)> {
    let schema = &cursor.ctx.paths.schema;
    let private = &cursor.ctx.paths.private;
    let object = cursor.ctx.object;
    let project = &cursor.project;
    let def = cursor.ctx.def();
    let label = &cursor.label;
    let group = required(args.group_by.as_ref(), "group_by", attr)?;
    let sort = required(args.sort.as_ref(), "sort", attr)?;
    let tie_breaker = required(args.tie_breaker.as_ref(), "tie_breaker", attr)?;
    if args.limit.unwrap_or(LimitKind::PerPartition) != LimitKind::PerPartition {
        return Err(syn::Error::new_spanned(
            attr,
            "a grouped cursor takes `limit = per_partition`",
        ));
    }
    let pull_stmt = format_ident!("{}_pull", cursor.field);
    let more_stmt = format_ident!("{}_pull_more", cursor.field);
    let ties_stmt = format_ident!("{}_ties", cursor.field);
    let group_name = cursor.name(group);
    let sort_name = cursor.name(sort);
    let render = |name: &syn::Ident, clustering: TokenStream| {
        let cql_fn = format_ident!("__cql_{name}");
        quote! {
            fn #cql_fn(ns: &str) -> String {
                #private::cursor_cql(
                    ns,
                    <#object as #schema::ScyllaObject>::NAME,
                    <#project as #schema::Projection>::COLUMNS,
                    &[(#group_name, #schema::Restriction::Eq)],
                    &#clustering,
                    None,
                    #schema::CursorLimitClause::PerPartition,
                )
            }
        }
    };
    let holder = cursor.holder(
        docs,
        &[
            (
                pull_stmt.clone(),
                "Read the first rows of a group",
                render(&pull_stmt, quote! { [] }),
            ),
            (
                more_stmt.clone(),
                "Read the rows of a group sorted after a value",
                render(
                    &more_stmt,
                    quote! {
                        [format!(
                            "{} {} ?",
                            #sort_name,
                            #private::resume_after(#private::declared_order(&#def, #sort_name)),
                        )]
                    },
                ),
            ),
            (
                ties_stmt.clone(),
                "Read the rows of a group with a sort value",
                render(&ties_stmt, quote! { [format!("{} = ?", #sort_name)] }),
            ),
        ],
    );
    let project_checks = cursor.project_checks();
    let checks = quote! {
        const _: () = #private::assert_cursor_partition(&#def, &[#group_name], #label);
        const _: () = #private::assert_cursor_clustering(&#def, &[#sort_name], #label);
        #project_checks
    };
    let ty = &cursor.ty;
    let session = quote! { #private::scylla::client::session::Session };
    let group_ty = cursor.owned(group);
    let sort_ty = cursor.owned(sort);
    let breaker_ty = cursor.owned(tie_breaker);
    let bind_group = cursor.bind_owned(group, &quote! { group });
    let bind_sort = cursor.bind_owned(sort, &quote! { sort });
    let row = quote! { row };
    let group_value = cursor.row_value(group, &row);
    let sort_value = cursor.row_value(sort, &row);
    let breaker_value = cursor.row_value(tie_breaker, &row);
    let serialize = quote! { #private::scylla::serialize::value::SerializeValue };
    let imp = quote! {
        impl #schema::GroupedCursorStatements for #ty {
            type Group = #group_ty;
            type SortBy = #sort_ty;
            type TieBreaker = #breaker_ty;
            type Row = #project;
            fn pull<'a>(
                &'a self,
                session: &'a #session,
                group: &'a Self::Group,
                limit: #schema::CursorLimit,
            ) -> #schema::CursorFuture<'a, Vec<#schema::RowResult<#project>>> {
                ::std::boxed::Box::pin(async move {
                    // bind the group, then the limit
                    let __group = #bind_group;
                    let __limit = limit.to_cql();
                    let binds: [&(dyn #serialize + Sync); 2] = [&__group, &__limit];
                    let result = session.execute_unpaged(&self.#pull_stmt, &binds[..]).await?;
                    #private::rows(result)
                })
            }
            fn pull_more<'a>(
                &'a self,
                session: &'a #session,
                group: &'a Self::Group,
                sort: &'a Self::SortBy,
                limit: #schema::CursorLimit,
            ) -> #schema::CursorFuture<'a, Vec<#schema::RowResult<#project>>> {
                ::std::boxed::Box::pin(async move {
                    // bind the group, then the sort value, then the limit
                    let __group = #bind_group;
                    let __sort = #bind_sort;
                    let __limit = limit.to_cql();
                    let binds: [&(dyn #serialize + Sync); 3] = [&__group, &__sort, &__limit];
                    let result = session.execute_unpaged(&self.#more_stmt, &binds[..]).await?;
                    #private::rows(result)
                })
            }
            fn ties<'a>(
                &'a self,
                session: &'a #session,
                group: &'a Self::Group,
                sort: &'a Self::SortBy,
                limit: #schema::CursorLimit,
            ) -> #schema::CursorFuture<'a, Vec<#schema::RowResult<#project>>> {
                ::std::boxed::Box::pin(async move {
                    // bind the group, then the sort value, then the limit
                    let __group = #bind_group;
                    let __sort = #bind_sort;
                    let __limit = limit.to_cql();
                    let binds: [&(dyn #serialize + Sync); 3] = [&__group, &__sort, &__limit];
                    let result = session.execute_unpaged(&self.#ties_stmt, &binds[..]).await?;
                    #private::rows(result)
                })
            }
            fn row_group(row: &#project) -> Self::Group {
                ::core::clone::Clone::clone(#group_value)
            }
            fn row_sort(row: &#project) -> Self::SortBy {
                ::core::clone::Clone::clone(#sort_value)
            }
            fn row_tie_breaker(row: &#project) -> &Self::TieBreaker {
                #breaker_value
            }
        }
    };
    Ok((quote! { #holder #imp }, checks))
}

/// Build an exists cursor: whether any row with a key exists in some buckets of a year
///
/// # Arguments
///
/// * `cursor` - The cursor's context
/// * `args` - The cursor's arguments
/// * `attr` - The cursor's attribute, for errors
/// * `docs` - The cursor field's doc attributes
fn exists(
    cursor: &CursorCtx<'_>,
    args: &CursorArgs,
    attr: &syn::Attribute,
    docs: &[syn::Attribute],
) -> syn::Result<(TokenStream, TokenStream)> {
    let schema = &cursor.ctx.paths.schema;
    let private = &cursor.ctx.paths.private;
    let object = cursor.ctx.object;
    let def = cursor.ctx.def();
    let label = &cursor.label;
    let key = required(args.key.as_ref(), "key", attr)?;
    let year = required(args.year.as_ref(), "year", attr)?;
    let bucket = required(args.bucket.as_ref(), "bucket", attr)?;
    let stmt = cursor.field.clone();
    let cql_fn = format_ident!("__cql_{stmt}");
    let key_name = cursor.name(key);
    let year_name = cursor.name(year);
    let bucket_name = cursor.name(bucket);
    let cql = quote! {
        fn #cql_fn(ns: &str) -> String {
            const COLUMNS: [&str; 3] = [#key_name, #year_name, #bucket_name];
            const OPS: [#schema::Restriction; 3] = [
                #schema::Restriction::Eq,
                #schema::Restriction::Eq,
                #schema::Restriction::In,
            ];
            const KEY_ORDER: [usize; 3] = #private::key_order(&#def, COLUMNS);
            // render the partition restrictions in key order
            let restrictions = KEY_ORDER.map(|index| (COLUMNS[index], OPS[index]));
            #private::cursor_cql(
                ns,
                <#object as #schema::ScyllaObject>::NAME,
                &[#key_name],
                &restrictions,
                &[],
                None,
                #schema::CursorLimitClause::PerPartitionOne,
            )
        }
    };
    let holder = cursor.holder(
        docs,
        &[(
            stmt.clone(),
            "Read one row of each partition holding a key",
            cql,
        )],
    );
    let checks = quote! {
        const _: () = #private::assert_cursor_partition(&#def, &[#key_name, #year_name, #bucket_name], #label);
    };
    let ty = &cursor.ty;
    let session = quote! { #private::scylla::client::session::Session };
    let key_ty = cursor.owned(key);
    let bind_key = cursor.bind_owned(key, &quote! { key });
    let bind_year = cursor.bind_owned(year, &quote! { &year });
    let bind_buckets = cursor.bind_list(bucket, &quote! { &__buckets });
    let imp = quote! {
        impl #schema::ExistsStatement for #ty {
            type Key = #key_ty;
            fn any<'a>(
                &'a self,
                session: &'a #session,
                year: i32,
                buckets: &'a [#schema::Bucket],
                key: &'a Self::Key,
            ) -> #schema::CursorFuture<'a, bool> {
                ::std::boxed::Box::pin(async move {
                    const COLUMNS: [&str; 3] = [#key_name, #year_name, #bucket_name];
                    const KEY_ORDER: [usize; 3] = #private::key_order(&#def, COLUMNS);
                    // bind the key, year, and buckets in key order
                    let __buckets = #private::bucket_ints(buckets)?;
                    let __key = #bind_key;
                    let __year = #bind_year;
                    let __bucket = #bind_buckets;
                    let binds = #private::in_key_order::<3>(KEY_ORDER, [&__key, &__year, &__bucket]);
                    let result = session.execute_unpaged(&self.#stmt, &binds[..]).await?;
                    #private::any_row(result)
                })
            }
        }
    };
    Ok((quote! { #holder #imp }, checks))
}

/// Build the statements, checks, and trait impl one `#[cursor(...)]` field declares
///
/// # Arguments
///
/// * `ctx` - The queries struct's context
/// * `field` - The field the cursor is declared on
/// * `attr` - The field's `#[cursor(...)]` attribute
pub(super) fn cursor(
    ctx: &Context<'_>,
    field: &syn::Field,
    attr: &syn::Attribute,
) -> syn::Result<Statement> {
    let args = cursor_args(attr)?;
    let ident = field.ident.clone().expect("named fields have names");
    let docs = doc_attrs(field);
    let kind = *required(args.kind.as_ref(), "kind", attr)?;
    // an exists cursor reads only its key, so it names no projection
    let project = match (&args.project, kind) {
        (Some(project), _) => project.clone(),
        (None, Kind::Exists) => ctx.object.clone(),
        (None, _) => return Err(syn::Error::new_spanned(attr, "this cursor needs `project`")),
    };
    // name the generated type after the queries struct and the field
    let queries = ctx.queries.to_string();
    let base = queries.strip_suffix("Queries").unwrap_or(&queries);
    let cursor = CursorCtx {
        ctx,
        ty: format_ident!("{base}{}Cursor", camel(&ident.to_string())),
        field: ident.clone(),
        project,
        label: ctx.label(&ident),
    };
    let (generated, checks) = match kind {
        Kind::Time => time(&cursor, &args, attr, &docs)?,
        Kind::Simple => simple(&cursor, &args, attr, &docs)?,
        Kind::Grouped => grouped(&cursor, &args, attr, &docs)?,
        Kind::Exists => exists(&cursor, &args, attr, &docs)?,
    };
    Ok(Statement {
        field: ident,
        docs,
        cql: TokenStream::new(),
        methods: TokenStream::new(),
        items: quote! { #generated #checks },
        grouped: Some(Grouped { ty: cursor.ty }),
    })
}
