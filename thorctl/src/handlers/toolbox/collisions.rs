//! Resolution of `(group, name)` collisions in a toolbox manifest
//!
//! A toolbox import collapses every resource into one group when
//! `--group-override` is used (and a hand-authored manifest may already
//! double-define a name). When two image or pipeline entries resolve to the same
//! Thorium `(group, name)` identity, importing both would silently overwrite one
//! with the other. This module detects those collisions and resolves them before
//! any writes happen:
//!
//! - pure duplicates (byte-identical configs) are de-duped silently
//! - in an interactive session, the user chooses per collision to **Rename** the
//!   extras (dependent pipelines are repointed automatically) or **Skip** them
//! - otherwise the colliding resources, and any pipelines that depend on them,
//!   are skipped with a warning so the rest of the toolbox still imports

use colored::Colorize;
use std::collections::{HashMap, HashSet};
use thorium::Error;

use super::manifest::{Collision, CollisionMember, SourceGroups, ToolboxManifest};
use super::prompt;
use crate::handlers::progress::Bar;

/// The action to take on a real (non-duplicate) collision
enum CollisionAction {
    /// Keep every colliding resource by renaming the extras
    Rename,
    /// Skip the colliding resources (and, for images, dependent pipelines)
    Skip,
}

/// Detect and resolve every image and pipeline collision in the manifest
///
/// Returns a map of each renamed image's new manifest key to its original key.
/// Bundled image tarballs are saved on disk under the original key, so callers that
/// push bundled images need this to find an archive after its image was renamed.
///
/// # Arguments
///
/// * `manifest` - The manifest to mutate in place (rename/skip resolutions)
/// * `sources` - Pre-override groups captured before [`ToolboxManifest::override_group`]
/// * `can_prompt` - Whether the session can ask the user (TTY, not `-y`)
/// * `progress` - The progress bar, suspended while prompting
pub fn resolve_collisions(
    manifest: &mut ToolboxManifest,
    sources: &SourceGroups,
    can_prompt: bool,
    progress: &Bar,
) -> Result<HashMap<String, String>, Error> {
    // collect image renames as new-key -> original-key so bundled tarballs (named by
    // the original on-disk key) stay resolvable after a rename
    let image_renames = resolve_kind::<ImageCollisions>(manifest, sources, can_prompt, progress)?;
    // pipelines have no bundled tarballs, so their renames need no remap and are dropped
    resolve_kind::<PipelineCollisions>(manifest, sources, can_prompt, progress)?;
    Ok(image_renames.into_iter().collect())
}

/// The per-kind operations the generic resolver dispatches through
///
/// Image and pipeline resolution share one control flow (de-dupe → prompt →
/// rename-or-skip); only these manifest operations differ — notably the
/// image-only cascade of dropping pipelines that depended on a skipped image.
trait CollisionKind {
    /// The resource noun used in prompts and messages ("image"/"pipeline")
    const NOUN: &'static str;
    /// Detect this kind's `(group, name)` collisions
    fn detect(manifest: &ToolboxManifest, sources: &SourceGroups) -> Result<Vec<Collision>, Error>;
    /// De-dupe a byte-identical collision down to a single entry
    fn dedupe(manifest: &mut ToolboxManifest, collision: &Collision);
    /// A suggested unused rename for a colliding member
    fn suggested_rename(
        manifest: &ToolboxManifest,
        collision: &Collision,
        member: &CollisionMember,
    ) -> String;
    /// The names already used in `group` (a rename must avoid these)
    fn names_in_group(manifest: &ToolboxManifest, group: &str) -> HashSet<String>;
    /// Rename a member and repoint anything that referenced it
    fn rename_member(
        manifest: &mut ToolboxManifest,
        collision: &Collision,
        member: &CollisionMember,
        new_name: &str,
        sources: &SourceGroups,
    );
    /// Drop the colliding identity and warn; images also drop dependent pipelines
    fn skip(manifest: &mut ToolboxManifest, collision: &Collision, progress: &Bar);
}

