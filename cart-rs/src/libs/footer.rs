//! The footers for the cart format

use crate::Error;
use std::io::Write;

/// The length of a standard `CaRT` footer
pub const FOOTER_LEN: usize = 28;
/// The magic number beginning the footer
pub static MAGIC_NUM: &[u8; 4] = b"TRAC";

/// The mandatory footer object for `CaRT`
///
/// While the [cart docs](https://bitbucket.org/cse-assemblyline/cart/src/master/) say this footer is 32 bytes long in reality it is only 28 bytes. It
/// appears the python cart implementation adds 4 bytes to the end of the stream. This can be
/// seen by carting a file with the python implementation and then running hexdump on the output.
///
/// Currently this footer is not used as cart-rs does not support an optional footer (it will just
/// be ignored when uncarting a file).
#[derive(Debug, Clone)]
pub struct Footer {
    // The size of the optional footer to skip
    pub opt_len: u64,
}

impl Footer {
    /// Build the footer for this carted file
    #[must_use]
    pub fn new_buffer() -> [u8; FOOTER_LEN] {
        // build our mandatory footer vector of 28 bytes
        let mut footer = [0; FOOTER_LEN];
        // write the CaRT magic number
        Self::write_magic_num(&mut footer);
        footer
    }

    /// Write the `CaRT` footer to a buffer
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer to write the footer to
    pub fn write(mut buf: &mut [u8]) -> Result<(), std::io::Error> {
        let mut footer: [u8; FOOTER_LEN] = [0; FOOTER_LEN];
        Self::write_magic_num(&mut footer);
        buf.write_all(&footer)
    }

    /// Write the `CaRT` magic number to the beginning of a buffer
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer to write the `CaRT` magic number to
    fn write_magic_num(buf: &mut [u8]) {
        buf[..MAGIC_NUM.len()].copy_from_slice(MAGIC_NUM);
    }

    /// Gets the footer from the last 28 bytes of the raw binary
    ///
    /// # Arguments
    ///
    /// * `raw` - The last 28 bytes of the binary containing the footer
    ///
    /// # Errors
    ///
    /// If this buffer does not start with the TRAC magic number then an error will be returned.
    ///
    /// # Errors
    ///
    /// If any IO errors occur then an error will be returned and the header will fail to write.
    /// IO errors should only happen if an insufficient buffer is provided.
    pub fn get(raw: &[u8]) -> Result<Self, Error> {
        // get the last 28 bytes of this buffer
        let end = &raw[raw.len() - 28..];
        // make sure the magic numbers match carts magic number
        if end[..4] == *MAGIC_NUM {
            // extract the length of the optional footer
            let opt_len = bincode::deserialize(&end[20..])?;
            return Ok(Footer { opt_len });
        }
        Err(Error::new(
            "Footer does not start with the TRAC magic number".to_string(),
        ))
    }

    /// Calculate how much of the binary to trim off the end to not read the footer
    #[must_use]
    pub fn trim(&self) -> usize {
        // In order to not read the footer we need to trim 28 bytes plus any optional footer
        (28 + self.opt_len) as usize
    }
}
