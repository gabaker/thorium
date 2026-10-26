//! Wraps tool calls for ThorPal

use rig::tool::Tool;

pub mod analyst;
pub mod constants;
pub mod containers;
pub mod decode;
pub mod file;
pub mod forum;
pub mod ghidra;
pub mod memories;
pub mod registry;
pub mod scripts;
pub mod xor;

pub struct ThorPalTool<T: Tool> {
    /// The tool to call
    inner: T,
}