/// Image collision operations
struct ImageCollisions;
/// Pipeline collision operations
struct PipelineCollisions;

impl CollisionKind for ImageCollisions {
    const NOUN: &'static str = "image";
    /// Detect colliding image identities across source groups
    fn detect(manifest: &ToolboxManifest, sources: &SourceGroups) -> Result<Vec<Collision>, Error> {
        manifest.detect_image_collisions(sources)
    }
    /// De-dupe a byte-identical image collision down to a single entry
    fn dedupe(manifest: &mut ToolboxManifest, collision: &Collision) {
        manifest.dedupe_image_collision(collision);
    }
    /// A suggested unused rename for a colliding image member
    fn suggested_rename(
        manifest: &ToolboxManifest,
        collision: &Collision,
        member: &CollisionMember,
    ) -> String {
        manifest.suggested_image_rename(collision, member)
    }
    /// The image names already used in `group`
    fn names_in_group(manifest: &ToolboxManifest, group: &str) -> HashSet<String> {
        manifest.image_names_in_group(group)
    }
    /// Rename an image member and repoint the pipelines that referenced it
    fn rename_member(
        manifest: &mut ToolboxManifest,
        collision: &Collision,
        member: &CollisionMember,
        new_name: &str,
        sources: &SourceGroups,
    ) {
        // repoint the pipelines that wanted each renamed image variant
        manifest.rename_image_member(collision, member, new_name, sources);
    }
    /// Drop the colliding image identity (and its dependent pipelines) and warn
    fn skip(manifest: &mut ToolboxManifest, collision: &Collision, progress: &Bar) {
        let dropped =
            manifest.remove_image_identity_and_dependents(&collision.group, &collision.name);
        warn_skipped_image(progress, collision, &dropped);
    }
}

impl CollisionKind for PipelineCollisions {
    const NOUN: &'static str = "pipeline";
    /// Detect colliding pipeline identities across source groups
    fn detect(manifest: &ToolboxManifest, sources: &SourceGroups) -> Result<Vec<Collision>, Error> {
        manifest.detect_pipeline_collisions(sources)
    }
    /// De-dupe a byte-identical pipeline collision down to a single entry
    fn dedupe(manifest: &mut ToolboxManifest, collision: &Collision) {
        manifest.dedupe_pipeline_collision(collision);
    }
    /// A suggested unused rename for a colliding pipeline member
    fn suggested_rename(
        manifest: &ToolboxManifest,
        collision: &Collision,
        member: &CollisionMember,
    ) -> String {
        manifest.suggested_pipeline_rename(collision, member)
    }
    /// The pipeline names already used in `group`
    fn names_in_group(manifest: &ToolboxManifest, group: &str) -> HashSet<String> {
        manifest.pipeline_names_in_group(group)
    }
    /// Rename a pipeline member (no cascade — pipelines aren't referenced by name)
    fn rename_member(
        manifest: &mut ToolboxManifest,
        _collision: &Collision,
        member: &CollisionMember,
        new_name: &str,
        _sources: &SourceGroups,
    ) {
        // pipelines aren't referenced by name elsewhere, so there's no cascade
        manifest.rename_pipeline_member(member, new_name);
    }
    /// Drop the colliding pipeline identity and warn
    fn skip(manifest: &mut ToolboxManifest, collision: &Collision, progress: &Bar) {
        manifest.remove_pipeline_identity(&collision.group, &collision.name);
        progress.warning(format!(
            "Skipping colliding pipeline '{}:{}' ({} conflicting definitions: {})",
            collision.group.bright_yellow(),
            collision.name.bright_yellow(),
            collision.members.len(),
            describe_members("pipeline", collision),
        ));
    }
}

