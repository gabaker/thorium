//! Handles the image edit command

use thorium::{Error, Thorium};

use crate::CtlConf;
use crate::args::images::EditImage;
use crate::handlers::edit::{self, ImageEditable};
use crate::utils;

/// Edit an image using a text editor, detect the updates, then update the image
///
/// The edit stops before the editor opens when Thorium refuses image writes.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl conf
/// * `cmd` - The edit image command that was run
pub async fn edit(thorium: Thorium, conf: &CtlConf, cmd: &EditImage) -> Result<(), Error> {
    // find the image's group so the write check and the edit agree on it
    let group = match &cmd.group {
        Some(group) => group.clone(),
        None => utils::images::find_image_group(&thorium, &cmd.image).await?,
    };
    // stop before opening an editor when image writes are refused
    super::ensure_image_writes_accepted(&thorium, &group, &cmd.image).await?;
    edit::edit_entity::<ImageEditable>(
        &thorium,
        conf,
        &cmd.image,
        Some(group.as_str()),
        cmd.editor.as_deref(),
    )
    .await
}
