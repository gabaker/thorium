//! Creating a tool from a stored image
//!
//! Images can no longer be created or changed, but the ones that exist still describe how a job
//! runs. A snapshot converts one into an Image tool with the same conversion the migration of
//! stored images uses, so the tool's spec (and its hash) is the one the migration produces.

use thorium::client::migrations::{ImageContext, ToolIdMap, convert_image};
use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadataRequest, EntityRequest, ToolEntityRequest,
};
use thorium::{Error, Thorium};

use crate::args::tools::SnapshotTool;

/// Split `GROUP/IMAGE` into its group and image name
///
/// # Arguments
///
/// * `raw` - The image as typed
pub fn split_image(raw: &str) -> Result<(&str, &str), Error> {
    // both a group and an image name are needed
    match raw.split_once('/') {
        Some((group, image)) if !group.is_empty() && !image.is_empty() && !image.contains('/') => {
            Ok((group, image))
        }
        _ => Err(Error::new(format!(
            "`{raw}` must name an image as GROUP/IMAGE"
        ))),
    }
}

/// Find a tool with a name in some groups
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `name` - The tool's name
/// * `groups` - The groups to look in
async fn existing(
    thorium: &Thorium,
    name: &str,
    groups: &[String],
) -> Result<Option<Entity>, Error> {
    // list the tools in those groups page by page
    let opts = EntityListOpts::default()
        .groups(groups.to_vec())
        .kind(EntityKinds::Tool);
    let mut cursor = thorium.entities.list_details(&opts).await?;
    loop {
        // stop at a tool with the name
        if let Some(found) = cursor.data.drain(..).find(|tool| tool.name == name) {
            return Ok(Some(found));
        }
        // stop once every page was read
        if cursor.exhausted() {
            return Ok(None);
        }
        // get the next page of tools
        cursor.refill().await?;
    }
}

/// Create a tool from a stored image
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The snapshot command
pub async fn snapshot(thorium: &Thorium, cmd: &SnapshotTool) -> Result<(), Error> {
    // read the image
    let (group, name) = split_image(&cmd.image)?;
    let image = thorium.images.get(group, name).await?;
    // convert it the way the migration of stored images does
    let map = ToolIdMap::from_images([&image]);
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    let migration = convert_image(&image, &ctx).map_err(|skipped| {
        Error::new(format!(
            "Image {group}/{name} can't become a tool: {}; {}",
            skipped.reason,
            skipped.reason.remedy()
        ))
    })?;
    let tool = migration.tool;
    // the tool keeps the image's name, version and group unless told otherwise
    let tool_name = cmd.name.clone().unwrap_or_else(|| tool.name.clone());
    let version = cmd
        .version
        .clone()
        .unwrap_or_else(|| tool.version.to_string());
    let groups = if cmd.group.is_empty() {
        vec![tool.group.clone()]
    } else {
        cmd.group.clone()
    };
    // a tool with that name in those groups is never replaced
    if let Some(found) = existing(thorium, &tool_name, &groups).await? {
        return Err(Error::new(format!(
            "A tool named {tool_name} already exists ({}); pass -n to snapshot under another name",
            found.id
        )));
    }
    // say what the conversion noted and warned about, never printing a secret value
    for note in migration.notes.iter().chain(&migration.warnings) {
        eprintln!("Note: {}", note.note);
    }
    // a dry run says what would be created
    if cmd.dry_run {
        println!(
            "Would create Image tool {tool_name} with version {version} in {} from image {group}/{name}",
            groups.join(", ")
        );
        return Ok(());
    }
    // create the tool with the converted spec
    let meta = ToolEntityRequest::new(&version, &tool.spec)?;
    let mut req = EntityRequest::new(&tool_name, EntityMetadataRequest::Tool(meta), groups);
    req.description = tool.description;
    req.tags = tool.tags;
    let resp = thorium.entities.create(req).await?;
    println!(
        "Created tool {} ({}) with version {version} from image {group}/{name}",
        resp.name, resp.id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_image;

    /// An image is named by its group and name
    #[test]
    fn splits_images() {
        // a group and a name
        assert_eq!(split_image("corn/scan").unwrap(), ("corn", "scan"));
        // anything else is refused
        for bad in ["scan", "/scan", "corn/", "a/b/c", ""] {
            assert!(split_image(bad).is_err(), "{bad}");
        }
    }
}
