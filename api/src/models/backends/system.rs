use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use futures::TryStreamExt;
use futures::stream::{self, StreamExt};
use itertools::Itertools;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use tracing::{Level, Span, event, instrument, span};
use uuid::Uuid;

use super::db::{self, SimpleCursorExt};
use super::{entities, users};
use crate::models::backends::NotificationSupport;
use crate::models::schema::queries::{NodesListCursor, NodesListDetailsCursor};
use crate::models::{
    AllowedEndpoint, ApiCursor, Backup, BackupParams, BackupProblem, BackupProblems, Group,
    GroupRequest, GroupUsers, GroupUsersRequest, HostPath, HostPathWhitelistUpdate, Image,
    ImageBan, ImageBanKind, ImageBanUpdate, ImageKey, ImageScaler, ModelsConfUpdate, Node,
    NodeGetParams, NodeListLine, NodeListParams, NodeRegistration, NodeRow, NodeUpdate, Pipeline,
    PipelineBan, PipelineBanKind, PipelineBanUpdate, PipelineKey, PruneAction, PrunedReference,
    PrunedReferent, RestoreParams, SystemInfo, SystemSettings, SystemSettingsUpdate, SystemStats,
    User, VolumeTypes, Worker, WorkerDeleteMap, WorkerRegistrationList, WorkerUpdate, conversions,
};
use crate::utils::{ApiError, Shared};
use crate::{
    bad, deserialize, deserialize_ext, deserialize_opt, extract, is_admin, log_scylla_err,
    not_found, unauthorized, update,
};

/// Check if Thorium is healthy
///
/// Currently this just checks redis but we should add scylla too.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
pub async fn health(shared: &Shared) -> Result<bool, ApiError> {
    db::system::health(shared).await
}

/// Returns a string denoting this server as a Thorium server
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
pub async fn iff(shared: &Shared) -> Result<String, ApiError> {
    // get the system IFF string from the backend
    db::system::iff(shared).await
}

impl SystemInfo {
    /// Initialize the system info data with default values
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is Initializing system info
    /// * `shared` - Shared Thorium objects
    /// * `span` - The span to log traces under
    #[instrument(name = "SystemInfo::init", skip_all, err(Debug))]
    pub async fn init(user: &User, shared: &Shared) -> Result<(), ApiError> {
        // only admins can get system info
        is_admin!(user);
        // get the system info from the backend
        db::system::init(shared).await?;
        // check if the system group already exists
        if !db::groups::exists(&["system".to_owned()], shared).await? {
            // the system group doesn't already exist so create a request to create it
            let req = GroupRequest::new("system")
                .owners(GroupUsersRequest::default().direct(&user.username));
            // save our group to the backend
            Group::create(user, req, shared).await?;
        }
        Ok(())
    }

    /// Gets system info on the data in the current backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting system info
    /// * `clear` - Whether to reset the flags denoting things like a stale cache
    /// * `shared` - Shared Thorium objects
    pub async fn get(
        user: &User,
        scaler: Option<ImageScaler>,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // only admins can get system info
        is_admin!(user);
        // get the system info from the backend
        db::system::get_info(scaler, shared).await
    }

    /// Sets the scaler cache reset flag
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting system info
    /// * `clear` - Whether to reset the flags denoting things like a stale cache
    /// * `shared` - Shared Thorium objects
    pub async fn reset_cache(user: &User, shared: &Shared) -> Result<(), ApiError> {
        // only admins can get system info
        is_admin!(user);
        // get the system info from the backend
        db::system::reset_cache(shared).await
    }
}

impl SystemStats {
    /// Gets statistics on the data in the current backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting system stats
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "SystemStats::get", skip_all, err(Debug))]
    pub async fn get(user: &User, shared: &Shared) -> Result<Self, ApiError> {
        // build a map of groups status objects
        let mut group_status = HashMap::default();
        // crawl all groups visible to this user
        // TODO we need to make this a cursor instead of a hardcoded max limit
        for group in Group::list_details(user, 0, 5000, shared).await?.details {
            // get the status of this group
            let status = group.stats(shared).await?;
            // add it to our map of statuses
            group_status.insert(group.name, status);
        }
        // get the system statistics from the backend
        db::system::get_stats(group_status, shared).await
    }
}

impl HostPathWhitelistUpdate {
    /// Update the [`SystemSettings`] with the contents of this host path whitelist update
    ///
    /// # Arguments
    ///
    /// * `settings` - The settings to update
    pub fn update(self, settings: &mut SystemSettings) -> Result<(), ApiError> {
        // validate the host path whitelist update
        self.validate(settings)?;
        // add the requested paths
        for path in self.add_paths {
            settings.host_path_whitelist.insert(path);
        }
        // remove the requested paths
        for path in self.remove_paths {
            settings.host_path_whitelist.remove(&path);
        }
        Ok(())
    }

    /// Validate a `HostPathWhitelistUpdate` before updating
    ///
    /// # Arguments
    ///
    /// * `settings` - The current system settings we're validating against
    fn validate(&self, settings: &SystemSettings) -> Result<(), ApiError> {
        // make sure all of the paths to add to the whitelist are valid host paths
        let bad_add_host_paths: Vec<String> = self
            .add_paths
            .iter()
            .filter_map(|path| {
                (!HostPath::is_valid(path)).then_some(path.to_string_lossy().into_owned())
            })
            .collect();
        if !bad_add_host_paths.is_empty() {
            return bad!(format!(
                "Invalid host path(s) '{bad_add_host_paths:?}'! Host paths must be absolute \
                    and not contain any relative traversal ('.', '..', etc.)"
            ));
        }
        // make sure all of the paths to remove from the whitelist are IN the whitelist
        let bad_remove_host_paths: Vec<String> = self
            .remove_paths
            .iter()
            .filter_map(|path| {
                (!settings.host_path_whitelist.contains(path))
                    .then_some(path.to_string_lossy().into_owned())
            })
            .collect();
        if !bad_remove_host_paths.is_empty() {
            return bad!(format!(
                "Invalid host path(s) '{bad_remove_host_paths:?}'! The requested host paths to remove are \
                not in the current host path whitelist"
            ));
        }
        Ok(())
    }
}

impl ModelsConfUpdate {
    /// Apply this update to the allowed model endpoints in the [`SystemSettings`]
    ///
    /// The list is cleared first if requested, then endpoints are added, then removed. Adding
    /// an allowed endpoint or removing one that isn't allowed changes nothing.
    ///
    /// # Arguments
    ///
    /// * `settings` - The settings to update
    pub fn update(self, settings: &mut SystemSettings) {
        // clear the list first if requested
        if self.clear_endpoints {
            settings.models.allowed_endpoints.clear();
        }
        // add then remove the requested endpoints, each with its url normalized
        settings.models.allowed_endpoints.extend(
            self.add_endpoints
                .into_iter()
                .map(AllowedEndpoint::normalized),
        );
        for endpoint in self.remove_endpoints {
            settings
                .models
                .allowed_endpoints
                .remove(&endpoint.normalized());
        }
    }
}

impl SystemSettings {
    /// Reset dynamic Thorium settings to their defaults
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reseting system settings
    /// * `shared` - Shared Thorium objects
    pub async fn reset(user: &User, shared: &Shared) -> Result<(), ApiError> {
        // only admins can reset system settings
        is_admin!(user);
        // reset the system settings in the backend
        db::system::reset_settings(shared).await
    }

    /// Get the currently configured system settings
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting system settings
    /// * `shared` - Shared Thorium objects
    pub async fn get(user: &User, shared: &Shared) -> Result<Self, ApiError> {
        // only admins can get system settings
        is_admin!(user);
        // get the system settings from the backend
        db::system::get_settings(shared).await
    }

    /// Update the current [`SystemSettings`]
    ///
    /// # Arguments
    ///
    /// * `settings` - The `SystemSettings` to update
    /// * `user` - The user that is getting system info
    /// * `shared` - Shared Thorium objects
    #[rustfmt::skip]
    pub async fn update(
        mut self,
        mut update: SystemSettingsUpdate,
        user: &User,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // only admins can update system settings
        is_admin!(user);
        // update the settings
        update!(self.reserved_cpu, update.reserved_cpu, conversions::cpu);
        update!(self.reserved_memory, update.reserved_memory, conversions::storage);
        update!(self.reserved_storage, update.reserved_storage, conversions::storage);
        update!(self.fairshare_cpu, update.fairshare_cpu, conversions::cpu);
        update!(self.fairshare_memory, update.fairshare_memory, conversions::storage);
        update!(self.fairshare_storage, update.fairshare_storage, conversions::storage);
        // update the host path whitelist
        update.host_path_whitelist.update(&mut self)?;
        // update the unrestricted host paths setting
        update!(
            self.allow_unrestricted_host_paths,
            update.allow_unrestricted_host_paths
        );
        // clear the whitelist if we're set to
        if update.clear_host_path_whitelist {
            self.host_path_whitelist.clear();
        }
        // update the allowed model endpoints
        update.models.update(&mut self);
        // update the system settings in the backend
        db::system::update_settings(&self, shared).await?;
        Ok(self)
    }

