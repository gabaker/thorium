extern crate proc_macro;

use proc_macro::TokenStream;

mod store;

#[cfg(feature = "schema")]
mod schema;

mod digest;

#[cfg(feature = "sync")]
mod sync;

#[cfg(feature = "python")]
mod python;

// sync and python are the only users of utils right now;
// should that change, remove this feature gate
#[cfg(any(feature = "sync", feature = "python"))]
mod utils;

/// Derive the Scylla conversions of a type stored as JSON in a `text` column
///
/// The type is written with `serde_json::to_string` and read with `serde_json::from_slice`, and
/// its `CqlType` is `text`.
///
/// # Arguments
///
/// * `stream` - The type the derive is on
#[proc_macro_derive(ScyllaStoreJson)]
pub fn derive_scylla_store_json(stream: TokenStream) -> TokenStream {
    // parse our input type
    let ast = syn::parse_macro_input!(stream as syn::DeriveInput);
    store::derive_store_json(&ast).into()
}

/// Derive the Scylla conversions of a type stored by name in a `text` column
///
/// The type is written with its `as_str` method and read with `from_str`, and its `CqlType` is
/// `text`.
///
/// # Arguments
///
/// * `stream` - The type the derive is on
#[proc_macro_derive(ScyllaStoreAsStr)]
pub fn derive_scylla_store_as_str(stream: TokenStream) -> TokenStream {
    // parse our input type
    let ast = syn::parse_macro_input!(stream as syn::DeriveInput);
    store::derive_store_as_str(&ast).into()
}

/// Declare a Scylla base table
///
/// Generates the table's definition, a borrowed row (`<Table>Ref`) with `as_ref`, by-name row
/// serialization for both, checked unsigned conversions, and the full-row statements; views and
/// projections are checked against its definition at compile time.
///
/// # Arguments
///
/// * `stream` - The struct the derive is on
#[cfg(feature = "schema")]
#[proc_macro_derive(ScyllaTable, attributes(thorium, thorium_table))]
pub fn derive_scylla_table(stream: TokenStream) -> TokenStream {
    // parse our input struct
    let ast = syn::parse_macro_input!(stream as syn::DeriveInput);
    schema::table::derive(&ast).into()
}

/// Declare a Scylla materialized view over a base table
///
/// Generates the view's definition, and checks at compile time that every column exists in the
/// base with the same type, that the view's key holds the base's, and that columns the base
/// redacts are redacted.
///
/// # Arguments
///
/// * `stream` - The struct the derive is on
#[cfg(feature = "schema")]
#[proc_macro_derive(ScyllaView, attributes(thorium, thorium_view))]
pub fn derive_scylla_view(stream: TokenStream) -> TokenStream {
    // parse our input struct
    let ast = syn::parse_macro_input!(stream as syn::DeriveInput);
    schema::view::derive(&ast).into()
}

/// Declare the columns one query reads from a table or view
///
/// Generates the column list and a by-name row deserializer, and checks each field's type
/// against its column at compile time.
///
/// # Arguments
///
/// * `stream` - The struct the derive is on
#[cfg(feature = "schema")]
#[proc_macro_derive(ScyllaProjection, attributes(thorium, thorium_projection))]
pub fn derive_scylla_projection(stream: TokenStream) -> TokenStream {
    // parse our input struct
    let ast = syn::parse_macro_input!(stream as syn::DeriveInput);
    schema::projection::derive(&ast).into()
}

/// Generate the prepared statements and typed methods of a table's queries struct
///
/// Each field is one statement, declared by its attribute; the struct is rewritten so each
/// field holds the prepared statement, and it gains `prepare` and one method per statement.
///
/// # Arguments
///
/// * `args` - The attribute's arguments, naming the table
/// * `input` - The queries struct
#[cfg(feature = "schema")]
#[proc_macro_attribute]
pub fn table_queries(args: TokenStream, input: TokenStream) -> TokenStream {
    schema::queries::expand(schema::queries::Target::Table, args.into(), input.into()).into()
}

