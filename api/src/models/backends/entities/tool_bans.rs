//! Keeps the bans of legacy images on the tool instances they converted into
//!
//! A cluster that converted its images to tools before conversions carried bans has unbanned
//! instances for banned images. The consistency scan the API runs at startup backfills those
//! bans onto every instance of each converted tool, lifting a carried ban from an instance also
//! lifts it from the legacy image so a later scan doesn't add it back, and the audit lists every
//! banned legacy image with each instance of its converted tool.

use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::tools;
use crate::client::migrations::ToolIdMap;
use crate::models::backends::{NotificationSupport, db};
use crate::models::bans::Ban;
use crate::models::{
    Entity, EntityMetadata, Image, ImageBan, ImageBanKind, ImageBanUpdate, ImageKey, User,
    UserRole, UserSettings,
};
use crate::utils::{ApiError, Shared};

/// One ban of a legacy image, as the audit reports it
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyBanAudit {
    /// The ban's id, which the converted instances' copies of the ban share
    pub id: Uuid,
    /// The kind of ban (`Generic`, `InvalidImageUrl`, or `InvalidHostPath`)
    pub kind: String,
    /// Why the image was banned
    pub reason: String,
    /// Whether this kind of ban is carried to the converted instances
    pub carried: bool,
    /// Whether every readable instance of the converted tool has this ban
    pub on_every_instance: bool,
}