    /// A helper function for checking images in the consistency scan
    ///
    /// # Arguments
    ///
    /// * `image` - The image we're checking for consistency
    /// * `user` - The user who is running the scan
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "SystemSettings::scan_helper_images", skip_all, err(Debug))]
    async fn scan_helper_images(
        &self,
        mut image: Image,
        user: &User,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // get the list of bans to add and remove
        let (bans_added, bans_removed) = if self.allow_unrestricted_host_paths {
            // add no bans and remove all host path related bans if we allow all host paths
            let added: Vec<ImageBan> = Vec::new();
            let removed: Vec<Uuid> = image
                .bans
                .iter()
                .filter_map(|(ban_id, ban)| match &ban.ban_kind {
                    ImageBanKind::InvalidHostPath(_) => Some(*ban_id),
                    _ => None,
                })
                .collect();
            (added, removed)
        } else {
            // create a set of already banned paths so we don't accidentally add the same ban twice
            let banned_paths: HashSet<&PathBuf> = image
                .bans
                .values()
                .filter_map(|ban| match &ban.ban_kind {
                    ImageBanKind::InvalidHostPath(ban_type) => Some(&ban_type.host_path),
                    _ => None,
                })
                .collect();
            // find any volumes that have bad host paths and add a ban for each one
            let added: Vec<ImageBan> = image
                .volumes
                .iter()
                .filter_map(|vol| match vol.archetype {
                    VolumeTypes::HostPath => {
                        // attempt to get the host path or skip if there isn't one set
                        let host_path = &vol.host_path.as_ref()?.path;
                        if self.is_whitelisted_host_path(host_path) {
                            // skip if the host path is whitelisted
                            None
                        } else {
                            let host_path = PathBuf::from(host_path);
                            if banned_paths.contains(&host_path) {
                                // if the path is already banned, don't add a new one
                                None
                            } else {
                                // otherwise create a new ban
                                Some(ImageBan::new(ImageBanKind::host_path(&vol.name, host_path)))
                            }
                        }
                    }
                    _ => None,
                })
                .collect();
            // find any bans with host paths that are now whitelisted
            let removed: Vec<Uuid> = image
                .bans
                .iter()
                .filter_map(|(ban_id, ban)| {
                    match &ban.ban_kind {
                        ImageBanKind::InvalidHostPath(ban) => {
                            if self.is_whitelisted_host_path(&ban.host_path) {
                                // if the ban's host path is now whitelisted, set it to be removed
                                Some(*ban_id)
                            } else {
                                None
                            }
                        }
                        // skip if the ban isn't host path related
                        _ => None,
                    }
                })
                .collect();
            (added, removed)
        };
        // if we have any bans to add/remove, update the image with those bans
        if !bans_added.is_empty() || !bans_removed.is_empty() {
            // create a ban update with the new bans
            let ban_update = ImageBanUpdate::default()
                .add_bans(bans_added.clone())
                .remove_bans(bans_removed.clone());
            // add the bans to the image
            ban_update.update(&mut image, user)?;
            db::images::update(&image, shared).await?;
            // update ban notifications
            image
                .update_ban_notifications(
                    &ImageKey::from(&image),
                    &bans_added,
                    &bans_removed,
                    shared,
                )
                .await?;
        }
        Ok(())
    }

    /// A helper function for checking pipelines in the consistency scan
    ///
    /// # Arguments
    ///
    /// * `pipeline` - The pipeline we're checking for consistency
    /// * `user` - The user who is running the scan
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "SystemSettings::scan_helper_pipelines", skip_all, err(Debug))]
    async fn scan_helper_pipelines(
        &self,
        mut pipeline: Pipeline,
        user: &User,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // get a list of the pipeline's images
        let images = pipeline.order.iter().flatten().unique();
        // get a map of images that already have a ban in the pipeline and their ban ids
        let current_bans_images = pipeline
            .bans
            .values()
            .filter_map(|ban| match &ban.ban_kind {
                PipelineBanKind::BannedImage(ban_kind) => Some((&ban_kind.image, &ban.id)),
                _ => None,
            })
            .collect::<HashMap<&String, &Uuid>>();
        // calculate which pipeline bans to add/remove
        let (bans_added, bans_removed) = stream::iter(images)
            .map(Ok::<&std::string::String, ApiError>)
            .try_fold(
                (Vec::new(), Vec::new()),
                |(mut bans_added, mut bans_removed), image| {
                    let group = &pipeline.group;
                    let current_bans_images = &current_bans_images;
                    async move {
                        // get the image's bans
                        let bans = db::images::get_bans(group, image, shared).await?;
                        // determine if the image is banned
                        let image_banned = !bans.is_empty();
                        // determine if the image already has a ban in the pipeline's ban list
                        let image_pipeline_ban = current_bans_images.get(image);
                        match (image_banned, image_pipeline_ban) {
                            // the image is banned and the pipeline does not have it, so add a new ban
                            (true, None) => {
                                bans_added
                                    .push(PipelineBan::new(PipelineBanKind::image_ban(image)));
                            }
                            // the image is no longer banned but the pipeline still has it as a ban, so remove it
                            (false, Some(pipeline_ban)) => bans_removed.push(**pipeline_ban),
                            // either the image is banned and already in the pipeline bans or it isn't and it's not in the
                            // pipeline, so do nothing
                            _ => (),
                        }
                        Ok((bans_added, bans_removed))
                    }
                },
            )
            .await?;
        // update the pipeline's bans
        let ban_update = PipelineBanUpdate::default()
            .add_bans(bans_added.clone())
            .remove_bans(bans_removed.clone());
        ban_update.update(&mut pipeline, user)?;
        db::pipelines::update(&pipeline, &[], &[], shared).await?;
        // update the pipeline's ban notifications
        pipeline
            .update_ban_notifications(
                &PipelineKey::from(&pipeline),
                &bans_added,
                &bans_removed,
                shared,
            )
            .await?;
        Ok(())
    }

    /// Performs a scan of Thorium data, checking that all data is compliant with the settings in `self`
    /// and cleaning/marking/modifying data that isn't
    ///
    /// This applies to images with host path mounts that may not be on the configured whitelist
    /// after a settings update, to pipelines those images may be a part of, and to group and user
    /// model endpoints that may not be on the allowed model endpoint list
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is telling Thorium to scan
    #[instrument(name = "SystemSettings::consistency_scan", skip_all, err(Debug))]
    pub async fn consistency_scan(&self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // only admins can perform a scan
        is_admin!(user);
        // update all images that parse; records that don't parse are skipped
        stream::iter(db::images::backup(shared).await?.0)
            .map(Ok)
            .try_for_each_concurrent(1000, |image| self.scan_helper_images(image, user, shared))
            .await?;
        // update all pipelines that parse
        stream::iter(db::pipelines::backup(shared).await?.0)
            .map(Ok)
            .try_for_each_concurrent(1000, |pipeline| {
                self.scan_helper_pipelines(pipeline, user, shared)
            })
            .await?;
        // check every model endpoint against the allowed endpoint list
        self.scan_models(user, shared).await
    }

    /// Ban every group and user model endpoint outside the allowed endpoint list and clear that
    /// ban from every one inside it
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is telling Thorium to scan
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "SystemSettings::scan_models", skip_all, err(Debug))]
    pub async fn scan_models(&self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // only admins can perform a scan
        is_admin!(user);
        // ban or unban every group model endpoint against the allowed endpoint list
        Box::pin(entities::models::scan_whitelist(&self.models, user, shared)).await?;
        // ban or unban every user's own model endpoints against it too
        users::scan_whitelist(&self.models, shared).await
    }

    /// Checks if the given path or any of its parents are in the whitelist
    ///
    /// # Arguments
    ///
    /// * `path` - The path to check if it or any of its parents are on the whitelist
    pub fn is_whitelisted_host_path<T: Into<PathBuf>>(&self, path: T) -> bool {
        // convert to a PathBuf
        let path: PathBuf = path.into();
        if self.host_path_whitelist.contains(&path) {
            // return true if this path is in the whitelist
            true
        } else if let Some(parent) = path.parent() {
            // recursively check the parent directory
            self.is_whitelisted_host_path(parent)
        } else {
            // return false if we're out of parents to check
            false
        }
    }
}

/// How many times a backup reads Redis before it gives up on records that don't fit together
const BACKUP_READS: u32 = 3;

/// The bootstrap admin every deployment has, which a pruning backup makes the owner of a group
/// that would otherwise be left with none
const BOOTSTRAP_ADMIN: &str = "thorium";

/// A reference among a backup's users and groups that names a record the backup doesn't hold
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Dangling {
    /// A group role naming a user
    Member {
        /// The group
        group: String,
        /// The role
        role: &'static str,
        /// The user
        user: String,
    },
    /// A user's groups naming a group
    Membership {
        /// The user
        user: String,
        /// The group
        group: String,
    },
    /// A scoped token's groups naming a group
    TokenGroup {
        /// The token's owner
        owner: String,
        /// The token's name
        name: String,
        /// The group
        group: String,
    },
}