/// Resolve every collision of one kind: de-dupe duplicates, rename or skip real
/// conflicts
///
/// Distinct collision identities don't affect each other, so resolving against the
/// snapshot taken up front is safe. Returns the renames performed as `(new manifest
/// key, original manifest key)` pairs so callers can remap on-disk artifacts named
/// by the original key.
///
/// # Arguments
///
/// * `manifest` - The manifest to mutate in place (rename/skip resolutions)
/// * `sources` - Pre-override groups used to disambiguate which variant a pipeline wanted
/// * `can_prompt` - Whether the session can ask the user (TTY, not `-y`)
/// * `progress` - The progress bar, suspended while prompting
fn resolve_kind<K: CollisionKind>(
    manifest: &mut ToolboxManifest,
    sources: &SourceGroups,
    can_prompt: bool,
    progress: &Bar,
) -> Result<Vec<(String, String)>, Error> {
    // record each rename so the caller can remap artifacts keyed by the original name
    let mut renames = Vec::new();
    for collision in K::detect(manifest, sources)? {
        if collision.identical {
            K::dedupe(manifest, &collision);
            progress.info_anonymous(format!(
                "De-duplicated identical {} '{}:{}' ({} copies)",
                K::NOUN,
                collision.group.bright_yellow(),
                collision.name.bright_yellow(),
                collision.members.len(),
            ));
            continue;
        }
        let action = if can_prompt {
            progress.suspend(|| prompt_collision_action(K::NOUN, &collision))?
        } else {
            CollisionAction::Skip
        };
        match action {
            CollisionAction::Rename => {
                // let the user pick which entry keeps the original name; rename the rest
                let keep = progress.suspend(|| prompt_keep_member(K::NOUN, &collision))?;
                for (index, member) in collision.members.iter().enumerate() {
                    if index == keep {
                        continue;
                    }
                    let suggested = K::suggested_rename(manifest, &collision, member);
                    // names already used in the target group are off-limits, so a
                    // rename can't introduce a fresh collision
                    let taken = K::names_in_group(manifest, &collision.group);
                    let new_name = progress.suspend(|| {
                        prompt_new_name(K::NOUN, &collision.name, member, &suggested, &taken)
                    })?;
                    K::rename_member(manifest, &collision, member, &new_name, sources);
                    // map the new key back to the original on-disk key (the member's
                    // manifest key) so a bundled tarball saved under it is still found
                    renames.push((new_name.clone(), member.manifest_key.clone()));
                }
                progress.info_anonymous(format!(
                    "Renamed colliding {} '{}:{}' into {} distinct {}s",
                    K::NOUN,
                    collision.group.bright_yellow(),
                    collision.name.bright_yellow(),
                    collision.members.len(),
                    K::NOUN,
                ));
            }
            CollisionAction::Skip => K::skip(manifest, &collision, progress),
        }
    }
    Ok(renames)
}

/// Prompt the user for how to resolve a collision
///
/// # Arguments
///
/// * `kind` - The resource noun ("image" or "pipeline") shown in the prompt
/// * `collision` - The collision whose members are listed for the user
fn prompt_collision_action(kind: &str, collision: &Collision) -> Result<CollisionAction, Error> {
    // describe the collision and list every conflicting member
    println!(
        "\n{} {} '{}:{}' is defined {} times; the copies would overwrite each other:",
        "Collision:".bright_yellow(),
        kind,
        collision.group.bright_blue(),
        collision.name.bright_blue(),
        collision.members.len(),
    );
    for member in &collision.members {
        println!(
            "  - manifest entry '{}' version '{}' (from group '{}')",
            member.manifest_key, member.version, member.source_group,
        );
    }
    // offer the two resolution choices, rename first as the default
    let items = &[
        "Rename - keep all of them; rename the extras (dependent pipelines are repointed)",
        "Skip   - skip these and any pipelines that depend on them",
    ];
    // read the user's selection
    let selection = dialoguer::Select::new()
        .items(items)
        .default(0)
        .interact()
        .map_err(|err| Error::new(format!("Failed to read user input: {err}")))?;
    // map the selected index to its action
    Ok(match selection {
        0 => CollisionAction::Rename,
        _ => CollisionAction::Skip,
    })
}

