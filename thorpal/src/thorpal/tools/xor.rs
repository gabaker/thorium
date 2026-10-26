//! A bytewise XOR tool for decoding obfuscated data

use rig::tool::ToolExecutionError;

/// Remove any whitespace in this string
///
/// # Arguments
///
/// * `s` - The string to strip whitespace from
fn remove_spaces(s: &str) -> String {
    // remove any whitespace
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Xors two equal length hex strings
///
/// If either input contains spaces then they are stripped before xoring and
/// the result is re-spaced every byte to match the input format.
///
/// # Arguments
///
/// * `a` - The first hex string to xor
/// * `b` - The second hex string to xor
#[thorium_derive::thorpal_tool(
    group = "transforms",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_bytewise_xor",
    rhai_doc = "bytewise_xor(a: string, b: string) -> string -- xor two equal-length hex strings",
    rhai_reference = "Xors two equal-length hex strings bytewise.

Accepts spaced (`\"de ad be ef\"`) or unspaced (`\"deadbeef\"`) hex; the result
matches the input format. Errors if the lengths differ or the hex is invalid."
)]
#[rig::tool_macro(description = "Xors two equal length hex strings")]
pub fn bytewise_xor(mut a: String, mut b: String) -> Result<String, rig::tool::ToolExecutionError> {
    // track whether we removed spaces
    let mut has_spaces = false;
    // check and remove spaces from our strings if needed
    if a.contains(' ') || b.contains(' ') {
        // remove the spaces in our input hex strings
        a = remove_spaces(&a);
        b = remove_spaces(&b);
        // mark that we have spaces in our inputs
        has_spaces = true;
    }
    // both hex strings must be the same length to xor bytewise
    if a.len() != b.len() {
        return Err(ToolExecutionError::invalid_args(
            "Hex strings must have the same length".to_string(),
        ));
    }
    // each byte takes two hex characters
    if !a.len().is_multiple_of(2) {
        return Err(ToolExecutionError::invalid_args(
            "Hex strings must have an even number of characters".to_string(),
        ));
    }
    // decode both hex strings into bytes
    let bytes_a = hex_to_bytes(&a)?;
    let bytes_b = hex_to_bytes(&b)?;
    // xor each pair of bytes and re-encode the result as hex
    let result: String = bytes_a
        .iter()
        .zip(bytes_b.iter())
        .map(|(x, y)| format!("{:02x}", x ^ y))
        .collect();
    // if we had spaces then readd them
    if has_spaces {
        // inject a space back in after every 2 chars; chunk over the characters
        // directly (the result is pure ASCII hex) so there is no byte-slice utf8
        // round-trip to unwrap
        let chars: Vec<char> = result.chars().collect();
        let spaced = chars
            .chunks(2)
            .map(|chunk| chunk.iter().collect::<String>())
            .collect::<Vec<String>>()
            .join(" ");
        Ok(spaced)
    } else {
        Ok(result)
    }
}

/// Decode a hex string with an even number of characters into bytes
///
/// # Arguments
///
/// * `hex` - The hex string to decode
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, ToolExecutionError> {
    // allocate room for every decoded byte up front
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    // decode each two-character chunk into a single byte
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| {
            ToolExecutionError::invalid_args(format!("Invalid hex at position {}", i))
        })?;
        bytes.push(byte);
    }
    Ok(bytes)
}
