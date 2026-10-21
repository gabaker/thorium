//! Backup and restore support for the graphics bucket
//!
//! The bucket holds only user icons and entity images, so the `graphics` component copies every
//! object a listing of the bucket names, without reading any table's archives.

use bytecheck::CheckBytes;
use rkyv::{Archive, Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thorium::Conf;
use thorium::models::graphics::graphic_content_type;

use crate::Error;
use crate::backup::s3::object_key;
use crate::backup::{S3Backup, S3Restore, Utils};

/// An object of the graphics bucket, named by its key
#[derive(Debug, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct Graphic {
    /// The object's key
    pub key: String,
}

impl Utils for Graphic {
    /// The name of the directory a backup keeps the graphics in
    fn name() -> &'static str {
        "graphics"
    }
}

#[async_trait::async_trait]
impl S3Backup for Graphic {
    /// Get the objects archived rows name, which for graphics is none: a listing of the bucket
    /// names them instead
    ///
    /// # Arguments
    ///
    /// * `_conf` - The config for this Thorium cluster
    /// * `_root` - The root path to write objects too
    /// * `_chars` - The number of characters to use in order to partition this data on disk
    /// * `_buffer` - The slice of data to deserialize and crawl for s3 backup objects
    async fn paths(
        _conf: &Conf,
        _root: &PathBuf,
        _chars: usize,
        _buffer: &[u8],
    ) -> Result<Vec<(String, String, PathBuf)>, Error> {
        // no table's archives name graphics
        Ok(Vec::new())
    }

    /// The graphics bucket is copied whole, by listing it
    fn listed_role() -> Option<&'static str> {
        // every object of the bucket is an icon or an image to keep
        Some("graphics")
    }
}

impl S3Restore for Graphic {
    /// Get the bucket and key of a backed-up graphic from where the backup keeps it
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file we are restoring
    /// * `objects` - The component's `objects/` directory, which `path` is under
    /// * `conf` - The Thorium config for this cluster
    fn parse(path: &Path, objects: &Path, conf: &Conf) -> Result<(String, String), Error> {
        // every graphic is in the graphics bucket, under the key the backup kept it by
        let (_, key) = object_key(path, objects, false)?;
        Ok((conf.thorium.graphics.bucket.clone(), key))
    }

    /// Returns the content type the API stores a graphic with, from its key's extension
    ///
    /// The API names each graphic's key by its content type, so the extension gives the type
    /// back; a key without a known extension is restored as plain bytes.
    ///
    /// # Arguments
    ///
    /// * `key` - The graphic's key
    fn content_type(key: &str) -> &'static str {
        // the extension of the key's last segment names the type the API stored it with
        Path::new(key)
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(graphic_content_type)
            .unwrap_or("application/octet-stream")
    }
}

#[cfg(test)]
mod tests {
    use super::Graphic;
    use crate::backup::S3Restore;
    use crate::backup::tables::{Comment, Output, S3Id};
    use thorium::models::graphics::GRAPHIC_TYPES;

    /// A restored graphic gets the content type the API stores its extension's graphics with,
    /// and every other object is plain bytes
    #[test]
    fn graphics_are_restored_with_their_content_type() {
        // every type the API accepts, for an icon and an entity image key
        for (content_type, extension) in GRAPHIC_TYPES {
            for key in [
                format!("users/thorium/icon.{extension}"),
                format!("0b4a5e8e-3c84-4d0e-9a4c-3f5a1f0e2d11/device.{extension}"),
            ] {
                assert_eq!(Graphic::content_type(&key), content_type, "{key}");
            }
        }
        // a key without a known extension is plain bytes
        for key in [
            "users/thorium/icon",
            "a.b/c",
            "x/y.html",
            "x/y.PNG",
            "x/.png",
        ] {
            assert_eq!(
                Graphic::content_type(key),
                "application/octet-stream",
                "{key}"
            );
        }
        // the other components never take a type from a key
        for key in ["x/y.png", "y.svg"] {
            assert_eq!(Comment::content_type(key), "application/octet-stream");
            assert_eq!(Output::content_type(key), "application/octet-stream");
            assert_eq!(S3Id::content_type(key), "application/octet-stream");
        }
    }
}