impl Backup {
    /// Gets a backup of the current server
    ///
    /// Fails with 422, listing every record's key, when any user, group, image, pipeline, or
    /// scoped token doesn't parse, rather than leaving it out of the backup; since a record deleted
    /// between listing and reading it reads as one that doesn't parse, the records are read again
    /// a few times before the backup fails. The backup must
    /// also pass the checks a restore makes; since a change made while the records are read can
    /// leave a reference dangling, the records are read again a few times before the backup
    /// fails with 422, listing every problem, rather than returning a backup no restore accepts.
    ///
    /// With `prune_dangling`, a user or group reference that every read finds dangling, and whose
    /// user or group a direct check of Redis confirms is gone, is dropped instead and listed in
    /// the backup's `pruned`; every other problem still fails the backup.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting a system backup
    /// * `params` - The backup params
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Backup::new", skip_all, fields(prune_dangling = params.prune_dangling), err(Debug))]
    pub async fn new(
        user: &User,
        params: &BackupParams,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // only admins can backup data
        is_admin!(user);
        // the reads made so far, counting the one in progress
        let mut read = 1;
        // the dangling references every read so far found, which alone may be pruned
        let mut lasting: Option<BTreeSet<Dangling>> = None;
        loop {
            // read every record, with what a restore would refuse and what a prune would drop; a record
            // deleted between listing and reading it reads as one that doesn't parse, so read again
            // until the last read rather than failing at once
            let mut backup = match Self::read(user, shared).await {
                Ok(backup) => backup,
                Err(ApiError::Problems { code, body })
                    if code == StatusCode::UNPROCESSABLE_ENTITY && read < BACKUP_READS =>
                {
                    // wait a moment for a deletion in progress to finish, then read again
                    event!(
                        Level::WARN,
                        msg = "A backup read records that don't parse; reading them again",
                        read,
                        problems = body.problems.len()
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(200 * u64::from(read)))
                        .await;
                    read += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let problems = backup.problems();
            let dangling = if params.prune_dangling {
                backup.dangling()
            } else {
                BTreeSet::new()
            };
            // return the backup once a restore would accept it and nothing is left to prune
            if problems.is_empty() && dangling.is_empty() {
                return Ok(backup);
            }
            // keep only the references this read finds dangling too
            let still = match lasting.take() {
                Some(before) => before.intersection(&dangling).cloned().collect(),
                None => dangling,
            };
            // once every read is done, prune what is confirmed gone and fail on what is left
            if read == BACKUP_READS {
                let problems = if still.is_empty() {
                    problems
                } else {
                    let gone = Self::confirm_gone(still, shared).await?;
                    backup.pruned = backup.apply_prune(&gone);
                    backup.problems()
                };
                if problems.is_empty() {
                    // say what the backup dropped, which it lists in full
                    if !backup.pruned.is_empty() {
                        event!(
                            Level::WARN,
                            msg = "A backup dropped dangling user and group references",
                            pruned = backup.pruned.len()
                        );
                    }
                    return Ok(backup);
                }
                return Err(ApiError::Problems {
                    code: StatusCode::UNPROCESSABLE_ENTITY,
                    body: BackupProblems {
                        error: format!(
                            "the records in Redis don't fit together, so a restore would refuse \
                             them: {} problems were left after {read} reads, and no backup was \
                             built; repair them and back up again",
                            problems.len()
                        ),
                        problems,
                    },
                });
            }
            lasting = Some(still);
            // wait a moment for a change in progress to finish, then read again
            event!(
                Level::WARN,
                msg = "A backup's records don't fit together; reading them again",
                read,
                problems = problems.len()
            );
            tokio::time::sleep(std::time::Duration::from_millis(200 * u64::from(read))).await;
            read += 1;
        }
    }

    /// Keeps the dangling references whose user or group a direct check of Redis says is gone
    ///
    /// # Arguments
    ///
    /// * `dangling` - The references every read found dangling
    /// * `shared` - Shared Thorium objects
    async fn confirm_gone(
        dangling: BTreeSet<Dangling>,
        shared: &Shared,
    ) -> Result<BTreeSet<Dangling>, ApiError> {
        // whether each user and group exists, each checked once
        let mut users: HashMap<String, bool> = HashMap::new();
        let mut groups: HashMap<String, bool> = HashMap::new();
        let mut gone = BTreeSet::new();
        for reference in dangling {
            // check the user or group the reference names
            let exists = match &reference {
                Dangling::Member { user, .. } => {
                    if let Some(exists) = users.get(user) {
                        *exists
                    } else {
                        let exists = db::users::exists(user, shared).await?;
                        users.insert(user.clone(), exists);
                        exists
                    }
                }
                Dangling::Membership { group, .. } | Dangling::TokenGroup { group, .. } => {
                    if let Some(exists) = groups.get(group) {
                        *exists
                    } else {
                        let exists =
                            db::groups::exists(std::slice::from_ref(group), shared).await?;
                        groups.insert(group.clone(), exists);
                        exists
                    }
                }
            };
            // a reference to a user or group that exists is kept
            if !exists {
                gone.insert(reference);
            }
        }
        Ok(gone)
    }

    /// Reads every record a backup holds, failing with 422 when any of them doesn't parse
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting a system backup
    /// * `shared` - Shared Thorium objects
    async fn read(user: &User, shared: &Shared) -> Result<Self, ApiError> {
        // get the current system settings
        let settings = SystemSettings::get(user, shared).await?;
        // get lists of users/groups/images/pipelines to backup
        let (users, mut problems) = db::users::backup(shared).await?;
        let (groups, group_problems) = db::groups::backup(shared).await?;
        let (images, image_problems) = db::images::backup(shared).await?;
        let (pipelines, pipeline_problems) = db::pipelines::backup(shared).await?;
        // get the scoped tokens of the users read above, so no token's owner is missing
        let mut usernames: Vec<String> = users.iter().map(|user| user.username.clone()).collect();
        usernames.sort_unstable();
        let (scoped_tokens, token_problems) =
            db::users::backup_scoped_tokens(&usernames, shared).await?;
        // get every group's network-policy set
        let mut group_names: Vec<String> = groups.iter().map(|group| group.name.clone()).collect();
        group_names.sort_unstable();
        let network_policy_sets = db::network_policies::backup_sets(&group_names, shared).await?;
        // a record that doesn't parse fails the backup instead of going missing from it
        problems.extend(group_problems);
        problems.extend(image_problems);
        problems.extend(pipeline_problems);
        problems.extend(token_problems);
        if !problems.is_empty() {
            return Err(ApiError::Problems {
                code: StatusCode::UNPROCESSABLE_ENTITY,
                body: BackupProblems {
                    error: format!(
                        "{} records in Redis don't parse, so no backup was built; repair them and \
                         back up again",
                        problems.len()
                    ),
                    problems,
                },
            });
        }
        let backup = Backup {
            settings,
            users,
            groups,
            images,
            pipelines,
            scoped_tokens,
            network_policy_sets,
            pruned: Vec::new(),
        };
        Ok(backup)
    }

    /// Lists every reason this backup can't be restored as it is
    ///
    /// Every name must be a valid Redis key segment and appear once; every group member and owner
    /// must be a user in the backup, and every group a user is in a group in the backup; every
    /// image and pipeline must name a group in the backup; every stage of a pipeline that no tool
    /// backs must be an image of the pipeline's group in the backup; every user must hold a
    /// primary token no other user holds; every scoped token must belong to a user in the backup
    /// and hold a value no primary or other scoped token holds; and every network-policy set must
    /// name a group in the backup.
    #[must_use]
    pub fn problems(&self) -> Vec<BackupProblem> {
        let mut problems = Vec::new();
        // the users and groups, which the other records refer to
        let (users, groups) = self.people_problems(&mut problems);
        // the images and pipelines of the groups
        self.image_problems(&groups, &mut problems);
        // the users' primary and scoped tokens, and the groups' network-policy sets
        self.token_problems(&users, &mut problems);
        self.policy_set_problems(&groups, &mut problems);
        problems
    }

    /// Records the problems of the backup's users and groups, returning their names
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn people_problems(&self, problems: &mut Vec<BackupProblem>) -> (HashSet<&str>, HashSet<&str>) {
        // each user appears once under a valid name
        let mut users = HashSet::with_capacity(self.users.len());
        for user in &self.users {
            let key = format!("users/{}", user.username);
            check_segment(problems, &key, "username", &user.username);
            if !users.insert(user.username.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    "the backup holds this user more than once",
                ));
            }
        }
        // each group appears once under a valid name, and its members are users in the backup
        let mut groups = HashSet::with_capacity(self.groups.len());
        for group in &self.groups {
            let key = format!("groups/{}", group.name);
            check_segment(problems, &key, "name", &group.name);
            if !groups.insert(group.name.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    "the backup holds this group more than once",
                ));
            }
            for (role, members) in roles(group) {
                // the direct and combined members, each named once and in a stable order
                let names: BTreeSet<&String> =
                    members.direct.iter().chain(&members.combined).collect();
                for name in names {
                    if !users.contains(name.as_str()) {
                        problems.push(BackupProblem::new(
                            &key,
                            format!("its {role} include {name}, a user the backup doesn't hold"),
                        ));
                    }
                }
            }
        }
        // each user's groups are groups in the backup
        for user in &self.users {
            let names: BTreeSet<&String> = user.groups.iter().collect();
            for name in names {
                if !groups.contains(name.as_str()) {
                    problems.push(BackupProblem::new(
                        format!("users/{}", user.username),
                        format!("its groups include {name}, a group the backup doesn't hold"),
                    ));
                }
            }
        }
        // the names the other records refer to
        (users, groups)
    }

    /// Lists the references among the backup's users and groups that name a user or group the
    /// backup doesn't hold: group members, users' groups, and scoped tokens' groups
    fn dangling(&self) -> BTreeSet<Dangling> {
        // the users and groups the backup holds
        let users: HashSet<&str> = self
            .users
            .iter()
            .map(|user| user.username.as_str())
            .collect();
        let groups: HashSet<&str> = self
            .groups
            .iter()
            .map(|group| group.name.as_str())
            .collect();
        let mut dangling = BTreeSet::new();
        // group members, direct and combined, that aren't users in the backup
        for group in &self.groups {
            for (role, members) in roles(group) {
                for user in members.direct.iter().chain(&members.combined) {
                    if !users.contains(user.as_str()) {
                        dangling.insert(Dangling::Member {
                            group: group.name.clone(),
                            role,
                            user: user.clone(),
                        });
                    }
                }
            }
        }
        // users' groups that aren't in the backup
        for user in &self.users {
            for group in &user.groups {
                if !groups.contains(group.as_str()) {
                    dangling.insert(Dangling::Membership {
                        user: user.username.clone(),
                        group: group.clone(),
                    });
                }
            }
        }
        // scoped tokens' groups that aren't in the backup
        for token in &self.scoped_tokens {
            for group in &token.groups {
                if !groups.contains(group.as_str()) {
                    dangling.insert(Dangling::TokenGroup {
                        owner: token.owner.clone(),
                        name: token.name.clone(),
                        group: group.clone(),
                    });
                }
            }
        }
        dangling
    }

    /// Drops dangling references from the backup, returning every change it made
    ///
    /// A group whose last direct owner is dropped, and which has no owner metagroup, gets the
    /// bootstrap admin as its direct owner, since the API never leaves a group without an owner;
    /// without that admin in the backup, the group's owners are kept, so their problem stays. A
    /// scoped token left with no group is deleted, since the API never holds such a token.
    ///
    /// # Arguments
    ///
    /// * `gone` - The references to drop
    fn apply_prune(&mut self, gone: &BTreeSet<Dangling>) -> Vec<PrunedReference> {
        // the changes, in the order they are made
        let mut pruned = Vec::new();
        // group members first, since an owner the admin replaces makes the admin a member
        self.prune_members(gone, &mut pruned);
        // then users' groups and scoped tokens' groups
        self.prune_memberships(gone, &mut pruned);
        self.prune_tokens(gone, &mut pruned);
        pruned
    }

    /// Drops the group members that are gone, giving a group left without an owner the
    /// bootstrap admin as owner, and records each change
    ///
    /// # Arguments
    ///
    /// * `gone` - The references to drop
    /// * `pruned` - The changes made so far
    fn prune_members(&mut self, gone: &BTreeSet<Dangling>, pruned: &mut Vec<PrunedReference>) {
        // only a backup holding the admin can give it a group
        let has_admin = self
            .users
            .iter()
            .any(|user| user.username == BOOTSTRAP_ADMIN);
        // the groups the admin now owns
        let mut owned = Vec::new();
        // drop each group's members that are gone, role by role
        for group in &mut self.groups {
            let name = group.name.clone();
            let owned_before = owned.len();
            let roles = [
                ("owners", &mut group.owners),
                ("managers", &mut group.managers),
                ("users", &mut group.users),
                ("monitors", &mut group.monitors),
            ];
            for (role, members) in roles {
                // the members of this role that are gone
                let dropped: BTreeSet<&String> = gone
                    .iter()
                    .filter_map(|reference| match reference {
                        Dangling::Member {
                            group,
                            role: of,
                            user,
                        } if *group == name && *of == role => Some(user),
                        _ => None,
                    })
                    .collect();
                if dropped.is_empty() {
                    continue;
                }
                // a group never loses its last owner: the admin takes its place, if it can
                let ownerless = role == "owners"
                    && members.metagroups.is_empty()
                    && members.direct.iter().all(|user| dropped.contains(user));
                if ownerless && !has_admin {
                    continue;
                }
                // drop each member from the direct and combined members
                for user in dropped {
                    members.direct.remove(user);
                    members.combined.remove(user);
                    pruned.push(PrunedReference {
                        record: format!("groups/{name}"),
                        field: role.to_owned(),
                        action: PruneAction::Dropped,
                        name: Some(user.clone()),
                        referent: Some(PrunedReferent::User),
                        reason: "the user isn't in the backup and doesn't exist".to_owned(),
                    });
                }
                // and make the admin the owner of a group left without one
                if ownerless {
                    members.direct.insert(BOOTSTRAP_ADMIN.to_owned());
                    members.combined.insert(BOOTSTRAP_ADMIN.to_owned());
                    owned.push(name.clone());
                    pruned.push(PrunedReference {
                        record: format!("groups/{name}"),
                        field: role.to_owned(),
                        action: PruneAction::Added,
                        name: Some(BOOTSTRAP_ADMIN.to_owned()),
                        referent: Some(PrunedReferent::User),
                        reason: "the group would be left with no owner, which the API never \
                                 allows, so the bootstrap admin owns it"
                            .to_owned(),
                    });
                }
            }
            // an owner holds no lower role, as the API keeps it, so the admin leaves those it held
            if owned.len() > owned_before {
                let lower = [
                    ("managers", &mut group.managers),
                    ("users", &mut group.users),
                    ("monitors", &mut group.monitors),
                ];
                for (role, members) in lower {
                    let direct = members.direct.remove(BOOTSTRAP_ADMIN);
                    let combined = members.combined.remove(BOOTSTRAP_ADMIN);
                    if direct || combined {
                        pruned.push(PrunedReference {
                            record: format!("groups/{name}"),
                            field: role.to_owned(),
                            action: PruneAction::Dropped,
                            name: Some(BOOTSTRAP_ADMIN.to_owned()),
                            referent: Some(PrunedReferent::User),
                            reason: "the bootstrap admin now owns the group, and the API gives \
                                     a group's owner no lower role"
                                .to_owned(),
                        });
                    }
                }
            }
        }
        // the admin is a member of every group it now owns, as an owner the API adds is
        if let Some(admin) = self
            .users
            .iter_mut()
            .find(|user| user.username == BOOTSTRAP_ADMIN)
        {
            for group in owned {
                if !admin.groups.contains(&group) {
                    admin.groups.push(group);
                }
            }
        }
    }

    /// Drops the groups that are gone from users' groups, and records each drop
    ///
    /// # Arguments
    ///
    /// * `gone` - The references to drop
    /// * `pruned` - The changes made so far
    fn prune_memberships(&mut self, gone: &BTreeSet<Dangling>, pruned: &mut Vec<PrunedReference>) {
        for user in &mut self.users {
            // the groups of this user that are gone
            let dropped: BTreeSet<String> = gone
                .iter()
                .filter_map(|reference| match reference {
                    Dangling::Membership { user: of, group } if *of == user.username => {
                        Some(group.clone())
                    }
                    _ => None,
                })
                .collect();
            // drop them from the user's groups
            user.groups.retain(|group| !dropped.contains(group));
            for group in dropped {
                pruned.push(PrunedReference {
                    record: format!("users/{}", user.username),
                    field: "groups".to_owned(),
                    action: PruneAction::Dropped,
                    name: Some(group),
                    referent: Some(PrunedReferent::Group),
                    reason: "the group isn't in the backup and doesn't exist".to_owned(),
                });
            }
        }
    }

    /// Narrows each scoped token to the groups that exist, deleting a token left with none, and
    /// records each change
    ///
    /// # Arguments
    ///
    /// * `gone` - The references to drop
    /// * `pruned` - The changes made so far
    fn prune_tokens(&mut self, gone: &BTreeSet<Dangling>, pruned: &mut Vec<PrunedReference>) {
        // the positions of the tokens left with no group
        let mut emptied = HashSet::new();
        for (index, token) in self.scoped_tokens.iter_mut().enumerate() {
            // the groups of this token that are gone
            let record = format!("scoped_tokens/{}/{}", token.owner, token.name);
            let dropped: BTreeSet<String> = gone
                .iter()
                .filter_map(|reference| match reference {
                    Dangling::TokenGroup { owner, name, group }
                        if *owner == token.owner && *name == token.name =>
                    {
                        Some(group.clone())
                    }
                    _ => None,
                })
                .collect();
            if dropped.is_empty() {
                continue;
            }
            // drop them from the token's groups, narrowing it
            token.groups.retain(|group| !dropped.contains(group));
            for group in dropped {
                pruned.push(PrunedReference {
                    record: record.clone(),
                    field: "groups".to_owned(),
                    action: PruneAction::Dropped,
                    name: Some(group),
                    referent: Some(PrunedReferent::Group),
                    reason: "the group isn't in the backup and doesn't exist".to_owned(),
                });
            }
            // a token left with no group is deleted
            if token.groups.is_empty() {
                emptied.insert(index);
                pruned.push(PrunedReference {
                    record,
                    field: "groups".to_owned(),
                    action: PruneAction::Deleted,
                    name: None,
                    referent: None,
                    reason: "a scoped token must be limited to at least one group, and every \
                             group it named is gone"
                        .to_owned(),
                });
            }
        }
        // remove the emptied tokens, keeping the others in order
        let mut index = 0;
        self.scoped_tokens.retain(|_| {
            let keep = !emptied.contains(&index);
            index += 1;
            keep
        });
    }

    /// Records the problems of the backup's images and pipelines
    ///
    /// # Arguments
    ///
    /// * `groups` - The names of the backup's groups
    /// * `problems` - The problems found so far
    fn image_problems(&self, groups: &HashSet<&str>, problems: &mut Vec<BackupProblem>) {
        // each image appears once under valid names, in a group in the backup
        let mut images = HashSet::with_capacity(self.images.len());
        for image in &self.images {
            let key = format!("images/{}/{}", image.group, image.name);
            check_segment(problems, &key, "group", &image.group);
            check_segment(problems, &key, "name", &image.name);
            if !groups.contains(image.group.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    format!("its group {} isn't in the backup", image.group),
                ));
            }
            if !images.insert((image.group.as_str(), image.name.as_str())) {
                problems.push(BackupProblem::new(
                    &key,
                    "the backup holds this image more than once",
                ));
            }
        }
        // each pipeline appears once under valid names, in a group in the backup, and runs
        // only images of that group in the backup, except for the stages a tool backs
        let mut pipelines = HashSet::with_capacity(self.pipelines.len());
        for pipeline in &self.pipelines {
            let key = format!("pipelines/{}/{}", pipeline.group, pipeline.name);
            check_segment(problems, &key, "group", &pipeline.group);
            check_segment(problems, &key, "name", &pipeline.name);
            if !groups.contains(pipeline.group.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    format!("its group {} isn't in the backup", pipeline.group),
                ));
            }
            if !pipelines.insert((pipeline.group.as_str(), pipeline.name.as_str())) {
                problems.push(BackupProblem::new(
                    &key,
                    "the backup holds this pipeline more than once",
                ));
            }
            // a stage a tool backs names no image, so only the other stages must be images
            let order: BTreeSet<&String> = pipeline.unconverted_stages().into_iter().collect();
            for image in order {
                if !images.contains(&(pipeline.group.as_str(), image.as_str())) {
                    problems.push(BackupProblem::new(
                        &key,
                        format!(
                            "it runs image {image}, which the backup doesn't hold in group {}",
                            pipeline.group
                        ),
                    ));
                }
            }
        }
    }

    /// Records the problems of the backup's primary and scoped tokens
    ///
    /// A token value logs in as whoever holds it, so no two users may share a primary token and
    /// no scoped token may hold a primary token's value or another scoped token's.
    ///
    /// # Arguments
    ///
    /// * `users` - The names of the backup's users
    /// * `problems` - The problems found so far
    fn token_problems(&self, users: &HashSet<&str>, problems: &mut Vec<BackupProblem>) {
        // each user's primary token is a value no other user holds
        let mut primaries = HashSet::with_capacity(self.users.len());
        for user in &self.users {
            if !primaries.insert(user.token.as_str()) {
                problems.push(BackupProblem::new(
                    format!("users/{}", user.username),
                    "another user in the backup holds the same token",
                ));
            }
        }
        // each scoped token appears once, under valid names, for a user in the backup, with a
        // value of its own
        let mut tokens = HashSet::with_capacity(self.scoped_tokens.len());
        let mut values = HashSet::with_capacity(self.scoped_tokens.len());
        for token in &self.scoped_tokens {
            let key = format!("scoped_tokens/{}/{}", token.owner, token.name);
            check_segment(problems, &key, "owner", &token.owner);
            check_segment(problems, &key, "name", &token.name);
            if !users.contains(token.owner.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    format!("its owner {} isn't in the backup", token.owner),
                ));
            }
            if !tokens.insert((token.owner.as_str(), token.name.as_str())) {
                problems.push(BackupProblem::new(
                    &key,
                    "the backup holds this scoped token more than once",
                ));
            }
            if primaries.contains(token.token.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    "a user in the backup holds its value as a primary token",
                ));
            }
            if !values.insert(token.token.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    "another scoped token in the backup holds the same value",
                ));
            }
        }
    }

    /// Records the problems of the backup's network-policy sets
    ///
    /// # Arguments
    ///
    /// * `groups` - The names of the backup's groups
    /// * `problems` - The problems found so far
    fn policy_set_problems(&self, groups: &HashSet<&str>, problems: &mut Vec<BackupProblem>) {
        // each network-policy set appears once, for a group in the backup, under valid names
        let mut sets = HashSet::with_capacity(self.network_policy_sets.len());
        for set in &self.network_policy_sets {
            let key = format!("network_policy_sets/{}", set.group);
            check_segment(problems, &key, "group", &set.group);
            if !groups.contains(set.group.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    format!("its group {} isn't in the backup", set.group),
                ));
            }
            if !sets.insert(set.group.as_str()) {
                problems.push(BackupProblem::new(
                    &key,
                    "the backup holds this group's network-policy set more than once",
                ));
            }
            for policy in &set.policies {
                check_segment(problems, &key, "policy", policy);
            }
        }
    }

    /// Validates a backup, then replaces this namespace's Redis state with it
    ///
    /// A backup with problems is refused before any key is deleted. Only the keys of this API's
    /// namespace are deleted, unless `params` asks to flush the API's whole Redis database.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is restoring Thorium data
    /// * `params` - The restore params
    /// * `shared` - Shared Thorium objects
    /// * `span` - The span to log traces under
    pub async fn restore(
        &self,
        user: &User,
        params: &RestoreParams,
        shared: &Shared,
        span: &Span,
    ) -> Result<(), ApiError> {
        // start our system restore span
        let span = span!(
            parent: span,
            Level::INFO,
            "System Restore",
            user = user.username,
            flush_db = params.flush_db
        );
        // only admins can restore data
        is_admin!(user);
        // refuse a backup whose records don't fit together before anything is deleted
        let problems = self.problems();
        if !problems.is_empty() {
            return Err(ApiError::Problems {
                code: StatusCode::BAD_REQUEST,
                body: BackupProblems {
                    error: format!(
                        "the backup can't be restored: {} problems were found, and nothing was \
                         changed",
                        problems.len()
                    ),
                    problems,
                },
            });
        }
        // refuse users whose password hashes use an algorithm or cost Thorium doesn't hash with
        db::users::check_restorable(&self.users)?;
        // what each step wrote, for the error of a step that fails
        let mut written: Vec<(&str, usize)> = Vec::with_capacity(8);
        // delete this namespace's keys, or flush the whole database when asked, naming the step
        // that ran in its error
        let deleted = if params.flush_db {
            db::system::flush_db(shared)
                .await
                .map_err(|error| step_failed("flush", &written, &error))?
        } else {
            db::system::wipe(shared)
                .await
                .map_err(|error| step_failed("wipe", &written, &error))?
        };
        written.push((
            "deleted keys",
            usize::try_from(deleted).unwrap_or(usize::MAX),
        ));
        // initialize the system and restore settings
        db::system::init(shared)
            .await
            .map_err(|error| step_failed("init", &written, &error))?;
        db::system::restore_settings(&self.settings, shared)
            .await
            .map_err(|error| step_failed("settings", &written, &error))?;
        written.push(("settings", 1));
        // restore users with their scoped tokens
        db::users::restore(&self.users, shared)
            .await
            .map_err(|error| step_failed("users", &written, &error))?;
        written.push(("users", self.users.len()));
        db::users::restore_scoped_tokens(&self.scoped_tokens, shared)
            .await
            .map_err(|error| step_failed("scoped tokens", &written, &error))?;
        written.push(("scoped tokens", self.scoped_tokens.len()));
        // restore groups, images, and pipelines
        db::groups::restore(&self.groups, shared, &span)
            .await
            .map_err(|error| step_failed("groups", &written, &error))?;
        written.push(("groups", self.groups.len()));
        db::images::restore(&self.images, shared)
            .await
            .map_err(|error| step_failed("images", &written, &error))?;
        written.push(("images", self.images.len()));
        db::pipelines::restore(&self.pipelines, shared)
            .await
            .map_err(|error| step_failed("pipelines", &written, &error))?;
        written.push(("pipelines", self.pipelines.len()));
        // restore the network policies each group holds
        db::network_policies::restore_sets(&self.network_policy_sets, shared)
            .await
            .map_err(|error| step_failed("network-policy sets", &written, &error))?;
        Ok(())
    }
}