/// Describe a collision member by its manifest key and full config identity
/// (`<source group>/<config name>:<version>`), so the distinguishing fields are
/// all visible when choosing which entry to keep or rename
///
/// # Arguments
///
/// * `noun` - The resource kind ("image" or "pipeline") for the message
/// * `name` - The shared config name of the collision (same for every member)
/// * `member` - The member to describe
fn describe_member(noun: &str, name: &str, member: &CollisionMember) -> String {
    format!(
        "manifest '{}' — {noun} '{}/{}:{}'",
        member.manifest_key, member.source_group, name, member.version,
    )
}

/// Prompt for which colliding member should keep the original name; the rest are renamed
///
/// # Arguments
///
/// * `noun` - The resource kind ("image" or "pipeline") for each member's description
/// * `collision` - The collision whose members are offered as choices
fn prompt_keep_member(noun: &str, collision: &Collision) -> Result<usize, Error> {
    // explain that the chosen entry keeps the name and the rest are renamed
    println!(
        "\nWhich entry should keep the name '{}'? The others will be renamed.",
        collision.name.bright_blue(),
    );
    // render each member as a selectable, fully-qualified description
    let items: Vec<String> = collision
        .members
        .iter()
        .map(|member| describe_member(noun, &collision.name, member))
        .collect();
    // read the index of the member the user wants to keep
    dialoguer::Select::new()
        .items(&items)
        .default(0)
        .interact()
        .map_err(|err| Error::new(format!("Failed to read user input: {err}")))
}

/// Prompt for a new name for a renamed collision member, prefilled with `suggested`
/// and rejecting any name already used in the target group (`taken`)
///
/// # Arguments
///
/// * `noun` - The resource kind ("image" or "pipeline") for the member's description
/// * `old_name` - The original colliding name shown in the member's description
/// * `member` - The member being renamed
/// * `suggested` - The default name prefilled into the prompt
/// * `taken` - The names already used in the target group, rejected by validation
fn prompt_new_name(
    noun: &str,
    old_name: &str,
    member: &CollisionMember,
    suggested: &str,
    taken: &HashSet<String>,
) -> Result<String, Error> {
    // prompt for a new name, prefilled with the suggestion and rejecting taken names
    dialoguer::Input::new()
        .with_prompt(format!("New name for {}", describe_member(noun, old_name, member)))
        .default(suggested.to_string())
        .validate_with(|value: &String| {
            // enforce the same name rules as any other resource name
            prompt::validate_name(value)?;
            // reject a name already used by another resource in the target group
            if taken.contains(value) {
                return Err(format!(
                    "'{value}' is already used by another resource in this group; pick a different name"
                ));
            }
            Ok(())
        })
        .interact_text()
        .map_err(|err| Error::new(format!("Failed to read new name: {err}")))
}

/// Warn that a colliding image (and its dependent pipelines) were skipped
///
/// # Arguments
///
/// * `progress` - The progress bar to route the warnings through
/// * `collision` - The skipped image collision being described
/// * `dropped_pipelines` - The pipelines dropped because they depended on the image
fn warn_skipped_image(progress: &Bar, collision: &Collision, dropped_pipelines: &[String]) {
    // warn that the colliding image identity was skipped, listing its members
    progress.warning(format!(
        "Skipping colliding image '{}:{}' ({} conflicting definitions: {})",
        collision.group.bright_yellow(),
        collision.name.bright_yellow(),
        collision.members.len(),
        describe_members("image", collision),
    ));
    // warn about each pipeline that was dropped along with the image
    for pipeline in dropped_pipelines {
        progress.warning(format!(
            "  └ also skipping dependent pipeline '{}'",
            pipeline.bright_yellow()
        ));
    }
}

/// A comma-separated description of every member of a collision
///
/// # Arguments
///
/// * `noun` - The resource kind ("image" or "pipeline") for each member's description
/// * `collision` - The collision whose members are described
fn describe_members(noun: &str, collision: &Collision) -> String {
    collision
        .members
        .iter()
        .map(|member| describe_member(noun, &collision.name, member))
        .collect::<Vec<_>>()
        .join(", ")
}