/// Generate the prepared statements and typed methods of a view's queries struct
///
/// Like `table_queries`, but a view is read-only: Scylla writes views through their base
/// table, so an insert, update, or delete fails to compile.
///
/// # Arguments
///
/// * `args` - The attribute's arguments, naming the view
/// * `input` - The queries struct
#[cfg(feature = "schema")]
#[proc_macro_attribute]
pub fn view_queries(args: TokenStream, input: TokenStream) -> TokenStream {
    schema::queries::expand(schema::queries::Target::View, args.into(), input.into()).into()
}

/// Collect every declared Scylla object into one ordered keyspace
///
/// Each field's type is a table or view; the objects are listed in field order, which is the
/// order they are created in, and two objects sharing a name fail to compile.
///
/// # Arguments
///
/// * `args` - The attribute's arguments, an optional `crate` override
/// * `input` - The keyspace struct
#[cfg(feature = "schema")]
#[proc_macro_attribute]
pub fn keyspace(args: TokenStream, input: TokenStream) -> TokenStream {
    schema::keyspace::expand(args.into(), input.into()).into()
}

/// Derives `DigestValue`, the default-pruned value Thorium hashes for content digests
///
/// Struct fields whose digest value equals their serde default are left out, and field and
/// variant names follow serde's `rename` and `rename_all` attributes. Serde attributes that
/// change the stored value in ways the digest doesn't model are compile errors.
///
/// # Arguments
///
/// * `stream` - The struct or enum to derive for
#[proc_macro_derive(DigestValue, attributes(serde))]
pub fn derive_digest_value(stream: TokenStream) -> TokenStream {
    // parse our input item
    let ast = syn::parse_macro_input!(stream as syn::DeriveInput);
    // build the impl or report why we can't
    match digest::derive(&ast) {
        Ok(output) => output.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Creates a blocking client by wrapping the async client and calling its
/// async methods by blocking on a static runtime.
///
/// When applied to a struct, this creates a the blocking client wrapper with the
/// suffix `Blocking` added.
///
/// When applied to an impl block, it duplicates all of the async client's methods,
/// and sets the body to just calling the method on the inner async client. Async
/// methods are blocked on the static runtime.
#[cfg(feature = "sync")]
#[proc_macro_attribute]
pub fn blocking_struct(args_raw: TokenStream, input: TokenStream) -> TokenStream {
    sync::blocking_struct(args_raw, input)
}

/// Create a wrapping, synchronous version of the given trait with the
/// suffix `Blocking` appended
///
/// The trait has a GAT that implements the target trait and a method that
/// returns an instance of the GAT from `&self`. Then all methods are just
/// called on that inner GAT. Any asynchronous methods are blocked on the
/// static runtime to ensure the trait is synchronous.
#[cfg(feature = "sync")]
#[proc_macro_attribute]
pub fn blocking_trait(meta: TokenStream, input: TokenStream) -> TokenStream {
    sync::blocking_trait(meta, input)
}

/// Create a blocking version of the async function
///
/// The body of the async function is just blocked on the static runtime
#[cfg(feature = "sync")]
#[proc_macro_attribute]
pub fn blocking_fn(meta: TokenStream, input: TokenStream) -> TokenStream {
    sync::blocking_fn(meta, input)
}

/// Set the struct to be a ``PyO3`` `pyclass`, applying `get` and `set` attributes to
/// public fields depending on the given args
#[cfg(feature = "python")]
#[proc_macro_attribute]
pub fn pyclass(meta: TokenStream, input: TokenStream) -> TokenStream {
    python::pyclass(meta, input)
}

/// Defines an identical enum with the suffix `Py` appended that's PyO3-compatible,
/// replacing unit-type variants (e.g. `MyVariant`) with empty tuple variants
/// (e.g. `MyVariant()`).
///
/// From is implemented either side to easily convert types between each other. No
/// attributes are carried over.
///
/// This is required until `PyO3` supports unit-type enum variants in complex enums
/// (see <https://pyo3.rs/v0.27.2/class.html#complex-enums>)
#[cfg(feature = "python")]
#[proc_macro_attribute]
pub fn pyenum(meta: TokenStream, input: TokenStream) -> TokenStream {
    python::pyenum(meta, input)
}