/// Builds the error of a restore step that failed, naming the step and what the steps before it
/// wrote
///
/// The step's own error is logged, not returned, since it can hold internal details. Until the
/// users step lands, the namespace may hold no users, so the error of a step at or before it says
/// how an admin gets back in to run the restore again.
///
/// # Arguments
///
/// * `step` - The step that failed
/// * `written` - What each step before it wrote
/// * `error` - Why the step failed
fn step_failed(step: &str, written: &[(&str, usize)], error: &ApiError) -> ApiError {
    // log the full error for the cluster's operators
    event!(
        Level::ERROR,
        msg = "A restore step failed",
        step,
        kind = error.kind(),
        error = error.msg()
    );
    // name the step, the kind of failure, and what was written before it
    let before = if written.is_empty() {
        "before any step wrote anything".to_owned()
    } else {
        let counts: Vec<String> = written
            .iter()
            .map(|(name, count)| format!("{name} {count}"))
            .collect();
        format!("after the steps before it wrote {}", counts.join(", "))
    };
    // say how to run it again, which needs an admin who can log in
    let rerun = if written.iter().any(|(name, _)| *name == "users") {
        "the same restore can be run again once the cause is fixed"
    } else {
        "the namespace may hold no users now, so no one can log in to run it again: once the \
         cause is fixed, bootstrap an admin with the API's secret key and run the same restore \
         again as that admin"
    };
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        Some(format!(
            "the restore failed in the {step} step ({} error) {before}; {rerun}",
            error.kind()
        )),
    )
}

