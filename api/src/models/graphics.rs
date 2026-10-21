//! The kinds of graphics Thorium stores for users and entities

/// Every content type Thorium accepts for a graphic, with the extension a graphic of that type
/// gets in its key in the graphics bucket
pub const GRAPHIC_TYPES: [(&str, &str); 7] = [
    ("image/bmp", "bmp"),
    ("image/gif", "gif"),
    ("image/jpeg", "jpeg"),
    ("image/png", "png"),
    ("image/svg+xml", "svg"),
    ("video/mp4", "mp4"),
    ("video/webm", "webm"),
];

/// Returns the extension a graphic of a content type is stored with, or `None` for a type
/// Thorium doesn't accept for graphics
///
/// # Arguments
///
/// * `content_type` - The graphic's content type, without parameters
#[must_use]
pub fn graphic_extension(content_type: &str) -> Option<&'static str> {
    // find the accepted type and its extension
    GRAPHIC_TYPES
        .iter()
        .find(|(listed, _)| *listed == content_type)
        .map(|(_, extension)| *extension)
}

/// Returns the content type of a graphic stored with an extension, or `None` for an extension
/// no accepted graphic type is stored with
///
/// # Arguments
///
/// * `extension` - The extension of the graphic's key, without its dot
#[must_use]
pub fn graphic_content_type(extension: &str) -> Option<&'static str> {
    // find the type stored with this extension
    GRAPHIC_TYPES
        .iter()
        .find(|(_, listed)| *listed == extension)
        .map(|(content_type, _)| *content_type)
}

#[cfg(test)]
mod tests {
    use super::{GRAPHIC_TYPES, graphic_content_type, graphic_extension};

    /// Each accepted type maps to one extension and back, and nothing else maps
    #[test]
    fn graphic_types_map_to_their_extensions_and_back() {
        // every listed type round trips through its extension
        for (content_type, extension) in GRAPHIC_TYPES {
            assert_eq!(graphic_extension(content_type), Some(extension));
            assert_eq!(graphic_content_type(extension), Some(content_type));
        }
        // and types or extensions that aren't listed map to nothing
        assert_eq!(graphic_extension("text/html"), None);
        assert_eq!(graphic_content_type("html"), None);
        assert_eq!(graphic_content_type("PNG"), None);
    }
}
