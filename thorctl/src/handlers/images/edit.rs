//! Handles the image edit command

use colored::Colorize;
use thorium::{Error, Thorium};

use crate::args::images::EditImage;
use crate::handlers::toolbox::editor;
use crate::handlers::toolbox::merge::MergeableImage;
use crate::handlers::toolbox::update;
use crate::{CtlConf, utils};

/// Edit an image using a text editor, detect the updates, then update the image
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl conf
/// * `cmd` - The edit image command that was run
pub async fn edit(thorium: Thorium, conf: &CtlConf, cmd: &EditImage) -> Result<(), Error> {
    let group = if let Some(group) = &cmd.group {
        group.clone()
    } else {
        utils::images::find_image_group(&thorium, &cmd.image).await?
    };
    let image = thorium.images.get(&group, &cmd.image).await?;
    let mergeable = MergeableImage::from(image.clone());
    let yaml = serde_yaml::to_string(&mergeable)
        .map_err(|err| Error::new(format!("Failed to serialize image to YAML: {err}")))?;

    let editor_cmd = cmd.editor.as_ref().unwrap_or(&conf.default_editor);
    let label = format!("{}-{}", group, cmd.image);
    let resolved: MergeableImage = match editor::editor_loop(&yaml, &label, editor_cmd).await? {
        Some(resolved) => resolved,
        None => {
            println!("Cancelled.");
            return Ok(());
        }
    };

    let image_update =
        update::calculate_image_update_from_mergeable(image, resolved)?;
    match image_update {
        Some(image_update) => {
            thorium
                .images
                .update(&group, &cmd.image, &image_update)
                .await?;
            println!(
                "{} {} {}",
                "Image".bright_green(),
                format!("'{}:{}'", group, cmd.image).yellow(),
                "updated successfully!".bright_green()
            );
        }
        None => {
            println!("No changes detected! Exiting...");
        }
    }
    Ok(())
}