/// Records a problem when a name can't be one segment of a Redis key
///
/// A name must be non-empty and hold no `:`, which separates key segments; keys are binary safe,
/// so any other character is allowed.
///
/// # Arguments
///
/// * `problems` - The problems found so far
/// * `key` - The record the name belongs to
/// * `field` - Which of the record's names this is
/// * `name` - The name
fn check_segment(problems: &mut Vec<BackupProblem>, key: &str, field: &str, name: &str) {
    // an empty name or a separator would change which key is written
    if name.is_empty() || name.contains(':') {
        problems.push(BackupProblem::new(
            key,
            format!("its {field} isn't a valid Redis key segment"),
        ));
    }
}

/// Returns the members of each role of a group, by the role's name
///
/// # Arguments
///
/// * `group` - The group
fn roles(group: &Group) -> [(&'static str, &GroupUsers); 4] {
    // every role a member can hold
    [
        ("owners", &group.owners),
        ("managers", &group.managers),
        ("users", &group.users),
        ("monitors", &group.monitors),
    ]
}

impl Node {
    /// Register a new node
    ///
    /// This will not reregister existing nodes
    ///
    /// # Arguments
    ///
    /// * `node` - The node registration info
    /// * `shared` - Shared Thorium objects
    /// * `span` - The span to log traces under
    #[instrument(name = "Node::register", skip_all, fields(cluster = node.cluster, node = node.name), err(Debug))]
    pub async fn register(
        user: &User,
        node: &NodeRegistration,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only admins can register nodes
        is_admin!(user);
        // TODO cxheck if node is already registered
        db::system::register(node, shared).await
    }

    /// Gets a nodes info
    ///
    /// # Arguments
    ///
    /// * `user` - The user getting a nodes info
    /// * `cluster` - The cluster this node is in
    /// * `node` - The name of this node
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Node::get", skip_all, err(Debug))]
    pub async fn get(
        user: &User,
        cluster: &str,
        node: &str,
        mut params: NodeGetParams,
        shared: &Shared,
    ) -> Result<Node, ApiError> {
        // only admins can get node info
        is_admin!(user);
        // if no scalers were added then expand them to all
        params.default_scalers();
        // get this nodes info
        db::system::get_node(cluster, node, &params, shared).await
    }

    /// Updates a nodes info
    ///
    /// # Arguments
    ///
    /// * `update` - The update to apply to this node
    /// * `shared` - Shared Thorium objects
    /// * `span` - The span to log traces under
    #[instrument(name = "Node::update", skip(self, shared), fields(cluster = self.cluster, node = self.name), err(Debug))]
    pub async fn update(&self, update: &NodeUpdate, shared: &Shared) -> Result<(), ApiError> {
        // update this nodes info
        db::system::update_node(self, update, shared).await
    }

    /// Lists node names
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is listing node names
    /// * `params` - The params for listing node names
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Node::list", skip_all, err(Debug))]
    pub async fn list(
        user: &User,
        mut params: NodeListParams,
        shared: &Shared,
    ) -> Result<ApiCursor<NodeListLine>, ApiError> {
        // if we don't have any clusters defined then add all known clusters
        params.default_expand(shared);
        // either list nodes or get just the details for the nodes we have
        if params.nodes.is_empty() {
            // if we have no nodes specified, get a chunk of the node names list scoped to the user
            let scylla_cursor = db::system::list_nodes(user, params, shared).await?;
            // convert our scylla cursor to a user facing one
            Ok(ApiCursor::from(scylla_cursor))
        } else {
            // if we have nodes specified then just get info on those
            let rows = db::system::get_node_rows(&params.clusters, &params.nodes, shared).await?;
            // turn our rows into node list lines
            let nodes = rows
                .into_iter()
                .map(|row| NodeListLine::from(row))
                .collect();
            // turn our nodes into a cursor object
            let cursor = ApiCursor {
                cursor: None,
                data: nodes,
            };
            Ok(cursor)
        }
    }

    /// Lists node details
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is listing node details
    /// * `params` - The params for listing nodes
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Node::list_details", skip_all, err(Debug))]
    pub async fn list_details(
        user: &User,
        mut params: NodeListParams,
        shared: &Shared,
    ) -> Result<ApiCursor<Node>, ApiError> {
        // if we don't have any clusters defined then add all known clusters
        params.default_expand(shared);
        // either list nodes or get just the details for the nodes we have
        if params.nodes.is_empty() {
            // if we don't have nodes, get a chunk of the node details list scoped to the user
            db::system::list_node_details(user, params, shared).await
        } else {
            // if we have nodes specified then just get info on those
            let rows = db::system::get_node_rows(&params.clusters, &params.nodes, shared).await?;
            // create an empty user facing cursor to store this cursors data
            let mut cursor = ApiCursor::empty(rows.len());
            // get all of our nodes workers 25 at a time
            let nodes = stream::iter(rows)
                .map(|row| db::system::get_workers_for_node(row, &params.scalers, shared))
                .buffered(25)
                .collect::<Vec<Result<Node, ApiError>>>()
                .await
                .into_iter()
                .filter_map(|res| log_scylla_err!(res));
            // add this nodes data
            cursor.data.extend(nodes);
            Ok(cursor)
        }
    }
}

impl TryFrom<NodeRow> for Node {
    type Error = ApiError;
    /// Try to convert a [`NodeRow`] into a [`Node`]
    ///
    /// # Arguments
    ///
    /// * `row` - The row to convert
    fn try_from(row: NodeRow) -> Result<Node, Self::Error> {
        // deserialize our health
        let health = deserialize!(&row.health);
        // build our resources objects
        let resources = deserialize!(&row.resources);
        // build our node struct
        let node = Node {
            cluster: row.cluster,
            name: row.node,
            health,
            resources,
            workers: HashMap::default(),
            heart_beat: row.heart_beat,
        };
        Ok(node)
    }
}

impl SimpleCursorExt for NodeListLine {
    /// The statements this cursor pages with
    type Statements = NodesListCursor;

    /// Get the statements this cursor pages with
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn statements(shared: &Shared) -> &Self::Statements {
        &shared.scylla.queries.nodes.list
    }
}

impl SimpleCursorExt for NodeRow {
    /// The statements this cursor pages with
    type Statements = NodesListDetailsCursor;

    /// Get the statements this cursor pages with
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn statements(shared: &Shared) -> &Self::Statements {
        &shared.scylla.queries.nodes.list_details
    }
}

impl<S> FromRequestParts<S> for NodeGetParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            Ok(Self::default())
        }
    }
}

