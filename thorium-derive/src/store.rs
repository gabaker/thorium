//! Derives for types stored in a Scylla `text` column, as JSON or by name
//!
//! The generated code names the api crate's re-exports through `::thorium`, so it compiles in
//! any crate that depends on `thorium-api` with its `scylla-utils` feature.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

/// The path of the api crate's re-exports for generated code
fn private() -> TokenStream {
    quote! { ::thorium::models::schema::__private }
}

/// Add the `CqlType` impl every text-stored type shares
///
/// # Arguments
///
/// * `stream` - The token stream to extend
/// * `name` - The type to implement it for
fn add_text_cql_type(stream: &mut TokenStream, name: &Ident) {
    // the column type of a text-stored value is always text
    stream.extend(quote! {
        impl ::thorium::models::schema::CqlType for #name {
            /// A text-stored value is always stored in a `text` column
            const DESC: ::thorium::models::schema::CqlDesc =
                ::thorium::models::schema::CqlDesc::Native(::thorium::models::schema::CqlNative::Text);
        }
    });
}

/// Add the JSON based serialize impl
///
/// # Arguments
///
/// * `stream` - The token stream to extend
/// * `name` - The type to implement it for
fn add_json_serialize(stream: &mut TokenStream, name: &Ident) {
    let private = private();
    // extend our token stream
    stream.extend(quote! {
        impl #private::scylla::serialize::value::SerializeValue for #name {
            fn serialize<'b>(
                &self,
                typ: &#private::scylla::frame::response::result::ColumnType,
                writer: #private::scylla::serialize::writers::CellWriter<'b>,
            ) -> Result<#private::scylla::serialize::writers::WrittenCellProof<'b>, #private::scylla::serialize::SerializationError> {
                // serialize our value as JSON
                let value = match #private::serde_json::to_string(self) {
                    Ok(value) => value,
                    Err(error) => return Err(#private::scylla::serialize::SerializationError::new(error)),
                };
                // write the JSON as text
                #private::scylla::serialize::value::SerializeValue::serialize(&value, typ, writer)
            }
        }
    });
}

/// Add the JSON based deserialize impl
///
/// # Arguments
///
/// * `stream` - The token stream to extend
/// * `name` - The type to implement it for
fn add_json_deserialize(stream: &mut TokenStream, name: &Ident) {
    let private = private();
    // extend our token stream
    stream.extend(quote! {
        impl<'frame, 'metadata> #private::scylla::deserialize::value::DeserializeValue<'frame, 'metadata> for #name {
            fn type_check(typ: &#private::scylla::frame::response::result::ColumnType) -> Result<(), #private::scylla::deserialize::TypeCheckError> {
                if let #private::scylla::frame::response::result::ColumnType::Native(#private::scylla::frame::response::result::NativeType::Text) = typ {
                    return Ok(());
                }
                Err(#private::scylla::deserialize::TypeCheckError::new(#private::DeserializationError::ExpectedText))
            }
            fn deserialize(
                _typ: &'metadata #private::scylla::frame::response::result::ColumnType<'metadata>,
                v: Option<#private::scylla::deserialize::FrameSlice<'frame>>,
            ) -> Result<Self, #private::scylla::deserialize::DeserializationError> {
                // check if we got data
                match v {
                    Some(fslice) => {
                        // parse the stored JSON
                        match #private::serde_json::from_slice(fslice.as_slice()) {
                            Ok(value) => Ok(value),
                            Err(_) => Err(#private::scylla::deserialize::DeserializationError::new(
                                #private::DeserializationError::UnknownValue,
                            )),
                        }
                    }
                    None => Err(#private::scylla::deserialize::DeserializationError::new(
                        #private::DeserializationError::ExpectedNotNull,
                    )),
                }
            }
        }
    });
}

/// Generate the `ScyllaStoreJson` derive
///
/// # Arguments
///
/// * `ast` - The type the derive is on
pub fn derive_store_json(ast: &syn::DeriveInput) -> TokenStream {
    // start with an empty stream
    let mut output = TokenStream::new();
    // add our JSON conversions and the column type
    add_json_serialize(&mut output, &ast.ident);
    add_json_deserialize(&mut output, &ast.ident);
    add_text_cql_type(&mut output, &ast.ident);
    output
}

/// Add the `as_str` based serialize impl
///
/// # Arguments
///
/// * `stream` - The token stream to extend
/// * `name` - The type to implement it for
fn add_as_str_serialize(stream: &mut TokenStream, name: &Ident) {
    let private = private();
    // extend our token stream
    stream.extend(quote! {
        impl #private::scylla::serialize::value::SerializeValue for #name {
            fn serialize<'b>(
                &self,
                typ: &#private::scylla::frame::response::result::ColumnType,
                writer: #private::scylla::serialize::writers::CellWriter<'b>,
            ) -> Result<#private::scylla::serialize::writers::WrittenCellProof<'b>, #private::scylla::serialize::SerializationError> {
                // cast our value as a str
                let value = &self.as_str();
                // write it as text
                #private::scylla::serialize::value::SerializeValue::serialize(&value, typ, writer)
            }
        }
    });
}

/// Add the `FromStr` based deserialize impl
///
/// # Arguments
///
/// * `stream` - The token stream to extend
/// * `name` - The type to implement it for
fn add_as_str_deserialize(stream: &mut TokenStream, name: &Ident) {
    let private = private();
    // extend our token stream
    stream.extend(quote! {
        impl<'frame, 'metadata> #private::scylla::deserialize::value::DeserializeValue<'frame, 'metadata> for #name {
            fn type_check(typ: &#private::scylla::frame::response::result::ColumnType) -> Result<(), #private::scylla::deserialize::TypeCheckError> {
                if let #private::scylla::frame::response::result::ColumnType::Native(#private::scylla::frame::response::result::NativeType::Text) = typ {
                    return Ok(());
                }
                Err(#private::scylla::deserialize::TypeCheckError::new(#private::DeserializationError::ExpectedText))
            }
            fn deserialize(
                _typ: &'metadata #private::scylla::frame::response::result::ColumnType<'metadata>,
                v: Option<#private::scylla::deserialize::FrameSlice<'frame>>,
            ) -> Result<Self, #private::scylla::deserialize::DeserializationError> {
                // check if we got data
                match v {
                    Some(fslice) => {
                        // try to convert our slice to a str
                        let converted = match std::str::from_utf8(fslice.as_slice()) {
                            Ok(converted) => converted,
                            Err(_) => {
                                return Err(#private::scylla::deserialize::DeserializationError::new(
                                    #private::DeserializationError::ExpectedText,
                                ))
                            }
                        };
                        // get the correct kind
                        match #name::from_str(converted) {
                            Ok(value) => Ok(value),
                            Err(_) => Err(#private::scylla::deserialize::DeserializationError::new(
                                #private::DeserializationError::UnknownValue,
                            )),
                        }
                    }
                    None => Err(#private::scylla::deserialize::DeserializationError::new(
                        #private::DeserializationError::ExpectedNotNull,
                    )),
                }
            }
        }
    });
}

/// Generate the `ScyllaStoreAsStr` derive
///
/// # Arguments
///
/// * `ast` - The type the derive is on
pub fn derive_store_as_str(ast: &syn::DeriveInput) -> TokenStream {
    // start with an empty stream
    let mut output = TokenStream::new();
    // add our name based conversions and the column type
    add_as_str_serialize(&mut output, &ast.ident);
    add_as_str_deserialize(&mut output, &ast.ident);
    add_text_cql_type(&mut output, &ast.ident);
    output
}