/// One instance of the tool a legacy image converted into, as the audit reports it
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConvertedInstanceBans {
    /// The tool instance's id
    pub id: Uuid,
    /// The ids of the image's carried bans this instance lacks
    pub missing: Vec<Uuid>,
    /// Why the instance couldn't be read, if it couldn't
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A legacy image with bans and every instance of the tool it converted into, as the audit
/// reports it
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyImageBans {
    /// The image's group
    pub group: String,
    /// The image's name
    pub image: String,
    /// The id of the tool the image converts into
    pub tool: Uuid,
    /// The id of the first tool instance the image converts into
    pub first_instance: Uuid,
    /// Every existing instance of the converted tool (none for an image not converted yet)
    pub instances: Vec<ConvertedInstanceBans>,
    /// The image's bans
    pub bans: Vec<LegacyBanAudit>,
    /// Whether every instance was read and has every ban it should carry
    pub covered: bool,
    /// Why the converted tool's instances couldn't be listed, if they couldn't
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Get the name of an image ban's kind
///
/// # Arguments
///
/// * `kind` - The kind of ban
fn kind_name(kind: &ImageBanKind) -> &'static str {
    // name each kind as its variant is named
    match kind {
        ImageBanKind::Generic(_) => "Generic",
        ImageBanKind::InvalidImageUrl(_) => "InvalidImageUrl",
        ImageBanKind::InvalidHostPath(_) => "InvalidHostPath",
    }
}

/// Whether a ban of a legacy image is carried to the tool instance it converted into
///
/// # Arguments
///
/// * `ban` - The image's ban
#[must_use]
pub fn carried(ban: &ImageBan) -> bool {
    // every kind of ban keeps the instance from running as it kept the image from running
    match ban.ban_kind {
        ImageBanKind::Generic(_)
        | ImageBanKind::InvalidImageUrl(_)
        | ImageBanKind::InvalidHostPath(_) => true,
    }
}

/// Get the bans of a legacy image that carry to its converted instance
///
/// # Arguments
///
/// * `image` - The legacy image
#[must_use]
pub fn carried_bans(image: &Image) -> BTreeMap<Uuid, ImageBan> {
    // keep only the kinds an instance carries
    image
        .bans
        .iter()
        .filter(|(_, ban)| carried(ban))
        .map(|(id, ban)| (*id, ban.clone()))
        .collect()
}

/// Read a tool instance in every one of its groups, or `None` if it doesn't exist
///
/// # Arguments
///
/// * `id` - The id of the tool instance
/// * `shared` - Shared Thorium objects
async fn read_instance(id: Uuid, shared: &Shared) -> Result<Option<Entity>, ApiError> {
    // an instance with no rows doesn't exist
    let groups = match db::entities::all_groups(id, shared).await {
        Ok(groups) => groups,
        Err(ApiError::Thorium { code, .. }) if code == StatusCode::NOT_FOUND => return Ok(None),
        Err(error) => return Err(error),
    };
    // read it in all of them, so a rewrite keeps every group's row
    let entity = db::entities::get_untagged(&groups, id, shared).await?;
    Ok(Some(entity))
}

/// Get the bans of a tool instance entity, or `None` if it isn't a tool instance
///
/// # Arguments
///
/// * `entity` - The entity
fn instance_bans(entity: &Entity) -> Option<&BTreeMap<Uuid, ImageBan>> {
    // only a tool instance carries bans
    match &entity.metadata {
        EntityMetadata::ToolInstance(instance) => Some(&instance.bans),
        _ => None,
    }
}

/// Build the admin the scan and the audit list a converted tool's instances as
///
/// Listing a tool's instances reads its edges as some user; the scan and the audit read every
/// group, so they read as an admin without a name, like the API's startup scan.
fn scan_user() -> User {
    // an admin in no groups; the edges are read in the tool's own groups
    User {
        username: String::default(),
        password: None,
        email: String::default(),
        role: UserRole::Admin,
        groups: Vec::default(),
        actual_groups: Vec::default(),
        token: String::default(),
        token_expiration: Utc::now(),
        unix: None,
        settings: UserSettings::default(),
        verified: false,
        verification_token: None,
        verification_sent: None,
        aliases: HashMap::default(),
        image: None,
    }
}

/// Get the ids of every instance of the tool a legacy image converted into, first instance first
///
/// The first instance's id derives from the image's group and name, and every later version is
/// found from the tool's `PublishedFrom` edges. An image not converted yet has no tool, so only its
/// first instance's id is listed, which doesn't exist either.
///
/// # Arguments
///
/// * `image` - The legacy image
/// * `shared` - Shared Thorium objects
async fn converted_instance_ids(image: &Image, shared: &Shared) -> Result<Vec<Uuid>, ApiError> {
    // the first instance is always listed, so a missing edge can't hide it
    let first = ToolIdMap::first_instance_id(&image.group, &image.name);
    let mut ids = vec![first];
    // an image not converted yet has no tool
    let tool = ToolIdMap::tool_id(&image.group, &image.name);
    let groups = match db::entities::all_groups(tool, shared).await {
        Ok(groups) => groups,
        Err(ApiError::Thorium { code, .. }) if code == StatusCode::NOT_FOUND => return Ok(ids),
        Err(error) => return Err(error),
    };
    // every version published since is linked to the tool
    for id in tools::instance_ids(&scan_user(), tool, &groups, shared).await? {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// List every legacy image with bans, every instance of the tool it converted into, and which of
/// the image's bans each instance lacks
///
/// An instance or tool that can't be read is reported with an error and leaves its image
/// uncovered; the audit goes on to the rest.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[instrument(name = "tool_bans::audit", skip_all, err(Debug))]
pub async fn audit(shared: &Shared) -> Result<Vec<LegacyImageBans>, ApiError> {
    // read every legacy image that parses
    let images = db::images::backup(shared).await?.0;
    let mut audits = Vec::new();
    for image in images.iter().filter(|image| !image.bans.is_empty()) {
        // read each instance of the converted tool and the bans it has
        let carry = carried_bans(image);
        let mut instances = Vec::new();
        let mut held = Vec::new();
        let error = match converted_instance_ids(image, shared).await {
            Ok(ids) => {
                for id in ids {
                    match read_instance(id, shared).await {
                        // an instance that doesn't exist (or isn't an instance) can't run
                        Ok(None) => (),
                        Ok(Some(entity)) => {
                            let Some(has) = instance_bans(&entity) else {
                                continue;
                            };
                            let missing = carry
                                .keys()
                                .filter(|ban| !has.contains_key(*ban))
                                .copied()
                                .collect();
                            held.push(has.keys().copied().collect::<Vec<Uuid>>());
                            instances.push(ConvertedInstanceBans {
                                id,
                                missing,
                                error: None,
                            });
                        }
                        // report an unreadable instance and go on to the next
                        Err(error) => instances.push(ConvertedInstanceBans {
                            id,
                            missing: Vec::new(),
                            error: Some(error.to_string()),
                        }),
                    }
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };
        // report each of the image's bans, ordered by id
        let mut bans = image
            .bans
            .values()
            .map(|ban| LegacyBanAudit {
                id: ban.id,
                kind: kind_name(&ban.ban_kind).to_owned(),
                reason: ban.msg(),
                carried: carried(ban),
                on_every_instance: held.iter().all(|has| has.contains(&ban.id)),
            })
            .collect::<Vec<_>>();
        bans.sort_by_key(|ban| ban.id);
        // an image is covered when every instance was read and has every ban it carries
        let covered = error.is_none()
            && instances
                .iter()
                .all(|inst| inst.error.is_none() && inst.missing.is_empty());
        audits.push(LegacyImageBans {
            group: image.group.clone(),
            image: image.name.clone(),
            tool: ToolIdMap::tool_id(&image.group, &image.name),
            first_instance: ToolIdMap::first_instance_id(&image.group, &image.name),
            instances,
            bans,
            covered,
            error,
        });
    }
    // list them by group and image
    audits.sort_by(|a, b| (&a.group, &a.image).cmp(&(&b.group, &b.image)));
    Ok(audits)
}

/// Add the bans of legacy images to every instance of the tools they converted into that lacks
/// them
///
/// A ban an instance already has (by id) is left alone, so running this again changes nothing.
/// An image that hasn't converted yet is skipped; its conversion writes the bans with the
/// instance. A tool or instance that can't be read or written is logged and skipped, and the
/// rest are still backfilled, so one bad record never fails the consistency scan (which would
/// stop the API starting) or the scans after it. An unreadable instance can't run either, since
/// every run path reads it first.
///
/// # Arguments
///
/// * `images` - The legacy images, with the bans they have
/// * `shared` - Shared Thorium objects
#[instrument(name = "tool_bans::backfill", skip_all)]
pub(crate) async fn backfill(images: &[Image], shared: &Shared) {
    let (mut added, mut failed) = (0_usize, 0_usize);
    for image in images {
        // only the bans an instance carries are backfilled
        let bans = carried_bans(image);
        if bans.is_empty() {
            continue;
        }
        // find every instance of the converted tool, skipping the image if they can't be listed
        let ids = match converted_instance_ids(image, shared).await {
            Ok(ids) => ids,
            Err(error) => {
                failed += 1;
                event!(
                    Level::ERROR,
                    msg = "Failed to list a converted tool's instances to carry a legacy image's bans",
                    group = image.group,
                    image = image.name,
                    error = error.to_string()
                );
                continue;
            }
        };
        // add the bans to each instance that lacks one, going on past any that fails
        for id in ids {
            match Box::pin(backfill_instance(id, &bans, shared)).await {
                Ok(true) => {
                    added += 1;
                    event!(
                        Level::INFO,
                        msg = "Carried a legacy image's bans to a converted tool instance",
                        group = image.group,
                        image = image.name,
                        instance = id.to_string()
                    );
                }
                Ok(false) => (),
                Err(error) => {
                    failed += 1;
                    event!(
                        Level::ERROR,
                        msg = "Failed to carry a legacy image's bans to a converted tool instance",
                        group = image.group,
                        image = image.name,
                        instance = id.to_string(),
                        error = error.to_string()
                    );
                }
            }
        }
    }
    // summarize the pass when it changed or skipped anything
    if added > 0 || failed > 0 {
        event!(
            Level::WARN,
            msg = "Backfilled legacy image bans onto converted tool instances",
            instances_banned = added,
            failures = failed
        );
    }
}

/// Add bans to a tool instance that lacks any of them, under its tool's lock
///
/// Returns whether the instance was rewritten.
///
/// # Arguments
///
/// * `id` - The tool instance's id
/// * `bans` - The bans it should have
/// * `shared` - Shared Thorium objects
async fn backfill_instance(
    id: Uuid,
    bans: &BTreeMap<Uuid, ImageBan>,
    shared: &Shared,
) -> Result<bool, ApiError> {
    // check without a lock first, so a scan over instances that already have their bans takes
    // no locks and writes nothing
    let Some(entity) = read_instance(id, shared).await? else {
        return Ok(false);
    };
    let Some(has) = instance_bans(&entity) else {
        return Ok(false);
    };
    if bans.keys().all(|ban| has.contains_key(ban)) {
        return Ok(false);
    }
    // the instance shares its tool's lock
    let Some(root) = tools::lock_root(&entity) else {
        return Ok(false);
    };
    let token = tools::lock(root, &entity.name, shared).await?;
    // re-read it under the lock, so a concurrent ban change isn't dropped
    let result = match read_instance(id, shared).await {
        Ok(Some(mut current)) => {
            if let EntityMetadata::ToolInstance(instance) = &mut current.metadata {
                // add each ban it lacks, keeping the ones it has
                for (id, ban) in bans {
                    instance.bans.entry(*id).or_insert_with(|| ban.clone());
                }
                db::entities::set_tool_instance_bans(current, shared)
                    .await
                    .map(|()| true)
            } else {
                Ok(false)
            }
        }
        Ok(None) => Ok(false),
        Err(error) => Err(error),
    };
    tools::unlock_after_write(root, &token, shared).await;
    result
}

/// Lift bans from the legacy image a tool instance's tool converted from, so the consistency scan
/// doesn't carry them back to the tool's instances
///
/// A tool converted from an image has the id derived from the image's group and name, so only
/// that image is changed, whichever of the tool's instances the bans were lifted from. Bans the
/// image doesn't have are skipped.
///
/// # Arguments
///
/// * `user` - The admin lifting the bans
/// * `instance` - The tool instance the bans were lifted from
/// * `lifted` - The ids of the lifted bans
/// * `shared` - Shared Thorium objects
#[instrument(name = "tool_bans::lift_from_legacy_image", skip_all, err(Debug))]
pub(crate) async fn lift_from_legacy_image(
    user: &User,
    instance: &Entity,
    lifted: &[Uuid],
    shared: &Shared,
) -> Result<(), ApiError> {
    // nothing to lift without lifted bans
    if lifted.is_empty() {
        return Ok(());
    }
    // the image has the tool's name, which is the instance's name without its version
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        return Ok(());
    };
    let Some(name) = instance.name.strip_suffix(&format!("@{}", meta.version)) else {
        return Ok(());
    };
    // a tool converted from an image is in that image's group
    for group in db::entities::all_groups(instance.id, shared).await? {
        if ToolIdMap::tool_id(&group, name) != meta.tool {
            continue;
        }
        // the image may have been deleted since it converted
        let mut image = match db::images::get(&group, name, shared).await {
            Ok(image) => image,
            Err(ApiError::Thorium { code, .. }) if code == StatusCode::NOT_FOUND => continue,
            Err(error) => return Err(error),
        };
        // lift only the bans the image still has
        let removed = lifted
            .iter()
            .filter(|id| image.bans.contains_key(*id))
            .copied()
            .collect::<Vec<Uuid>>();
        if removed.is_empty() {
            continue;
        }
        ImageBanUpdate::default()
            .remove_bans(removed.clone())
            .update(&mut image, user)?;
        db::images::update(&image, shared).await?;
        // clear the notifications the lifted bans left on the image
        image
            .update_ban_notifications::<ImageBan>(&ImageKey::from(&image), &[], &removed, shared)
            .await?;
    }
    Ok(())
}