impl<S> FromRequestParts<S> for NodeListParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            Ok(Self::default())
        }
    }
}

impl NodeListParams {
    /// Expand our params to sane defaults if they came in empty
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub fn default_expand(&mut self, shared: &Shared) {
        // expand our scalers
        self.default_scalers();
        // add the clusters for this scaler
        self.clusters_from_config_ref(&shared.config);
    }

    /// If we don't have any scalers then use all the ones we know about
    pub fn default_scalers(&mut self) {
        // if we don't have any scalers set then add all of them
        if self.scalers.is_empty() {
            // add all of our scalers
            self.scalers = vec![
                ImageScaler::K8s,
                ImageScaler::BareMetal,
                ImageScaler::Windows,
                ImageScaler::Kvm,
                ImageScaler::External,
            ];
        }
    }
}

impl NodeGetParams {
    /// If we don't have any scalers then use all the ones we know about
    pub fn default_scalers(&mut self) {
        // if we don't have any scalers set then add all of them
        if self.scalers.is_empty() {
            // add all of our scalers
            self.scalers = vec![
                ImageScaler::K8s,
                ImageScaler::BareMetal,
                ImageScaler::Windows,
                ImageScaler::Kvm,
                ImageScaler::External,
            ];
        }
    }
}

impl Worker {
    /// Gets info on a specific worker
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting this workers info
    /// * `name` - The name of the worker to get
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Worker::get", skip(user, shared), err(Debug))]
    pub async fn get(user: &User, name: &str, shared: &Shared) -> Result<Worker, ApiError> {
        // get this worker
        let worker = db::system::get_worker(name, shared).await?;
        // make sure this user can see this worker
        if !user.is_admin() && !user.groups.contains(&worker.group) {
            not_found!(format!("Worker {} does not exist", name))
        } else {
            Ok(worker)
        }
    }

    /// Updates a worker's status in Scylla
    ///
    /// # Arguments
    ///
    /// * `_` - The user that is updating this workers status
    /// * `scaler` - The scaler this worker is under
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Worker::update", skip_all, err(Debug))]
    pub async fn update(
        &self,
        user: &User,
        update: &WorkerUpdate,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only the owner of this worker or admins can update it
        if !user.is_admin() && user.username != self.user {
            return unauthorized!();
        }
        // add this worker to our workers table in scylla
        db::system::update_worker(self, update, shared).await
    }
}

impl TryFrom<HashMap<String, String>> for Worker {
    type Error = ApiError;

    /// Try to cast a [`HashMap`] of strings into an [`Worker`]
    ///
    /// # Arguments
    ///
    /// * `raw` - The `HashMap` to cast into a worker
    fn try_from(mut map: HashMap<String, String>) -> Result<Self, Self::Error> {
        // build a worker from our map of data
        let worker = Worker {
            cluster: extract!(map, "cluster"),
            node: extract!(map, "node"),
            scaler: deserialize_ext!(map, "scaler"),
            name: extract!(map, "name"),
            user: extract!(map, "user"),
            group: extract!(map, "group"),
            pipeline: extract!(map, "pipeline"),
            stage: extract!(map, "stage"),
            status: deserialize_ext!(map, "status"),
            spawned: deserialize_ext!(map, "spawned"),
            heart_beat: deserialize_ext!(map, "heart_beat", None),
            resources: deserialize_ext!(map, "resources"),
            pool: deserialize_ext!(map, "pool"),
            active: deserialize_ext!(map, "active", None),
            run: deserialize_opt!(map, "run", Uuid::parse_str),
        };
        Ok(worker)
    }
}

impl WorkerRegistrationList {
    /// Adds new workers to Scylla
    ///
    /// # Arguments
    ///
    /// * `_` - The user that is registering new workers
    /// * `scaler` - The scaler this worker is under
    /// * `shared` - Shared Thorium objects
    pub async fn register(
        &self,
        _: &User,
        scaler: ImageScaler,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // TODO ensure all of these nodes exist
        // add this worker to our workers table in scylla
        db::system::register_workers(scaler, self, shared).await
    }
}

impl WorkerDeleteMap {
    /// Deletes workers from scylla
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is deleteing workers
    /// * `scaler` - The scaler that we are deleting workers from
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "WorkerDeleteMap::delete", skip(self, user, shared), fields(user = user.username, count = self.workers.len()), err(Debug))]
    pub async fn delete(
        self,
        user: &User,
        scaler: ImageScaler,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // if this user isn't an admin then make sure they are only deleting their own workers
        db::system::can_delete_workers(user, &self, shared).await?;
        // delete the specified worekrs
        db::system::delete_workers(scaler, self, shared).await?;
        Ok(())
    }
}

impl From<NodeRow> for NodeListLine {
    /// cast a node row to a node list line
    ///
    /// # Arguments
    ///
    /// * `row` - The node row to convert
    fn from(row: NodeRow) -> Self {
        NodeListLine {
            cluster: row.cluster,
            node: row.node,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{BOOTSTRAP_ADMIN, check_segment};
    use crate::models::{
        Backup, Group, GroupAllowed, GroupUsers, Pipeline, PruneAction, PrunedReferent,
        SystemSettings, User, UserRole, UserSettings,
    };
    use clap::ValueEnum;
    use std::collections::{HashMap, HashSet};

    /// The backend `default_scalers` expansions for node list and get params expand an empty
    /// scaler list to every scaler, including `ImageScaler::Kvm`, so a node listing or node get
    /// with no scalers set includes KVM workers.
    #[test]
    fn node_params_default_scalers_include_every_scaler() {
        // expand empty node list and node get params
        let mut list = NodeListParams {
            scalers: Vec::default(),
            ..NodeListParams::default()
        };
        list.default_scalers();
        let mut get = NodeGetParams::default();
        get.default_scalers();
        for scaler in ImageScaler::value_variants() {
            assert!(
                list.scalers.contains(scaler),
                "node list params must default to including {scaler:?}"
            );
            assert!(
                get.scalers.contains(scaler),
                "node get params must default to including {scaler:?}"
            );
        }
    }

    /// Params that already name their scalers are left as is.
    #[test]
    fn node_params_with_scalers_are_not_expanded() {
        let mut get = NodeGetParams::default().scaler(ImageScaler::Windows);
        get.default_scalers();
        assert_eq!(get.scalers, vec![ImageScaler::Windows]);
    }

    /// Builds a user of the given groups holding a token of its own name
    ///
    /// # Arguments
    ///
    /// * `name` - The username
    /// * `groups` - The groups the user is in
    fn user(name: &str, groups: &[&str]) -> User {
        // a plain user with nothing but a name, groups, and a token
        User {
            username: name.to_owned(),
            password: None,
            email: format!("{name}@example.invalid"),
            role: UserRole::User,
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
            actual_groups: Vec::new(),
            token: format!("token-{name}"),
            token_expiration: chrono::Utc::now(),
            unix: None,
            settings: UserSettings::default(),
            verified: true,
            verification_token: None,
            verification_sent: None,
            aliases: HashMap::new(),
            image: None,
        }
    }

    /// Builds the members of one role of a group
    ///
    /// # Arguments
    ///
    /// * `direct` - The direct members, who are the combined members too
    fn members(direct: &[&str]) -> GroupUsers {
        // no metagroups, so the combined members are the direct ones
        let direct: HashSet<String> = direct.iter().map(|name| (*name).to_owned()).collect();
        GroupUsers {
            combined: direct.clone(),
            direct,
            metagroups: HashSet::new(),
        }
    }

    /// Builds a group with owners and users
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    /// * `owners` - Its direct owners
    /// * `users` - Its direct users
    fn group(name: &str, owners: &[&str], users: &[&str]) -> Group {
        // no managers or monitors
        Group {
            name: name.to_owned(),
            owners: members(owners),
            managers: members(&[]),
            analysts: HashSet::new(),
            users: members(users),
            monitors: members(&[]),
            description: None,
            allowed: GroupAllowed::default(),
        }
    }

    /// Builds a backup of users and groups
    ///
    /// # Arguments
    ///
    /// * `users` - The users
    /// * `groups` - The groups
    fn backup(users: Vec<User>, groups: Vec<Group>) -> Backup {
        // nothing but users and groups
        Backup {
            settings: SystemSettings::default(),
            users,
            groups,
            images: Vec::new(),
            pipelines: Vec::new(),
            scoped_tokens: Vec::new(),
            network_policy_sets: Vec::new(),
            pruned: Vec::new(),
        }
    }

    /// Returns whether a problem names a record and some text
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup to check
    /// * `key` - The record
    /// * `text` - Text its error must contain
    fn has_problem(backup: &Backup, key: &str, text: &str) -> bool {
        // the record and the text, in one problem
        backup
            .problems()
            .iter()
            .any(|problem| problem.key == key && problem.error.contains(text))
    }

    /// A user's membership in a group the backup doesn't hold is a problem, as a member that
    /// isn't a user is
    #[test]
    fn a_membership_in_a_missing_group_is_a_problem() {
        // alice is in a group that isn't there
        let held = backup(
            vec![
                user(BOOTSTRAP_ADMIN, &["vesta"]),
                user("alice", &["vesta", "ghost"]),
            ],
            vec![group("vesta", &[BOOTSTRAP_ADMIN], &["alice"])],
        );
        assert!(
            has_problem(&held, "users/alice", "ghost"),
            "{:?}",
            held.problems()
        );
        assert_eq!(held.problems().len(), 1, "{:?}", held.problems());
    }

    /// Pruning the last direct owner of a group without owner metagroups adds the bootstrap admin
    /// as its owner and member; without that admin in the backup the owner stays, and so does
    /// its problem
    #[test]
    fn the_last_owner_is_replaced_by_the_admin_or_stays_fatal() {
        // a group owned only by a user that is gone
        let mut held = backup(
            vec![user(BOOTSTRAP_ADMIN, &[]), user("alice", &["vesta"])],
            vec![group("vesta", &["ghost"], &["alice"])],
        );
        let dangling = held.dangling();
        let pruned = held.apply_prune(&dangling);
        // the admin owns the group and is in it
        let admin: HashSet<String> = [BOOTSTRAP_ADMIN.to_owned()].into();
        assert_eq!(held.groups[0].owners.direct, admin);
        assert_eq!(held.groups[0].owners.combined, admin);
        assert!(held.users[0].groups.contains(&"vesta".to_owned()));
        assert!(held.problems().is_empty(), "{:?}", held.problems());
        // the drop and the addition are recorded, in that order
        let changes: Vec<(PruneAction, Option<&str>)> = pruned
            .iter()
            .map(|change| (change.action, change.name.as_deref()))
            .collect();
        assert_eq!(
            changes,
            vec![
                (PruneAction::Dropped, Some("ghost")),
                (PruneAction::Added, Some(BOOTSTRAP_ADMIN))
            ]
        );
        assert!(pruned.iter().all(|change| change.record == "groups/vesta"
            && change.field == "owners"
            && change.referent == Some(PrunedReferent::User)));
        // without the admin the owner is kept and still a problem
        let mut held = backup(
            vec![user("alice", &["vesta"])],
            vec![group("vesta", &["ghost"], &["alice"])],
        );
        let dangling = held.dangling();
        let pruned = held.apply_prune(&dangling);
        assert!(pruned.is_empty(), "{pruned:?}");
        assert!(held.groups[0].owners.direct.contains("ghost"));
        assert!(
            has_problem(&held, "groups/vesta", "ghost"),
            "{:?}",
            held.problems()
        );
        // an owner beside the one that is gone keeps the group owned, so no admin is added
        let mut held = backup(
            vec![user(BOOTSTRAP_ADMIN, &[]), user("alice", &["vesta"])],
            vec![group("vesta", &["ghost", "alice"], &[])],
        );
        let dangling = held.dangling();
        let pruned = held.apply_prune(&dangling);
        assert_eq!(pruned.len(), 1, "{pruned:?}");
        let alice: HashSet<String> = ["alice".to_owned()].into();
        assert_eq!(held.groups[0].owners.direct, alice);
    }

    /// Pruning the last direct owner of a group with an owner metagroup only drops that owner,
    /// since the metagroup keeps the group owned, so the bootstrap admin isn't added
    #[test]
    fn an_owner_metagroup_keeps_the_group_owned_without_the_admin() {
        // a group whose only direct owner is gone, beside an owner metagroup
        let mut vesta = group("vesta", &["ghost"], &["alice"]);
        vesta.owners.metagroups.insert("vesta-owners".to_owned());
        let mut held = backup(
            vec![user(BOOTSTRAP_ADMIN, &[]), user("alice", &["vesta"])],
            vec![vesta],
        );
        let dangling = held.dangling();
        let pruned = held.apply_prune(&dangling);
        // the owner is dropped and nothing is added in its place
        let changes: Vec<(PruneAction, Option<&str>)> = pruned
            .iter()
            .map(|change| (change.action, change.name.as_deref()))
            .collect();
        assert_eq!(changes, vec![(PruneAction::Dropped, Some("ghost"))]);
        assert!(held.groups[0].owners.direct.is_empty());
        assert!(!held.groups[0].owners.combined.contains(BOOTSTRAP_ADMIN));
        // the admin isn't made a member of the group
        assert!(!held.users[0].groups.contains(&"vesta".to_owned()));
        assert!(held.problems().is_empty(), "{:?}", held.problems());
    }

    /// Pruning drops only user and group references: an image or pipeline naming a group that is
    /// gone, a pipeline running an image its group lacks, a shared token, and an invalid name stay
    /// problems
    #[test]
    fn pruning_leaves_every_other_problem() {
        // a dangling member beside problems pruning never touches
        let mut held = backup(
            vec![
                user(BOOTSTRAP_ADMIN, &["vesta"]),
                user("alice", &["vesta", "ghost"]),
            ],
            vec![
                group("vesta", &[BOOTSTRAP_ADMIN, "gone"], &["alice"]),
                group("bad:name", &[BOOTSTRAP_ADMIN], &[]),
            ],
        );
        let shared = held.users[0].token.clone();
        held.users[1].token = shared;
        let pipeline: Pipeline = serde_json::from_value(serde_json::json!({
            "group": "ghost-group", "name": "flow", "creator": BOOTSTRAP_ADMIN,
            "order": [["ghost-image"]], "sla": 1, "triggers": {}, "description": null,
            "bans": {},
        }))
        .unwrap();
        held.pipelines.push(pipeline);
        let dangling = held.dangling();
        let pruned = held.apply_prune(&dangling);
        // the member and the membership are pruned
        assert_eq!(pruned.len(), 2, "{pruned:?}");
        assert!(!has_problem(&held, "groups/vesta", "gone"));
        assert!(!has_problem(&held, "users/alice", "ghost"));
        // and everything else is still a problem
        assert!(has_problem(
            &held,
            "pipelines/ghost-group/flow",
            "ghost-group"
        ));
        assert!(has_problem(
            &held,
            "pipelines/ghost-group/flow",
            "ghost-image"
        ));
        assert!(has_problem(&held, "users/alice", "same token"));
        assert!(has_problem(
            &held,
            "groups/bad:name",
            "valid Redis key segment"
        ));
    }

    /// A pipeline stage a tool backs names no image, so only the stages no tool backs must be
    /// images of the pipeline's group in the backup
    #[test]
    fn tool_backed_stages_need_no_backed_up_image() {
        // a pipeline with a tool-backed stage and a stage no tool backs, in a group with no images
        let mut held = backup(
            vec![user(BOOTSTRAP_ADMIN, &["vesta"])],
            vec![group("vesta", &[BOOTSTRAP_ADMIN], &[])],
        );
        let pipeline: Pipeline = serde_json::from_value(serde_json::json!({
            "group": "vesta", "name": "flow", "creator": BOOTSTRAP_ADMIN,
            "order": [["harvest"], ["legacy"]], "sla": 1, "triggers": {}, "description": null,
            "bans": {}, "stage_tools": {"harvest": "6a0b7a62-0f3c-4f4e-9a55-5f1d6ce7d001"},
        }))
        .unwrap();
        held.pipelines.push(pipeline);
        // the tool-backed stage is no problem
        assert!(
            !has_problem(&held, "pipelines/vesta/flow", "harvest"),
            "{:?}",
            held.problems()
        );
        // the stage no tool backs still must be an image in the backup
        assert!(
            has_problem(&held, "pipelines/vesta/flow", "legacy"),
            "{:?}",
            held.problems()
        );
    }

    /// A key segment may hold any character but the separator, and may not be empty
    #[test]
    fn key_segments_allow_any_character_but_the_separator() {
        // unicode, a NUL, and spaces are fine in a binary-safe key
        let mut problems = Vec::new();
        for name in ["io", "é€😀éשלום\u{0}name", "a b", "a-b_c.d"] {
            check_segment(&mut problems, "users/x", "username", name);
        }
        assert!(problems.is_empty(), "{problems:?}");
        // a separator or an empty name is a problem
        check_segment(&mut problems, "users/x", "username", "a:b");
        check_segment(&mut problems, "users/x", "username", "");
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    /// Build an allowed gateway entry
    ///
    /// # Arguments
    ///
    /// * `url` - The allowed base url
    fn entry(url: &str) -> crate::models::AllowedEndpoint {
        crate::models::AllowedEndpoint::new(
            crate::models::ModelProviderKind::OpenAiCompatible,
            Some(url.to_owned()),
        )
    }

    /// A whitelist update clears first, then adds, then removes, and the settings compare equal
    /// to an update once it's applied
    #[test]
    fn models_conf_update_order() {
        use crate::models::{ModelsConfUpdate, SystemSettingsUpdate};
        // settings allowing e1
        let (e1, e2, e3) = (
            entry("https://e1/v1"),
            entry("https://e2/v1"),
            entry("https://e3/v1"),
        );
        let mut settings = SystemSettings::default();
        settings.models.allowed_endpoints.insert(e1.clone());
        // clearing and adding e2 leaves only e2
        let update = ModelsConfUpdate::default().clear().add_endpoint(e2.clone());
        update.clone().update(&mut settings);
        assert_eq!(
            settings.models.allowed_endpoints.iter().collect::<Vec<_>>(),
            vec![&e2]
        );
        // the applied update matches the settings
        let settings_update = SystemSettingsUpdate::default().models(update);
        assert_eq!(settings, settings_update);
        // adding and removing e3 leaves it absent
        ModelsConfUpdate::default()
            .add_endpoint(e3.clone())
            .remove_endpoint(e3.clone())
            .update(&mut settings);
        assert!(!settings.models.allowed_endpoints.contains(&e3));
        // an update whose added endpoint is missing doesn't match
        let missing = SystemSettingsUpdate::default()
            .models(ModelsConfUpdate::default().add_endpoint(e1.clone()));
        assert_ne!(settings, missing);
    }

    /// Adding an existing entry and removing an absent one change nothing, and clearing empties
    /// the list, which stays sorted
    #[test]
    fn models_conf_update_adds_and_removes() {
        use crate::models::ModelsConfUpdate;
        // add two entries out of order
        let (a, b) = (entry("https://a/v1"), entry("https://b/v1"));
        let mut settings = SystemSettings::default();
        ModelsConfUpdate::default()
            .add_endpoint(b.clone())
            .add_endpoint(a.clone())
            .update(&mut settings);
        assert_eq!(
            settings.models.allowed_endpoints.iter().collect::<Vec<_>>(),
            vec![&a, &b]
        );
        // adding an existing entry and removing an absent one change nothing
        let before = settings.models.clone();
        ModelsConfUpdate::default()
            .add_endpoint(a.clone())
            .remove_endpoint(entry("https://c/v1"))
            .update(&mut settings);
        assert_eq!(settings.models, before);
        // clearing empties the list
        ModelsConfUpdate::default().clear().update(&mut settings);
        assert!(settings.models.allowed_endpoints.is_empty());
    }

    /// An entry's url is kept with at most one trailing slash removed, so one spelling of a url
    /// removes the other
    #[test]
    fn models_conf_normalizes_urls() {
        use crate::models::{AllowedEndpoint, ModelsConfUpdate};
        // allow a url with a trailing slash, built and deserialized
        let deserialized: AllowedEndpoint = serde_json::from_value(serde_json::json!({
            "provider": "OpenAiCompatible",
            "base_url": "https://e2/v1/"
        }))
        .unwrap();
        let mut settings = SystemSettings::default();
        ModelsConfUpdate::default()
            .add_endpoint(entry("https://e1/v1/"))
            .add_endpoint(deserialized)
            .update(&mut settings);
        // the url without the slash is the same entry
        assert!(
            settings
                .models
                .allowed_endpoints
                .contains(&entry("https://e1/v1"))
        );
        // removing either spelling removes the entry
        ModelsConfUpdate::default()
            .remove_endpoint(entry("https://e1/v1"))
            .remove_endpoint(entry("https://e2/v1/"))
            .update(&mut settings);
        assert!(settings.models.allowed_endpoints.is_empty());
        let provider = crate::models::ModelProvider::OpenAiCompatible {
            base_url: "https://e1/v1".to_owned(),
            extra_headers: std::collections::BTreeMap::new(),
            key_header: None,
        };
        assert!(!settings.models.allows(&provider));
    }

    /// The entry built for a provider allows it whatever trailing slashes its url ends in, and
    /// normalizing an entry again leaves it unchanged
    #[test]
    fn provider_entries_allow_their_provider() {
        use crate::models::{AllowedEndpoint, ModelProvider, ModelsConfUpdate};
        for url in [
            "https://e/v1",
            "https://e/v1/",
            "https://e/v1//",
            "https://e/v1///",
        ] {
            // the provider's own entry matches it
            let provider = ModelProvider::OpenAiCompatible {
                base_url: url.to_owned(),
                extra_headers: std::collections::BTreeMap::new(),
                key_header: None,
            };
            let own = AllowedEndpoint::for_provider(&provider);
            assert!(own.matches(&provider), "{url} doesn't match its own entry");
            // normalizing the entry again keeps its url
            assert_eq!(own.clone().normalized(), own, "{url}");
            // the list allows the provider once its entry is added
            let mut settings = SystemSettings::default();
            ModelsConfUpdate::default()
                .add_endpoint(own)
                .update(&mut settings);
            assert!(
                settings.models.allows(&provider),
                "{url} isn't allowed once its entry is added"
            );
        }
    }
}
