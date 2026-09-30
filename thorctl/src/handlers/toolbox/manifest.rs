//! The toolbox manifest structure

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thorium::Error;
use thorium::models::{ImageRequest, NetworkPolicyRequest, PipelineRequest};

use super::prompt;
use crate::utils;

/// A toolbox manifest – a description of pipelines and images that
/// can be imported into Thorium
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolboxManifest {
    /// The name of this toolbox
    pub name: String,
    /// The registry the images can be found at, if the toolbox declares a central one
    ///
    /// Optional: a toolbox documenting tools whose images live in various external
    /// registries leaves this unset and relies on each image config's own `image` url.
    /// Informational on import (the per-image `image` urls are authoritative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// A map of pipeline names to their details
    pub pipelines: HashMap<String, PipelineManifest>,
    /// A map of image names to their details
    pub images: HashMap<String, ImageManifest>,
    /// Whether this toolbox bundles container image tarballs alongside its configs
    ///
    /// When true, an import must load each image's `<name>.tar.gz` tarball (found in the
    /// image version's `dir`, or `images/<name>/` when `dir` is empty), push it to a target
    /// registry, and rewrite the image's url before creating it in Thorium.
    #[serde(default)]
    pub bundled_images: bool,
    /// The default registry base path that bundled images are pushed under on import
    ///
    /// Used as the fallback when `--image-path-prefix` is not given. Bundled images are
    /// pushed to `<image_path_prefix>/<group>/<name>:<tag>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path_prefix: Option<String>,
}

/// The display label of a pipeline version (`group/name@version`, or `name@version`
/// when the version has no resolved config to take a group from)
///
/// # Arguments
///
/// * `pipeline` - The pipeline's manifest key
/// * `version_name` - The pipeline's version
/// * `version` - The pipeline version, whose config supplies the group when resolved
fn pipeline_label(pipeline: &str, version_name: &str, version: &PipelineVersion) -> String {
    utils::entry_id(
        version.config.as_ref().map(|config| config.group.as_str()),
        pipeline,
        version_name,
    )
}

/// A report of the image and pipeline versions dropped during lenient
/// validation, so the caller can warn the user about what was skipped
#[derive(Debug, Default)]
pub struct DroppedItems {
    /// `(label, reason)` for each removed image version, labelled `group/name@version` (or `name@version` without a group)
    pub images: Vec<(String, String)>,
    /// `(label, reasons)` for each removed pipeline version, labelled `group/name@version` (or `name@version` without a group)
    pub pipelines: Vec<(String, Vec<String>)>,
}

impl DroppedItems {
    /// Sort the dropped images and pipelines by label so the warnings built from
    /// this report come out in the same order on every run
    fn sort(&mut self) {
        // the removals are gathered from HashMap iteration, so order them by label
        self.images.sort();
        self.pipelines.sort();
    }
}

impl ToolboxManifest {
    /// Validate the manifest's intrinsic, group-independent structure, removing
    /// invalid image or pipeline versions instead of aborting
    ///
    /// Run this BEFORE any group override. Checks that every image and pipeline
    /// version has a resolved config, that every image a pipeline names (in its
    /// `images` map or its order) is present somewhere in the manifest, and that
    /// the image order parses. Group membership is checked separately, after the
    /// override, by [`Self::validate_group_coherence`].
    ///
    /// Must be called after configs have been resolved (via
    /// `shared::resolve_manifest_configs`). Returns a [`DroppedItems`] report
    /// describing every version removed and why.
    pub fn validate_structural(&mut self) -> DroppedItems {
        // the running tally of everything removed, returned for the caller to warn on
        let mut dropped = DroppedItems::default();
        // drop images without a resolved config first; this must precede the pipeline
        // pass because a pipeline that depended on a just-removed image then fails the
        // manifest-membership check below and is dropped too
        self.drop_unconfigured_images(&mut dropped);
        // drop pipelines whose config/order/image references are unusable
        self.drop_structurally_invalid_pipelines(&mut dropped);
        // order the report so the resulting warnings are reproducible across runs
        dropped.sort();
        dropped
    }

    /// Validate that each pipeline's order references images present in the
    /// pipeline's group, dropping any that don't
    ///
    /// Run this AFTER the group override so it checks the final group layout.
    /// Returns a [`DroppedItems`] report describing every version removed.
    pub fn validate_group_coherence(&mut self) -> DroppedItems {
        // the running tally of everything removed, returned for the caller to warn on
        let mut dropped = DroppedItems::default();
        // drop pipelines whose order names an image not present in the pipeline's group
        self.drop_incoherent_pipelines(&mut dropped);
        // order the report so the resulting warnings are reproducible across runs
        dropped.sort();
        dropped
    }

    /// Remove image versions that have no resolved config, recording each in `dropped`
    ///
    /// # Arguments
    ///
    /// * `dropped` - The report to record each removed image version in
    fn drop_unconfigured_images(&mut self, dropped: &mut DroppedItems) {
        // walk every image, pruning unconfigured versions; an empty image is then removed
        self.images.retain(|name, manifest| {
            // keep only versions that resolved a config; record the rest with the
            // config_from they failed to resolve so the user can see what was unreachable
            manifest.versions.retain(|version, v| {
                if v.config.is_none() {
                    dropped.images.push((
                        utils::entry_id(None, name, version),
                        format!("config not resolved (config_from: {:?})", v.config_from),
                    ));
                    false
                } else {
                    true
                }
            });
            // drop the image entry entirely once all of its versions are gone
            !manifest.versions.is_empty()
        });
    }

    /// Remove pipeline versions that lack a resolved config, have a malformed
    /// image order, or reference (in their `images` map or order) an image absent
    /// from the manifest, recording each in `dropped`
    ///
    /// # Arguments
    ///
    /// * `dropped` - The report to record each removed pipeline version in
    fn drop_structurally_invalid_pipelines(&mut self, dropped: &mut DroppedItems) {
        // build the lookup indexes once so the per-pipeline checks below are O(1)
        // instead of re-scanning every image version for each order entry/image ref.
        // an image reference may be a top-level manifest key (a tool name, e.g.
        // "sqlitediff") OR an image's config name (e.g. "sqldiff"), so both are indexed.
        let config_names: HashSet<&str> = self
            .images
            .values()
            .flat_map(|image_manifest| image_manifest.versions.values())
            .filter_map(|v| v.config.as_ref())
            .map(|config| config.name.as_str())
            .collect();
        let ref_names: HashSet<&str> = self
            .images
            .keys()
            .map(String::as_str)
            .chain(config_names.iter().copied())
            .collect();
        // index every (reference-name, version) pair an image is reachable under, again
        // accepting both the manifest key and the config name so a pipeline pinning either
        // form resolves; used to validate the pipeline's per-image version pins
        let mut ref_versions: HashSet<(&str, &str)> = HashSet::new();
        for (key, image_manifest) in &self.images {
            for (version, v) in &image_manifest.versions {
                ref_versions.insert((key.as_str(), version.as_str()));
                if let Some(config) = &v.config {
                    ref_versions.insert((config.name.as_str(), version.as_str()));
                }
            }
        }
        // gather the reasons each pipeline version is invalid into a side map keyed by
        // the version's display label, since we only hold a shared borrow of self while iterating
        // and can't mutate the pipelines until the scan finishes
        let mut invalid: HashMap<String, Vec<String>> = HashMap::new();
        for (pipeline, pipeline_manifest) in &self.pipelines {
            for (version_name, version) in &pipeline_manifest.versions {
                let mut reasons = Vec::new();
                // every image the pipeline pins in its map must exist at the pinned version;
                // distinguish "image absent entirely" from "image present, wrong version"
                for (image_name, image_version) in &version.images {
                    if !ref_versions
                        .contains(&(image_name.as_str(), image_version.version.as_str()))
                    {
                        if ref_names.contains(image_name.as_str()) {
                            reasons.push(format!(
                                "requires image '{}' not in manifest",
                                utils::entry_id(None, image_name, &image_version.version)
                            ));
                        } else {
                            reasons.push(format!("requires image '{image_name}' not in manifest"));
                        }
                    }
                }
                // the config must have resolved and its order must parse before we can
                // check the order entries; a missing or malformed config is itself fatal
                match &version.config {
                    None => reasons.push("config not resolved".to_string()),
                    Some(config) => match config.deserialize_image_order() {
                        Err(err) => reasons.push(format!("malformed image order: {err}")),
                        Ok(order) => {
                            // order entries always use the config name, so check them against
                            // config_names only (not the manifest-key alias set)
                            let mut missing: Vec<&str> = Vec::new();
                            for image in order.iter().flat_map(|sub_order| sub_order.iter()) {
                                if !config_names.contains(image) {
                                    missing.push(image);
                                }
                            }
                            if !missing.is_empty() {
                                reasons.push(format!(
                                    "order references image(s) not in manifest: {missing:?}"
                                ));
                            }
                        }
                    },
                }
                // a version with any accumulated reason is marked invalid for removal
                if !reasons.is_empty() {
                    invalid.insert(pipeline_label(pipeline, version_name, version), reasons);
                }
            }
        }
        // apply the removals now that the borrow of self.pipelines has been released
        Self::remove_invalid_pipeline_versions(&mut self.pipelines, &invalid, dropped);
    }

    /// Remove pipeline versions whose order references images not present in the
    /// pipeline's group, recording each in `dropped`
    ///
    /// # Arguments
    ///
    /// * `dropped` - The report to record each removed pipeline version in
    fn drop_incoherent_pipelines(&mut self, dropped: &mut DroppedItems) {
        // map of group -> set of image names present in the (surviving) manifest,
        // so the per-order-entry membership check below is O(1)
        let group_images = self
            .images
            .values()
            .flat_map(|image_manifest| image_manifest.versions.values())
            .filter_map(|v| v.config.as_ref())
            .fold(HashMap::<&str, HashSet<&str>>::new(), |mut map, config| {
                map.entry(&config.group).or_default().insert(&config.name);
                map
            });
        let mut invalid: HashMap<String, Vec<String>> = HashMap::new();
        for (pipeline, pipeline_manifest) in &self.pipelines {
            for (version_name, version) in &pipeline_manifest.versions {
                // structural validation already dropped versions without a config
                // or with a malformed order, so skip anything that won't parse
                let Some(config) = &version.config else {
                    continue;
                };
                let Ok(order) = config.deserialize_image_order() else {
                    continue;
                };
                // the images actually present in this pipeline's (post-override) group;
                // None when the group has no images at all, which makes every entry missing
                let images_in_group = group_images.get(config.group.as_str());
                // an order entry is incoherent when its image isn't in the pipeline's group,
                // even if that image exists elsewhere in the manifest under another group
                let mut missing: Vec<&str> = Vec::new();
                for image in order.iter().flat_map(|sub_order| sub_order.iter()) {
                    if !images_in_group.is_some_and(|names| names.contains(image)) {
                        missing.push(image);
                    }
                }
                if !missing.is_empty() {
                    invalid.insert(
                        pipeline_label(pipeline, version_name, version),
                        vec![format!(
                            "order references image(s) not in group '{}': {missing:?}",
                            config.group
                        )],
                    );
                }
            }
        }
        // apply the removals now that the borrow of self.pipelines has been released
        Self::remove_invalid_pipeline_versions(&mut self.pipelines, &invalid, dropped);
    }

    /// Remove the pipeline versions named in `invalid` (keyed per [`pipeline_label`]),
    /// dropping any pipeline left with no versions, and record them in `dropped`
    ///
    /// # Arguments
    ///
    /// * `pipelines` - The manifest's pipelines to remove the invalid versions from
    /// * `invalid` - Map of pipeline version labels to the reasons each is invalid
    /// * `dropped` - The report to record each removed pipeline version in
    fn remove_invalid_pipeline_versions(
        pipelines: &mut HashMap<String, PipelineManifest>,
        invalid: &HashMap<String, Vec<String>>,
        dropped: &mut DroppedItems,
    ) {
        // nothing flagged means no mutation and no report entries
        if invalid.is_empty() {
            return;
        }
        // drop each flagged version, then drop any pipeline left with no versions
        pipelines.retain(|pipeline, pipeline_manifest| {
            pipeline_manifest.versions.retain(|version_name, version| {
                !invalid.contains_key(&pipeline_label(pipeline, version_name, version))
            });
            !pipeline_manifest.versions.is_empty()
        });
        // record every removal (with its reasons) so the caller can warn the user
        for (key, reasons) in invalid {
            dropped.pipelines.push((key.clone(), reasons.clone()));
        }
    }

    /// Returns all of the groups the manifest expects to exist
    ///
    /// Collected from every resolved image and pipeline config; unconfigured
    /// versions contribute no group. The caller uses this to create any group
    /// missing from the target instance before importing.
    pub fn groups(&self) -> HashSet<String> {
        // union the group of every resolved pipeline config with that of every
        // resolved image config; the HashSet collapses duplicates
        self.pipelines
            .values()
            .flat_map(|pipeline_manifest| pipeline_manifest.versions.values())
            .filter_map(|v| v.config.as_ref())
            .map(|config| &config.group)
            .chain(
                self.images
                    .values()
                    .flat_map(|image_manifest| image_manifest.versions.values())
                    .filter_map(|v| v.config.as_ref())
                    .map(|config| &config.group),
            )
            .cloned()
            .collect()
    }

    /// Force all images and pipelines to be imported to the given group by
    /// setting the group for each item in the manifest, returning the updated
    /// manifest
    ///
    /// Each image version's bundled network policies are scoped to the same group
    /// so they stay usable by the images that reference them.
    ///
    /// # Arguments
    ///
    /// * `group` - The group to force items to be imported to
    pub fn override_group(mut self, group: &str) -> Self {
        // own the target group string once so each assignment below can clone from it
        let group = group.to_string();
        // chain a mutable reference to every resolved pipeline and image config's group,
        // then point each at the target; unconfigured versions have no group to set
        self.pipelines
            .values_mut()
            .flat_map(|pipeline_manifest| pipeline_manifest.versions.values_mut())
            .filter_map(|v| v.config.as_mut())
            .map(|config| &mut config.group)
            .chain(
                self.images
                    .values_mut()
                    .flat_map(|image_manifest| image_manifest.versions.values_mut())
                    .filter_map(|v| v.config.as_mut())
                    .map(|config| &mut config.group),
            )
            // set each group reference to the given group
            .for_each(|group_ref| group_ref.clone_from(&group));
        // scope every bundled network policy to the target group as well, since the
        // policies' original groups may not exist in the target instance
        self.images
            .values_mut()
            .flat_map(|image_manifest| image_manifest.versions.values_mut())
            .flat_map(|v| v.network_policies.iter_mut())
            .for_each(|policy| policy.groups = vec![group.clone()]);
        self
    }

    // ─── Collision detection & resolution ────────────────────────────────────

    /// Snapshot each image/pipeline version's current group, keyed by
    /// `(manifest_key, version)`, so collision resolution can later disambiguate
    /// which pipeline wanted which image variant by its original group
    ///
    /// Call this BEFORE [`Self::override_group`] collapses the groups.
    pub fn capture_source_groups(&self) -> SourceGroups {
        // record each resolved image version's group keyed by (manifest_key, version);
        // unconfigured versions are skipped because they have no group to remember
        let images = self
            .images
            .iter()
            .flat_map(|(key, manifest)| {
                manifest.versions.iter().filter_map(move |(version, v)| {
                    v.config
                        .as_ref()
                        .map(|c| ((key.clone(), version.clone()), c.group.clone()))
                })
            })
            .collect();
        // same snapshot for pipeline versions, so a renamed image's dependents can be
        // matched back to the variant they originally wanted by their own source group
        let pipelines = self
            .pipelines
            .iter()
            .flat_map(|(key, manifest)| {
                manifest.versions.iter().filter_map(move |(version, v)| {
                    v.config
                        .as_ref()
                        .map(|c| ((key.clone(), version.clone()), c.group.clone()))
                })
            })
            .collect();
        SourceGroups { images, pipelines }
    }

    /// Find sets of image versions that resolve to the same `(group, name)`
    /// Thorium identity and would therefore overwrite each other on import
    ///
    /// # Arguments
    ///
    /// * `sources` - The pre-override group snapshot used to tag each colliding member
    pub fn detect_image_collisions(&self, sources: &SourceGroups) -> Result<Vec<Collision>, Error> {
        // group members by their post-override (group, name) identity; every bucket with
        // more than one member is an overwrite collision
        let mut buckets: HashMap<(String, String), Vec<(CollisionMember, serde_json::Value)>> =
            HashMap::new();
        for (key, manifest) in &self.images {
            for (version, v) in &manifest.versions {
                // unconfigured versions have no identity to collide on
                let Some(config) = &v.config else {
                    continue;
                };
                // recover the pre-override group from the snapshot; fall back to the
                // current group when this version wasn't captured (e.g. no override ran)
                let source_group = sources
                    .images
                    .get(&(key.clone(), version.clone()))
                    .cloned()
                    .unwrap_or_else(|| config.group.clone());
                let member = CollisionMember {
                    manifest_key: key.clone(),
                    version: version.clone(),
                    source_group,
                };
                // a serialization failure must not be coerced to a shared `Null`,
                // or two distinct configs would compare "identical" and be wrongly
                // auto-deduped — surface it instead. set-valued fields are sorted so
                // equal configs compare equal regardless of hash iteration order
                let json = canonical_image_value(config).map_err(|err| {
                    Error::new(format!(
                        "Failed to serialize image '{}' for collision check: {err}",
                        config.name
                    ))
                })?;
                // file the member under its identity alongside its serialized config so
                // buckets_to_collisions can tell a pure duplicate from a real conflict
                buckets
                    .entry((config.group.clone(), config.name.clone()))
                    .or_default()
                    .push((member, json));
            }
        }
        // collapse the buckets into the multi-member collisions only
        Ok(Self::buckets_to_collisions(buckets))
    }

    /// Find sets of pipeline versions that resolve to the same `(group, name)`
    /// Thorium identity and would therefore overwrite each other on import
    ///
    /// # Arguments
    ///
    /// * `sources` - The pre-override group snapshot used to tag each colliding member
    pub fn detect_pipeline_collisions(
        &self,
        sources: &SourceGroups,
    ) -> Result<Vec<Collision>, Error> {
        // group members by their post-override (group, name) identity; every bucket with
        // more than one member is an overwrite collision
        let mut buckets: HashMap<(String, String), Vec<(CollisionMember, serde_json::Value)>> =
            HashMap::new();
        for (key, manifest) in &self.pipelines {
            for (version, v) in &manifest.versions {
                // unconfigured versions have no identity to collide on
                let Some(config) = &v.config else {
                    continue;
                };
                // recover the pre-override group from the snapshot; fall back to the
                // current group when this version wasn't captured (e.g. no override ran)
                let source_group = sources
                    .pipelines
                    .get(&(key.clone(), version.clone()))
                    .cloned()
                    .unwrap_or_else(|| config.group.clone());
                let member = CollisionMember {
                    manifest_key: key.clone(),
                    version: version.clone(),
                    source_group,
                };
                // see detect_image_collisions: never coerce a serialization failure
                // into a shared `Null` that would falsely read as a duplicate
                let json = serde_json::to_value(config).map_err(|err| {
                    Error::new(format!(
                        "Failed to serialize pipeline '{}' for collision check: {err}",
                        config.name
                    ))
                })?;
                // file the member under its identity alongside its serialized config so
                // buckets_to_collisions can tell a pure duplicate from a real conflict
                buckets
                    .entry((config.group.clone(), config.name.clone()))
                    .or_default()
                    .push((member, json));
            }
        }
        // collapse the buckets into the multi-member collisions only
        Ok(Self::buckets_to_collisions(buckets))
    }

    /// Turn `(group, name)` buckets into deterministic [`Collision`]s, keeping
    /// only buckets with more than one member
    ///
    /// # Arguments
    ///
    /// * `buckets` - Map of `(group, name)` identity to its members and their serialized configs
    fn buckets_to_collisions(
        buckets: HashMap<(String, String), Vec<(CollisionMember, serde_json::Value)>>,
    ) -> Vec<Collision> {
        let mut collisions = Vec::new();
        for ((group, name), members) in buckets {
            // a single occupant of an identity isn't a collision
            if members.len() < 2 {
                continue;
            }
            // a bucket where every member's config is byte-identical is a pure
            // duplicate, safe to de-dupe without asking the user to pick
            let first = &members[0].1;
            let identical = members.iter().all(|(_, json)| json == first);
            // the serialized configs were only needed for the identical check; drop them
            let mut members: Vec<CollisionMember> = members.into_iter().map(|(m, _)| m).collect();
            // sort by (manifest_key, version) so the first member is a stable canonical
            // choice and the rendered collision is reproducible across runs
            members
                .sort_by(|a, b| (&a.manifest_key, &a.version).cmp(&(&b.manifest_key, &b.version)));
            collisions.push(Collision {
                group,
                name,
                members,
                identical,
            });
        }
        // sort the collisions themselves so the import/diff output order is deterministic
        collisions.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
        collisions
    }

    /// Suggest a unique, valid new name for a colliding image member
    ///
    /// Based on `<name>-<version>`, sanitized to the Thorium name rule (lowercase
    /// letters, digits, and '-', at most [`prompt::RESOURCE_NAME_MAX`] characters),
    /// with a numeric suffix appended if that name is already taken (see
    /// [`Self::image_rename_conflict`]).
    ///
    /// # Arguments
    ///
    /// * `collision` - The collision the member belongs to (supplies the group and base name)
    /// * `member` - The colliding member to suggest a new name for (supplies the version)
    pub fn suggested_image_rename(
        &self,
        collision: &Collision,
        member: &CollisionMember,
    ) -> String {
        // base the suggestion on a sanitized "<name>-<version>"
        let base = suggestion_base(&collision.name, &member.version);
        // suffix it until it neither collides in the group nor overwrites a manifest entry
        unique_name(&base, |candidate| {
            self.image_rename_conflict(&collision.group, member, candidate)
                .is_some()
        })
    }

    /// Suggest a unique, valid new name for a colliding pipeline member
    ///
    /// Based on `<name>-<version>`, sanitized to the Thorium name rule, with a
    /// numeric suffix appended if that name is already taken (see
    /// [`Self::pipeline_rename_conflict`]).
    ///
    /// # Arguments
    ///
    /// * `collision` - The collision the member belongs to (supplies the group and base name)
    /// * `member` - The colliding member to suggest a new name for (supplies the version)
    pub fn suggested_pipeline_rename(
        &self,
        collision: &Collision,
        member: &CollisionMember,
    ) -> String {
        // base the suggestion on a sanitized "<name>-<version>"
        let base = suggestion_base(&collision.name, &member.version);
        // suffix it until it neither collides in the group nor overwrites a manifest entry
        unique_name(&base, |candidate| {
            self.pipeline_rename_conflict(&collision.group, member, candidate)
                .is_some()
        })
    }

    /// Every image config's `(group, name)` identity across all versions
    fn image_identities(&self) -> impl Iterator<Item = (&str, &str)> {
        // flatten every resolved image version down to its Thorium identity; unconfigured
        // versions are skipped because they have no identity yet
        self.images
            .values()
            .flat_map(|manifest| manifest.versions.values())
            .filter_map(|v| v.config.as_ref())
            .map(|config| (config.group.as_str(), config.name.as_str()))
    }

    /// Every pipeline config's `(group, name)` identity across all versions
    fn pipeline_identities(&self) -> impl Iterator<Item = (&str, &str)> {
        // flatten every resolved pipeline version down to its Thorium identity; unconfigured
        // versions are skipped because they have no identity yet
        self.pipelines
            .values()
            .flat_map(|manifest| manifest.versions.values())
            .filter_map(|v| v.config.as_ref())
            .map(|config| (config.group.as_str(), config.name.as_str()))
    }

    /// Why renaming an image member to `candidate` is not allowed, or `None` when it is
    ///
    /// A rename is rejected when another image in the group already uses the name
    /// (it would create a fresh collision) or when another top-level manifest entry
    /// is already keyed by the name (the moved version would be filed into, and could
    /// overwrite, that entry). The member's own manifest key is allowed.
    ///
    /// # Arguments
    ///
    /// * `group` - The group the renamed image is imported into
    /// * `member` - The colliding member being renamed
    /// * `candidate` - The proposed new name
    pub fn image_rename_conflict(
        &self,
        group: &str,
        member: &CollisionMember,
        candidate: &str,
    ) -> Option<String> {
        // a name already used by an image in the group would collide again on import
        if self
            .image_identities()
            .any(|(g, n)| g == group && n == candidate)
        {
            return Some(format!(
                "'{candidate}' is already used by another image in this group"
            ));
        }
        // a name already used as another entry's manifest key would merge into that entry
        if candidate != member.manifest_key && self.images.contains_key(candidate) {
            return Some(format!(
                "'{candidate}' is already used by another image entry in the toolbox manifest"
            ));
        }
        None
    }

    /// Why renaming a pipeline member to `candidate` is not allowed, or `None` when it is
    ///
    /// See [`Self::image_rename_conflict`]; the same group-name and manifest-key
    /// rules apply to pipelines.
    ///
    /// # Arguments
    ///
    /// * `group` - The group the renamed pipeline is imported into
    /// * `member` - The colliding member being renamed
    /// * `candidate` - The proposed new name
    pub fn pipeline_rename_conflict(
        &self,
        group: &str,
        member: &CollisionMember,
        candidate: &str,
    ) -> Option<String> {
        // a name already used by a pipeline in the group would collide again on import
        if self
            .pipeline_identities()
            .any(|(g, n)| g == group && n == candidate)
        {
            return Some(format!(
                "'{candidate}' is already used by another pipeline in this group"
            ));
        }
        // a name already used as another entry's manifest key would merge into that entry
        if candidate != member.manifest_key && self.pipelines.contains_key(candidate) {
            return Some(format!(
                "'{candidate}' is already used by another pipeline entry in the toolbox manifest"
            ));
        }
        None
    }

    /// The set of image names currently used in `group`
    ///
    /// # Arguments
    ///
    /// * `group` - The group whose image names to collect
    #[cfg(test)]
    pub fn image_names_in_group(&self, group: &str) -> HashSet<String> {
        // keep only the names whose identity is in the requested group
        self.image_identities()
            .filter(|(g, _)| *g == group)
            .map(|(_, n)| n.to_string())
            .collect()
    }

    /// De-duplicate a pure-duplicate image collision by keeping the first member
    /// and removing the rest (their configs are identical, so dependents are
    /// unaffected)
    ///
    /// # Arguments
    ///
    /// * `collision` - The identical-config collision whose extra members to remove
    pub fn dedupe_image_collision(&mut self, collision: &Collision) {
        // members[0] is the sorted canonical keeper; remove every other version, which is
        // safe only because the caller guarantees this collision is byte-identical
        for member in collision.members.iter().skip(1) {
            self.remove_image_version(&member.manifest_key, &member.version);
        }
    }

    /// De-duplicate a pure-duplicate pipeline collision by keeping the first member
    ///
    /// # Arguments
    ///
    /// * `collision` - The identical-config collision whose extra members to remove
    pub fn dedupe_pipeline_collision(&mut self, collision: &Collision) {
        // members[0] is the sorted canonical keeper; remove every other version, which is
        // safe only because the caller guarantees this collision is byte-identical
        for member in collision.members.iter().skip(1) {
            self.remove_pipeline_version(&member.manifest_key, &member.version);
        }
    }

    /// Remove a single image version, dropping the image entry if it becomes empty
    ///
    /// # Arguments
    ///
    /// * `manifest_key` - The top-level image key the version lives under
    /// * `version` - The version label to remove
    fn remove_image_version(&mut self, manifest_key: &str, version: &str) {
        // a missing key is a no-op; only touch the entry when it exists
        if let Some(manifest) = self.images.get_mut(manifest_key) {
            // drop the one version, then drop the whole entry if that emptied it so no
            // version-less image lingers in the manifest
            manifest.versions.remove(version);
            if manifest.versions.is_empty() {
                self.images.remove(manifest_key);
            }
        }
    }

    /// Remove a single pipeline version, dropping the pipeline entry if it becomes empty
    ///
    /// # Arguments
    ///
    /// * `manifest_key` - The top-level pipeline key the version lives under
    /// * `version` - The version label to remove
    fn remove_pipeline_version(&mut self, manifest_key: &str, version: &str) {
        // a missing key is a no-op; only touch the entry when it exists
        if let Some(manifest) = self.pipelines.get_mut(manifest_key) {
            // drop the one version, then drop the whole entry if that emptied it so no
            // version-less pipeline lingers in the manifest
            manifest.versions.remove(version);
            if manifest.versions.is_empty() {
                self.pipelines.remove(manifest_key);
            }
        }
    }

    /// Remove every image version matching `(group, name)` along with every
    /// pipeline that references that image, returning the dropped pipeline labels
    ///
    /// Used for the non-interactive (or user-chosen) skip resolution: the whole
    /// colliding identity is dropped, so any pipeline that needed it is dropped too.
    ///
    /// # Arguments
    ///
    /// * `group` - The group of the image identity to remove
    /// * `name` - The name of the image identity to remove
    pub fn remove_image_identity_and_dependents(&mut self, group: &str, name: &str) -> Vec<String> {
        // first drop every image version matching the (group, name) identity; an
        // unconfigured version (no group/name) can never match, so it is kept
        self.images.retain(|_key, manifest| {
            manifest.versions.retain(|_version, v| {
                v.config
                    .as_ref()
                    .is_none_or(|c| !(c.group == group && c.name == name))
            });
            !manifest.versions.is_empty()
        });
        // then drop every pipeline version that referenced the now-removed image, since
        // it can no longer run; collect their labels to report which dependents fell
        let mut dropped_pipelines = Vec::new();
        self.pipelines.retain(|pipeline_key, manifest| {
            manifest.versions.retain(|version_name, version| {
                // image references are bare names resolved within the pipeline's own
                // group, so only a pipeline in the removed image's group could have
                // referenced it; a same-named image in another group is unrelated and
                // its dependents must not be dropped
                let in_same_group = version
                    .config
                    .as_ref()
                    .is_some_and(|config| config.group == group);
                if in_same_group && pipeline_references_image(version, name) {
                    dropped_pipelines.push(pipeline_label(pipeline_key, version_name, version));
                    false
                } else {
                    true
                }
            });
            !manifest.versions.is_empty()
        });
        // sort so the dropped-dependents list is deterministic for the caller's warning
        dropped_pipelines.sort();
        dropped_pipelines
    }

    /// Remove every pipeline version matching `(group, name)`
    ///
    /// # Arguments
    ///
    /// * `group` - The group of the pipeline identity to remove
    /// * `name` - The name of the pipeline identity to remove
    pub fn remove_pipeline_identity(&mut self, group: &str, name: &str) {
        // drop every pipeline version matching the (group, name) identity; an unconfigured
        // version (no group/name) can never match, so it is kept. no cascade is needed
        // because nothing else in the manifest references a pipeline by name
        self.pipelines.retain(|_key, manifest| {
            manifest.versions.retain(|_version, v| {
                v.config
                    .as_ref()
                    .is_none_or(|c| !(c.group == group && c.name == name))
            });
            !manifest.versions.is_empty()
        });
    }

    /// Rename one colliding image member to `new_name` and repoint everything in
    /// the group that wanted *that* member's variant
    ///
    /// The version is split into its own top-level entry under `new_name` (with
    /// `config.name` updated). A dependent pipeline is matched to a variant by its
    /// version pin (under the image's config name or the member's manifest key),
    /// narrowed by its original (pre-override) group when the pin alone is not
    /// decisive. Other images' result/children dependencies on the name are matched
    /// by their original group. A dependent that can't be matched to exactly one
    /// variant is left on the variant that keeps the original name, and a warning
    /// describing it is returned.
    ///
    /// # Arguments
    ///
    /// * `collision` - The collision being resolved (supplies the original name)
    /// * `member` - The colliding member to rename
    /// * `new_name` - The new name to give the member
    /// * `sources` - The pre-override group snapshot used to disambiguate dependents
    pub fn rename_image_member(
        &mut self,
        collision: &Collision,
        member: &CollisionMember,
        new_name: &str,
        sources: &SourceGroups,
    ) -> Result<Vec<String>, Error> {
        // refuse a name that would collide again or overwrite another manifest entry,
        // before anything is moved
        if let Some(reason) = self.image_rename_conflict(&collision.group, member, new_name) {
            return Err(Error::new(format!(
                "Failed to rename image '{}' to '{new_name}': {reason}",
                utils::resource_id(&collision.group, &collision.name)
            )));
        }
        // the identity name every dependent currently points at
        let old_name = collision.name.as_str();
        // pull just the colliding version out of its current entry and re-file it under
        // new_name, so the other versions of that entry keep the original name
        if let Some(manifest) = self.images.get_mut(&member.manifest_key)
            && let Some(mut version) = manifest.versions.remove(&member.version)
        {
            // rewrite the config name so the imported image carries the new identity
            if let Some(config) = &mut version.config {
                config.name = new_name.to_string();
            }
            // remove the source entry if pulling that version emptied it
            if manifest.versions.is_empty() {
                self.images.remove(&member.manifest_key);
            }
            // file the moved version under the new top-level key; the conflict check above
            // guarantees this key is either unused or the member's own entry, which no
            // longer holds this version, so nothing is overwritten
            self.images
                .entry(new_name.to_string())
                .or_insert_with(|| ImageManifest {
                    versions: HashMap::new(),
                })
                .versions
                .insert(member.version.clone(), version);
        }
        // dependents that couldn't be matched to exactly one variant, reported to the caller
        let mut warnings = Vec::new();
        // repoint only the same-group pipelines that wanted *this* renamed variant; the
        // others keep pointing at old_name (which now belongs to a different member)
        for (pipeline_key, manifest) in &mut self.pipelines {
            for (version_name, version) in &mut manifest.versions {
                // image references resolve within the pipeline's own group, so a pipeline in
                // another group never referenced this image
                let in_group = version
                    .config
                    .as_ref()
                    .is_some_and(|config| config.group == collision.group);
                // skip pipelines outside the group or that never referenced the identity
                if !in_group || !pipeline_references_collision(version, collision) {
                    continue;
                }
                // match the pipeline to the variant it wanted by pin, then by source group
                let source = sources
                    .pipelines
                    .get(&(pipeline_key.clone(), version_name.clone()));
                match wanted_pipeline_member(version, collision, source) {
                    // rewrite both the images map and the order so the pipeline tracks the move
                    Some(wanted) if wanted == member => {
                        rewrite_pipeline_image_ref(version, old_name, member, new_name);
                    }
                    // the pipeline wanted another variant, so it stays on its current name
                    Some(_) => {}
                    // no pin or source group tells the variants apart; it stays on the
                    // variant keeping the original name, which the user is told about
                    None => warnings.push(format!(
                        "Pipeline '{}' does not identify which variant of image '{}' it uses; \
                         it keeps using the one that retains the name '{old_name}'",
                        utils::entry_id(Some(&collision.group), pipeline_key, version_name),
                        utils::resource_id(&collision.group, old_name)
                    )),
                }
            }
        }
        // repoint other images whose result/children dependencies named this variant
        for (image_key, manifest) in &mut self.images {
            for (version_name, version) in &mut manifest.versions {
                // only same-group images can depend on this image by name
                let Some(config) = version.config.as_mut() else {
                    continue;
                };
                // the colliding variants themselves are not dependents of their own name
                if config.group != collision.group
                    || config.name == old_name
                    || config.name == new_name
                {
                    continue;
                }
                // skip images that don't depend on the colliding name at all
                let deps = &mut config.dependencies;
                if !deps.results.images.iter().any(|name| name == old_name)
                    && !deps.children.images.iter().any(|name| name == old_name)
                {
                    continue;
                }
                // images carry no version pins, so match them by their source group only
                let source = sources
                    .images
                    .get(&(image_key.clone(), version_name.clone()));
                match member_by_source_group(&collision.members, source) {
                    // swap the old name for the new one in both dependency lists
                    Some(wanted) if wanted == member => {
                        for name in deps
                            .results
                            .images
                            .iter_mut()
                            .chain(deps.children.images.iter_mut())
                        {
                            if name == old_name {
                                *name = new_name.to_string();
                            }
                        }
                    }
                    // the image depended on another variant, so it stays as-is
                    Some(_) => {}
                    // ambiguous: leave it on the variant keeping the name and tell the user
                    None => warnings.push(format!(
                        "Image '{}' depends on image '{}' but does not identify which variant; \
                         it keeps depending on the one that retains the name '{old_name}'",
                        utils::entry_id(Some(&collision.group), image_key, version_name),
                        utils::resource_id(&collision.group, old_name)
                    )),
                }
            }
        }
        // order the warnings so the caller's output is reproducible across runs
        warnings.sort();
        Ok(warnings)
    }

    /// Rename one colliding pipeline member to `new_name` (pipelines aren't
    /// referenced by name elsewhere in the manifest, so there is no cascade)
    ///
    /// # Arguments
    ///
    /// * `collision` - The collision being resolved (supplies the group and original name)
    /// * `member` - The colliding pipeline member to rename
    /// * `new_name` - The new name to give the member
    pub fn rename_pipeline_member(
        &mut self,
        collision: &Collision,
        member: &CollisionMember,
        new_name: &str,
    ) -> Result<(), Error> {
        // refuse a name that would collide again or overwrite another manifest entry,
        // before anything is moved
        if let Some(reason) = self.pipeline_rename_conflict(&collision.group, member, new_name) {
            return Err(Error::new(format!(
                "Failed to rename pipeline '{}' to '{new_name}': {reason}",
                utils::resource_id(&collision.group, &collision.name)
            )));
        }
        // pull just the colliding version out of its current entry and re-file it under
        // new_name, leaving the entry's other versions under the original name
        if let Some(manifest) = self.pipelines.get_mut(&member.manifest_key)
            && let Some(mut version) = manifest.versions.remove(&member.version)
        {
            // rewrite the config name so the imported pipeline carries the new identity
            if let Some(config) = &mut version.config {
                config.name = new_name.to_string();
            }
            // remove the source entry if pulling that version emptied it
            if manifest.versions.is_empty() {
                self.pipelines.remove(&member.manifest_key);
            }
            // file the moved version under the new top-level key; the conflict check above
            // guarantees this key is either unused or the member's own entry, which no
            // longer holds this version, so nothing is overwritten
            self.pipelines
                .entry(new_name.to_string())
                .or_insert_with(|| PipelineManifest {
                    versions: HashMap::new(),
                })
                .versions
                .insert(member.version.clone(), version);
        }
        Ok(())
    }
}

/// A snapshot of every image/pipeline version's group before a group override,
/// used to disambiguate collisions (see [`ToolboxManifest::capture_source_groups`])
#[derive(Debug, Default)]
pub struct SourceGroups {
    /// `(manifest_key, version)` -> pre-override group for images
    images: HashMap<(String, String), String>,
    /// `(manifest_key, version)` -> pre-override group for pipelines
    pipelines: HashMap<(String, String), String>,
}

/// A set of image (or pipeline) versions that resolve to the same `(group, name)`
/// Thorium identity and would overwrite each other on import
#[derive(Debug)]
pub struct Collision {
    /// The shared (post-override) group
    pub group: String,
    /// The shared Thorium name
    pub name: String,
    /// The colliding versions, sorted deterministically
    pub members: Vec<CollisionMember>,
    /// Whether every member's config is byte-identical (a pure duplicate)
    pub identical: bool,
}

/// One member of a [`Collision`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionMember {
    /// The top-level manifest key this version lives under
    pub manifest_key: String,
    /// The version label
    pub version: String,
    /// The version's group before any override, for disambiguation
    pub source_group: String,
}

/// Whether a pipeline version references the given image name in its `images`
/// map or its order
///
/// # Arguments
///
/// * `version` - The pipeline version to inspect
/// * `name` - The image name to look for
fn pipeline_references_image(version: &PipelineVersion, name: &str) -> bool {
    // a hit in the images map is conclusive without parsing the order
    if version.images.contains_key(name) {
        return true;
    }
    // otherwise the order may still name it; a missing or unparsable order counts as no
    // reference (is_some_and short-circuits both the None and the Err to false)
    version
        .config
        .as_ref()
        .and_then(|config| config.deserialize_image_order().ok())
        .is_some_and(|order| order.iter().flatten().any(|image| *image == name))
}

/// Whether a pipeline version references a collision's identity, either by the
/// shared config name (in its `images` map or order) or by any member's manifest key
/// in its `images` map
///
/// # Arguments
///
/// * `version` - The pipeline version to inspect
/// * `collision` - The collision whose identity to look for
fn pipeline_references_collision(version: &PipelineVersion, collision: &Collision) -> bool {
    // a reference by the shared config name covers both the images map and the order
    if pipeline_references_image(version, &collision.name) {
        return true;
    }
    // the images map may also key an image by its manifest key (tool name)
    collision
        .members
        .iter()
        .any(|member| version.images.contains_key(&member.manifest_key))
}

/// The collision member a dependent pipeline wanted, or `None` when it can't be
/// told apart from the other members
///
/// Members whose version is pinned in the pipeline's `images` map (under the shared
/// config name or the member's own manifest key) are the candidates; with no such pin
/// every member is. More than one candidate is narrowed by the pipeline's original
/// (pre-override) group.
///
/// # Arguments
///
/// * `version` - The dependent pipeline version
/// * `collision` - The collision whose members are matched against
/// * `source_group` - The pipeline's pre-override group, if it was captured
fn wanted_pipeline_member<'a>(
    version: &PipelineVersion,
    collision: &'a Collision,
    source_group: Option<&String>,
) -> Option<&'a CollisionMember> {
    // collect the members this pipeline pins by version under either reference form
    let pinned: Vec<&CollisionMember> = collision
        .members
        .iter()
        .filter(|member| {
            [collision.name.as_str(), member.manifest_key.as_str()]
                .iter()
                .any(|reference| {
                    version
                        .images
                        .get(*reference)
                        .is_some_and(|pin| pin.version == member.version)
                })
        })
        .collect();
    // without any matching pin every member remains a candidate
    let candidates: Vec<&CollisionMember> = if pinned.is_empty() {
        collision.members.iter().collect()
    } else {
        pinned
    };
    // a single candidate is decisive on its own
    if let [only] = candidates.as_slice() {
        return Some(*only);
    }
    // otherwise narrow by the pipeline's original group
    member_by_source_group(candidates, source_group)
}

/// The single member whose source group matches `source_group`, or `None` when no
/// member or more than one member matches
///
/// # Arguments
///
/// * `members` - The candidate members
/// * `source_group` - The dependent's pre-override group, if it was captured
fn member_by_source_group<'a, I>(
    members: I,
    source_group: Option<&String>,
) -> Option<&'a CollisionMember>
where
    I: IntoIterator<Item = &'a CollisionMember>,
{
    // an uncaptured source group can't disambiguate anything
    let source_group = source_group?;
    // keep only the members that came from the dependent's original group
    let mut matching = members
        .into_iter()
        .filter(|member| &member.source_group == source_group);
    // exactly one match identifies the member; zero or several is ambiguous
    match (matching.next(), matching.next()) {
        (Some(member), None) => Some(member),
        _ => None,
    }
}

/// Rewrite a pipeline version's references to a renamed member so they point at
/// `new_name`, in both the `images` map and the image order
///
/// The `images` map pin is moved from the shared config name, or from the member's
/// manifest key when it pins the member's version, to `new_name`.
///
/// # Arguments
///
/// * `version` - The pipeline version to rewrite in place
/// * `old_name` - The shared config name currently referenced
/// * `member` - The member that was renamed
/// * `new_name` - The image name to replace the references with
fn rewrite_pipeline_image_ref(
    version: &mut PipelineVersion,
    old_name: &str,
    member: &CollisionMember,
    new_name: &str,
) {
    // move the images-map pin (if any) keyed by the config name to the new key, preserving
    // its pinned version; the map may legitimately lack the key if only the order names it
    if let Some(pipeline_image) = version.images.remove(old_name) {
        version.images.insert(new_name.to_string(), pipeline_image);
    }
    // a pin keyed by the member's manifest key moves too, but only when it pins this
    // member's version (another version under that key is a different image)
    if version
        .images
        .get(&member.manifest_key)
        .is_some_and(|pin| pin.version == member.version)
        && let Some(pipeline_image) = version.images.remove(&member.manifest_key)
    {
        version.images.insert(new_name.to_string(), pipeline_image);
    }
    if let Some(config) = &mut version.config {
        // deserialize the order into owned strings so the borrow of config is released
        // before we reassign config.order below; an unparsable order yields None and is
        // left untouched
        let rewritten: Option<Vec<Vec<String>>> =
            config.deserialize_image_order().ok().map(|order| {
                order
                    .into_iter()
                    .map(|stage| {
                        stage
                            .into_iter()
                            // swap the renamed image, leave every other entry as-is
                            .map(|image| {
                                if image == old_name {
                                    new_name.to_string()
                                } else {
                                    image.to_string()
                                }
                            })
                            .collect()
                    })
                    .collect()
            });
        // write the rewritten order back as JSON; a serialization failure leaves the
        // original order in place rather than corrupting it
        if let Some(new_order) = rewritten
            && let Ok(value) = serde_json::to_value(new_order)
        {
            config.order = value;
        }
    }
}

/// Truncate a name to at most `max` characters, dropping any trailing '-' the cut
/// leaves behind
///
/// # Arguments
///
/// * `name` - The name to truncate (expected to be ASCII)
/// * `max` - The maximum length to keep
fn truncate_name(name: &str, max: usize) -> String {
    // take at most `max` characters, then trim a dangling separator
    name.chars()
        .take(max)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

/// Map an arbitrary string onto the Thorium resource name rule: lowercase ASCII
/// letters, digits, and single '-' separators, with no leading or trailing '-',
/// truncated to [`prompt::RESOURCE_NAME_MAX`]
///
/// # Arguments
///
/// * `raw` - The string to sanitize
fn sanitize_name(raw: &str) -> String {
    // lowercase the input and turn every character outside [a-z0-9] into a separator
    let mapped: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    // collapse separator runs and drop leading/trailing separators
    let collapsed = mapped
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    // fit the result within the API's name length cap
    truncate_name(&collapsed, prompt::RESOURCE_NAME_MAX)
}

/// The sanitized `<name>-<version>` base a rename suggestion starts from, falling
/// back to the sanitized name (or `renamed`) when nothing valid is left
///
/// # Arguments
///
/// * `name` - The colliding resource's name
/// * `version` - The member's version label
fn suggestion_base(name: &str, version: &str) -> String {
    // prefer "<name>-<version>" mapped onto the name rule
    let base = sanitize_name(&format!("{name}-{version}"));
    if !base.is_empty() {
        return base;
    }
    // fall back to the bare name, and finally a fixed placeholder
    let base = sanitize_name(name);
    if base.is_empty() {
        "renamed".to_string()
    } else {
        base
    }
}

/// A valid name `taken` reports as free, appending `-2`, `-3`, … to `base` until one
/// is found
///
/// `taken` is the caller's freshness test (it decides what "already used" means).
/// `base` is truncated as needed so every candidate, suffix included, stays within
/// [`prompt::RESOURCE_NAME_MAX`]; the `(2..)` range is unbounded, so a free name is
/// always found.
///
/// # Arguments
///
/// * `base` - The preferred (already sanitized) name to return unchanged when it is free
/// * `taken` - Predicate reporting whether a candidate name is already used
fn unique_name(base: &str, mut taken: impl FnMut(&str) -> bool) -> String {
    // prefer the unsuffixed name when it is already free
    let first = truncate_name(base, prompt::RESOURCE_NAME_MAX);
    if !taken(&first) {
        return first;
    }
    // otherwise try "base-2", "base-3", … until one is free, shortening the base so the
    // suffix still fits; `taken` can only reject finitely many names, so this terminates
    let mut n: u64 = 2;
    loop {
        // build the next suffixed candidate within the length cap
        let suffix = format!("-{n}");
        let stem = truncate_name(base, prompt::RESOURCE_NAME_MAX.saturating_sub(suffix.len()));
        let candidate = format!("{stem}{suffix}");
        // return the first candidate nothing else uses
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Serialize an image config to JSON with object keys and set-valued arrays in a
/// canonical order, so two equal configs always produce equal values
///
/// # Arguments
///
/// * `config` - The image config to serialize
pub(crate) fn canonical_image_value(
    config: &ImageRequest,
) -> Result<serde_json::Value, serde_json::Error> {
    // serde_json objects are sorted maps, so only the set-valued arrays need sorting
    let mut value = serde_json::to_value(config)?;
    crate::utils::sort_set_fields(&mut value, crate::utils::IMAGE_SET_FIELDS);
    Ok(value)
}

/// A pipeline entry in a toolbox manifest: its versions keyed by version label
#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineManifest {
    /// A map of pipeline versions to their details
    #[serde(flatten)]
    pub versions: HashMap<String, PipelineVersion>,
}

/// Details for a specific pipeline version
#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineVersion {
    /// The tool directory (where this pipeline's `manifest.toml` lives), relative to the
    /// `toolbox.json`'s location. Lets `export` find where a pipeline already lives and update it in
    /// place; empty for older toolboxes that predate the field (the default layout is used then).
    #[serde(default)]
    pub dir: String,
    /// A description of the pipeline for the purpose of the toolbox, not for
    /// Thorium itself
    #[serde(default)]
    pub description: String,
    /// A map of image names to their info for the pipeline
    #[serde(default)]
    pub images: HashMap<String, PipelineImage>,
    /// URL to fetch the pipeline config from (alternative to inline config)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_from: Option<String>,
    /// The pipeline's Thorium configuration (inline or resolved from `config_from`)
    #[serde(default)]
    pub config: Option<PipelineRequest>,
}

/// A pipeline's reference to an image version it runs
#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineImage {
    /// The version of the image this pipeline expects
    pub version: String,
}

/// An image entry in a toolbox manifest: its versions keyed by version label
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageManifest {
    /// A map of image versions to their details
    #[serde(flatten)]
    pub versions: HashMap<String, ImageVersion>,
}

/// Details for a specific image version
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageVersion {
    /// The tool directory (where this image's `manifest.toml` and any bundled tarball live),
    /// relative to the `toolbox.json`'s location. Used to find a bundled image's tarball; empty
    /// for older toolboxes that predate the field, in which case the bundled-image lookup falls
    /// back to the default `images/<name>` layout.
    #[serde(default)]
    pub dir: String,
    /// The image's build path relative to the toolbox manifest's location
    #[serde(default)]
    pub build_path: String,
    /// URL to fetch the image config from (alternative to inline config)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_from: Option<String>,
    /// The image's Thorium configuration (inline or resolved from `config_from`)
    #[serde(default)]
    pub config: Option<ImageRequest>,
    /// URLs to fetch network policy definitions from (resolved into
    /// `network_policies` before import, like `config_from`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_policies_from: Vec<String>,
    /// The network policies this image references, bundled so an import can
    /// create them in the target instance when missing
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_policies: Vec<NetworkPolicyRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use thorium::models::{ImageRequest, PipelineRequest};

    /// An image version whose config carries a distinguishing `image` url, so two
    /// versions of the same `(group, name)` aren't byte-identical
    ///
    /// # Arguments
    ///
    /// * `group` - The group for the image config
    /// * `name` - The name for the image config
    /// * `distinct` - The image url that makes this config distinguishable
    fn image_version(group: &str, name: &str, distinct: &str) -> ImageVersion {
        // build the base request, then stamp a distinguishing image url so two versions of
        // the same (group, name) serialize differently and aren't treated as duplicates
        let mut config = ImageRequest::new(group, name);
        config.image = Some(distinct.to_string());
        ImageVersion {
            dir: String::new(),
            build_path: String::new(),
            config_from: None,
            config: Some(config),
            network_policies_from: Vec::new(),
            network_policies: Vec::new(),
        }
    }

    /// Build an image manifest with a single resolved `latest` version in `group`
    ///
    /// # Arguments
    ///
    /// * `group` - The group for the image config
    /// * `name` - The name for the image config
    fn image(group: &str, name: &str) -> ImageManifest {
        ImageManifest {
            versions: HashMap::from([("latest".to_string(), image_version(group, name, name))]),
        }
    }

    /// Build an image manifest with one entry per version label (each config distinct)
    ///
    /// # Arguments
    ///
    /// * `group` - The group for each image config
    /// * `name` - The name for each image config
    /// * `versions` - The version labels to build an entry for
    fn versioned_image(group: &str, name: &str, versions: &[&str]) -> ImageManifest {
        ImageManifest {
            // give each version a "name:version" image url so the variants are distinct
            versions: versions
                .iter()
                .map(|v| {
                    (
                        (*v).to_string(),
                        image_version(group, name, &format!("{name}:{v}")),
                    )
                })
                .collect(),
        }
    }

    /// Build an image manifest whose single `latest` version never resolved a config
    fn unresolved_image() -> ImageManifest {
        ImageManifest {
            versions: HashMap::from([(
                "latest".to_string(),
                ImageVersion {
                    dir: String::new(),
                    build_path: String::new(),
                    config_from: Some("https://example/cfg.json".to_string()),
                    config: None,
                    network_policies_from: Vec::new(),
                    network_policies: Vec::new(),
                },
            )]),
        }
    }

    /// Build a pipeline manifest with a single resolved `latest` version, whose
    /// `images` map pins each named image to version `latest`
    ///
    /// # Arguments
    ///
    /// * `group` - The group for the pipeline config
    /// * `name` - The name for the pipeline config
    /// * `order` - The pipeline's image order as a JSON value
    /// * `images` - The image names to pin to version `latest`
    fn pipeline(
        group: &str,
        name: &str,
        order: serde_json::Value,
        images: &[&str],
    ) -> PipelineManifest {
        // pin every named image to "latest" and defer to the pinned builder
        let pins: Vec<(&str, &str)> = images.iter().map(|i| (*i, "latest")).collect();
        pipeline_pinned(group, name, order, &pins)
    }

    /// Build a pipeline manifest pinning each named image to a specific version
    ///
    /// # Arguments
    ///
    /// * `group` - The group for the pipeline config
    /// * `name` - The name for the pipeline config
    /// * `order` - The pipeline's image order as a JSON value
    /// * `images` - The `(image, version)` pins for the pipeline's images map
    fn pipeline_pinned(
        group: &str,
        name: &str,
        order: serde_json::Value,
        images: &[(&str, &str)],
    ) -> PipelineManifest {
        // turn the (image, version) pairs into the pipeline's images map
        let image_map = images
            .iter()
            .map(|(image, version)| {
                (
                    (*image).to_string(),
                    PipelineImage {
                        version: (*version).to_string(),
                    },
                )
            })
            .collect();
        PipelineManifest {
            versions: HashMap::from([(
                "latest".to_string(),
                PipelineVersion {
                    dir: String::new(),
                    description: String::new(),
                    images: image_map,
                    config_from: None,
                    config: Some(PipelineRequest::new(group, name, order)),
                },
            )]),
        }
    }

    /// Assemble a toolbox manifest from the given images and pipelines
    ///
    /// # Arguments
    ///
    /// * `images` - The `(key, manifest)` image entries to include
    /// * `pipelines` - The `(key, manifest)` pipeline entries to include
    fn manifest(
        images: Vec<(&str, ImageManifest)>,
        pipelines: Vec<(&str, PipelineManifest)>,
    ) -> ToolboxManifest {
        // key each entry by its given name; the remaining fields are inert test defaults
        ToolboxManifest {
            name: "t".to_string(),
            registry: Some("r".to_string()),
            images: images
                .into_iter()
                .map(|(n, m)| (n.to_string(), m))
                .collect(),
            pipelines: pipelines
                .into_iter()
                .map(|(n, m)| (n.to_string(), m))
                .collect(),
            bundled_images: false,
            image_path_prefix: None,
        }
    }

    /// The deserialized image order of a pipeline's `latest` version, as owned strings
    ///
    /// # Arguments
    ///
    /// * `m` - The manifest to read the pipeline from
    /// * `pipeline_key` - The top-level key of the pipeline to read
    fn order_of(m: &ToolboxManifest, pipeline_key: &str) -> Vec<Vec<String>> {
        // reach into the "latest" version's resolved config, parse its order, and own the
        // borrowed entries so callers can compare against literal Vec<Vec<String>>
        m.pipelines[pipeline_key].versions["latest"]
            .config
            .as_ref()
            .unwrap()
            .deserialize_image_order()
            .unwrap()
            .into_iter()
            .map(|stage| stage.into_iter().map(String::from).collect())
            .collect()
    }

    // ─── structural / coherence validation ───────────────────────────────────

    /// A manifest whose pipelines reference present, correctly-grouped images
    /// survives both validation passes untouched
    #[test]
    fn keeps_valid_manifest() {
        let mut m = manifest(
            vec![("a", image("g", "a")), ("b", image("g", "b"))],
            vec![("p", pipeline("g", "p", json!(["a", "b"]), &["a", "b"]))],
        );
        assert!(m.validate_structural().pipelines.is_empty());
        assert!(m.validate_group_coherence().pipelines.is_empty());
        assert!(m.pipelines.contains_key("p"));
        assert_eq!(m.images.len(), 2);
    }

    /// Structural validation drops a pipeline that references an image absent
    /// from the manifest
    #[test]
    fn structural_drops_pipeline_referencing_missing_image() {
        let mut m = manifest(
            vec![("a", image("g", "a"))],
            vec![("p", pipeline("g", "p", json!(["a"]), &["a", "missing"]))],
        );
        let dropped = m.validate_structural();
        assert!(!m.pipelines.contains_key("p"));
        assert_eq!(dropped.pipelines.len(), 1);
        assert_eq!(dropped.pipelines[0].0, "g/p@latest");
        assert!(dropped.pipelines[0].1.iter().any(|r| r.contains("missing")));
    }

    /// Group-coherence validation drops a pipeline whose order references an
    /// image that exists but isn't in the pipeline's group
    #[test]
    fn coherence_drops_misgrouped_order_image() {
        // image 'a' lives in group 'other', but the pipeline imports into 'g'
        let mut m = manifest(
            vec![("a", image("other", "a"))],
            vec![("p", pipeline("g", "p", json!(["a"]), &["a"]))],
        );
        // structurally fine (the image exists in the manifest)...
        assert!(m.validate_structural().pipelines.is_empty());
        // ...but incoherent: 'a' isn't in the pipeline's group
        let dropped = m.validate_group_coherence();
        assert!(!m.pipelines.contains_key("p"));
        assert!(
            dropped.pipelines[0]
                .1
                .iter()
                .any(|r| r.contains("not in group 'g'"))
        );
    }

    /// A pipeline validates when its images map keys by the tool name while its
    /// order uses the image's config name
    #[test]
    fn keeps_pipeline_when_image_map_uses_tool_name() {
        // the image's manifest key (tool name) is 'sqlitediff' but its config name
        // is 'sqldiff'; the pipeline's images map keys by the tool name while its
        // order uses the config name — both must validate
        let mut m = manifest(
            vec![(
                "sqlitediff",
                ImageManifest {
                    versions: HashMap::from([(
                        "latest".to_string(),
                        image_version("g", "sqldiff", "url"),
                    )]),
                },
            )],
            vec![(
                "sqlitediff",
                pipeline_pinned(
                    "g",
                    "sqlitediff",
                    json!(["sqldiff"]),
                    &[("sqlitediff", "latest")],
                ),
            )],
        );
        assert!(m.validate_structural().pipelines.is_empty());
        assert!(m.validate_group_coherence().pipelines.is_empty());
        assert!(m.pipelines.contains_key("sqlitediff"));
    }

    /// A group override collapses a misgrouped image into the target group so the
    /// dependent pipeline then validates
    #[test]
    fn group_override_resolves_misgrouped_image() {
        let m = manifest(
            vec![("a", image("other", "a"))],
            vec![("p", pipeline("g", "p", json!(["a"]), &["a"]))],
        );
        let mut m = m.override_group("static");
        assert!(m.validate_structural().pipelines.is_empty());
        assert!(m.validate_group_coherence().pipelines.is_empty());
        assert!(m.pipelines.contains_key("p"));
    }

    /// Structural validation drops an unresolved image and any pipeline that
    /// depended on it
    #[test]
    fn structural_drops_unresolved_image_and_dependent_pipeline() {
        let mut m = manifest(
            vec![("a", unresolved_image())],
            vec![("p", pipeline("g", "p", json!(["a"]), &["a"]))],
        );
        let dropped = m.validate_structural();
        assert!(m.images.is_empty());
        assert_eq!(dropped.images.len(), 1);
        assert_eq!(dropped.images[0].0, "a@latest");
        assert!(!m.pipelines.contains_key("p"));
    }

    // ─── collision detection & resolution ────────────────────────────────────

    /// One image with two distinct version configs is detected as a single
    /// non-identical collision
    #[test]
    fn detects_multi_version_collision() {
        // one image with two distinct version configs collides on (g, exiftool)
        let m = manifest(
            vec![(
                "exiftool",
                versioned_image("g", "exiftool", &["latest", "1.2"]),
            )],
            vec![],
        );
        let sources = m.capture_source_groups();
        let collisions = m.detect_image_collisions(&sources).unwrap();
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].name, "exiftool");
        assert_eq!(collisions[0].members.len(), 2);
        assert!(!collisions[0].identical);
    }

    /// Renaming one version variant repoints only the pipeline pinned to that
    /// version, leaving the other variant under the original name
    #[test]
    fn rename_multi_version_cascades_by_pinned_version() {
        // pipeline pins exiftool@latest; renaming the latest variant repoints it
        let m = manifest(
            vec![(
                "exiftool",
                versioned_image("g", "exiftool", &["latest", "1.2"]),
            )],
            vec![(
                "p",
                pipeline_pinned("g", "p", json!(["exiftool"]), &[("exiftool", "latest")]),
            )],
        );
        let sources = m.capture_source_groups();
        let mut m = m;
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        // members sort to [1.2, latest]; keep the first, rename the second (latest)
        let latest = collision
            .members
            .iter()
            .find(|mem| mem.version == "latest")
            .unwrap()
            .clone();
        m.rename_image_member(&collision, &latest, "exiftool-latest", &sources)
            .unwrap();
        // both images now coexist
        assert!(m.images.contains_key("exiftool")); // the 1.2 variant kept the name
        assert!(m.images.contains_key("exiftool-latest"));
        // the pipeline that wanted latest was repointed
        assert_eq!(order_of(&m, "p"), vec![vec!["exiftool-latest".to_string()]]);
        assert!(
            m.pipelines["p"].versions["latest"]
                .images
                .contains_key("exiftool-latest")
        );
    }

    /// Renaming one of two same-named images merged from different source groups
    /// repoints only the pipeline from that source group
    #[test]
    fn rename_same_name_diff_group_cascades_by_source_group() {
        // two genuinely-different images named 'exiftool' in different groups,
        // merged by the override (distinct `image` urls so they aren't deduped)
        let m = manifest(
            vec![
                (
                    "exiftool",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("static", "exiftool", "url-static"),
                        )]),
                    },
                ),
                (
                    "exiftool-uur",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("dynamic", "exiftool", "url-uur"),
                        )]),
                    },
                ),
            ],
            vec![
                (
                    "p-static",
                    pipeline("static", "p-static", json!(["exiftool"]), &["exiftool"]),
                ),
                (
                    "p-uur",
                    pipeline("dynamic", "p-uur", json!(["exiftool"]), &["exiftool"]),
                ),
            ],
        );
        // capture BEFORE the override, then collapse groups
        let sources = m.capture_source_groups();
        let mut m = m.override_group("static");
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        assert_eq!(collision.members.len(), 2);
        assert!(!collision.identical);
        // canonical = first member by key ('exiftool', source static); rename the uur one
        let uur = collision
            .members
            .iter()
            .find(|mem| mem.source_group == "dynamic")
            .unwrap()
            .clone();
        let new_name = m.suggested_image_rename(&collision, &uur);
        m.rename_image_member(&collision, &uur, &new_name, &sources)
            .unwrap();
        // the static pipeline keeps 'exiftool'; the uur pipeline is repointed
        assert_eq!(order_of(&m, "p-static"), vec![vec!["exiftool".to_string()]]);
        assert_eq!(order_of(&m, "p-uur"), vec![vec![new_name.clone()]]);
    }

    /// Two entries with distinct manifest keys but the same config name + group
    /// are still detected as a collision
    #[test]
    fn detects_collision_by_config_name_not_manifest_key() {
        // distinct manifest keys, same config.name + group -> still a collision
        let m = manifest(
            vec![
                (
                    "a",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("g", "x", "url-a"),
                        )]),
                    },
                ),
                (
                    "b",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("g", "x", "url-b"),
                        )]),
                    },
                ),
            ],
            vec![],
        );
        let sources = m.capture_source_groups();
        let collisions = m.detect_image_collisions(&sources).unwrap();
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].name, "x");
        assert!(!collisions[0].identical);
    }

    /// Two byte-identical entries are detected as an identical collision and
    /// de-duped down to the first member
    #[test]
    fn dedupes_identical_duplicate() {
        // two manifest entries with identical configs -> pure duplicate
        let m = manifest(vec![("a", image("g", "x")), ("b", image("g", "x"))], vec![]);
        let sources = m.capture_source_groups();
        let mut m = m;
        let collisions = m.detect_image_collisions(&sources).unwrap();
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].identical);
        m.dedupe_image_collision(&collisions[0]);
        // only the first member (key 'a') survives
        assert_eq!(m.images.len(), 1);
        assert!(m.images.contains_key("a"));
    }

    /// Skipping a collision removes every entry of that identity and the pipelines
    /// depending on it, while leaving unrelated pipelines intact
    #[test]
    fn skip_removes_identity_and_dependents() {
        let m = manifest(
            vec![
                (
                    "a",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("g", "x", "url-a"),
                        )]),
                    },
                ),
                (
                    "b",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("g", "x", "url-b"),
                        )]),
                    },
                ),
            ],
            vec![
                ("dep", pipeline("g", "dep", json!(["x"]), &["x"])),
                ("free", pipeline("g", "free", json!(["other"]), &["other"])),
            ],
        );
        let mut m = m;
        let dropped = m.remove_image_identity_and_dependents("g", "x");
        // both colliding image entries are gone...
        assert!(!m.image_names_in_group("g").contains("x"));
        // ...the pipeline that used it is dropped, the one that didn't is kept
        assert_eq!(dropped, vec!["g/dep@latest".to_string()]);
        assert!(!m.pipelines.contains_key("dep"));
        assert!(m.pipelines.contains_key("free"));
    }

    /// Removing one `(group, name)` image identity must be scoped to that group: a
    /// same-named image in a different group, and a pipeline in that other group that
    /// depends on it, must both survive (image references are bare names resolved within
    /// the pipeline's own group, so a cross-group same-named image is unrelated)
    #[test]
    fn skip_removal_is_scoped_to_the_image_group() {
        let mut m = manifest(
            vec![("x-g1", image("g1", "x")), ("x-g2", image("g2", "x"))],
            // a pipeline in g2 depending on the g2 copy of x
            vec![("p2", pipeline("g2", "p2", json!(["x"]), &["x"]))],
        );
        let dropped = m.remove_image_identity_and_dependents("g1", "x");
        // the g1 identity is removed...
        assert!(!m.image_names_in_group("g1").contains("x"));
        // ...while the g2 copy and its dependent pipeline are left untouched
        assert!(m.image_names_in_group("g2").contains("x"));
        assert!(dropped.is_empty());
        assert!(m.pipelines.contains_key("p2"));
    }

    /// When the natural rename candidates are exhausted, a numeric suffix is
    /// appended to keep the suggested name unique
    #[test]
    fn suggested_rename_appends_numeric_suffix() {
        // three same-version variants force a fallback suffix on the third name
        let m = manifest(
            vec![
                (
                    "a",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("g", "x", "url-a"),
                        )]),
                    },
                ),
                (
                    "b",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("g", "x", "url-b"),
                        )]),
                    },
                ),
                (
                    "c",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("g", "x", "url-c"),
                        )]),
                    },
                ),
            ],
            vec![],
        );
        let sources = m.capture_source_groups();
        let mut m = m;
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        // rename the 2nd and 3rd members (keep the 1st canonical)
        let m1 = collision.members[1].clone();
        let n1 = m.suggested_image_rename(&collision, &m1);
        assert_eq!(n1, "x-latest");
        m.rename_image_member(&collision, &m1, &n1, &sources)
            .unwrap();
        let m2 = collision.members[2].clone();
        let n2 = m.suggested_image_rename(&collision, &m2);
        assert_eq!(n2, "x-latest-2");
    }

    /// Two image entries whose network-policy sets hold the same elements are an
    /// identical duplicate regardless of each set's iteration order
    #[test]
    fn identical_check_ignores_set_order() {
        // build two configs with the same policies inserted in opposite orders, repeated
        // across many sets so differing hash orders are all but certain to appear
        let names: Vec<String> = (0..16).map(|n| format!("p{n}")).collect();
        let mut a = image_version("g", "x", "url");
        let mut b = image_version("g", "x", "url");
        a.config.as_mut().unwrap().network_policies = names.iter().cloned().collect();
        b.config.as_mut().unwrap().network_policies = names.iter().rev().cloned().collect();
        let m = manifest(
            vec![
                (
                    "a",
                    ImageManifest {
                        versions: HashMap::from([("latest".to_string(), a)]),
                    },
                ),
                (
                    "b",
                    ImageManifest {
                        versions: HashMap::from([("latest".to_string(), b)]),
                    },
                ),
            ],
            vec![],
        );
        let sources = m.capture_source_groups();
        let collisions = m.detect_image_collisions(&sources).unwrap();
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].identical);
    }

    /// The suggested rename is sanitized to a valid name even when the version label
    /// has dots, uppercase letters, or would push the name past the length cap
    #[test]
    fn suggested_rename_is_a_valid_name() {
        let m = manifest(
            vec![(
                "detect-it-easy",
                versioned_image("g", "detect-it-easy", &["3.09", "V3.10-RC.Long-Label"]),
            )],
            vec![],
        );
        let sources = m.capture_source_groups();
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        for member in &collision.members {
            let name = m.suggested_image_rename(&collision, member);
            assert!(
                prompt::validate_name(&name, prompt::RESOURCE_NAME_MAX).is_ok(),
                "'{name}' should be a valid name"
            );
        }
        let old = collision
            .members
            .iter()
            .find(|mem| mem.version == "3.09")
            .unwrap();
        assert_eq!(
            m.suggested_image_rename(&collision, old),
            "detect-it-easy-3-09"
        );
    }

    /// A numeric suffix still fits within the name length cap
    #[test]
    fn unique_name_suffix_respects_length_cap() {
        let base = "a".repeat(prompt::RESOURCE_NAME_MAX);
        let name = unique_name(&base, |candidate| candidate == base);
        assert_eq!(name.len(), prompt::RESOURCE_NAME_MAX);
        assert!(name.ends_with("-2"));
    }

    /// A rename onto another entry's manifest key (whose config name differs) is
    /// rejected instead of overwriting that entry's version, and the suggestion skips it
    #[test]
    fn rename_rejects_existing_manifest_key() {
        let m = manifest(
            vec![
                (
                    "clamav",
                    ImageManifest {
                        versions: HashMap::from([(
                            "1".to_string(),
                            image_version("a", "clamav", "url-a"),
                        )]),
                    },
                ),
                (
                    "clamav-1",
                    ImageManifest {
                        versions: HashMap::from([(
                            "1".to_string(),
                            image_version("b", "clamav", "url-b"),
                        )]),
                    },
                ),
            ],
            vec![],
        );
        let sources = m.capture_source_groups();
        let mut m = m.override_group("shared");
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        // keep 'clamav-1' and rename the 'clamav' entry
        let member = collision
            .members
            .iter()
            .find(|mem| mem.manifest_key == "clamav")
            .unwrap()
            .clone();
        // the suggestion avoids the existing 'clamav-1' key
        assert_eq!(m.suggested_image_rename(&collision, &member), "clamav-1-2");
        // an explicit rename onto that key is refused and changes nothing
        assert!(
            m.rename_image_member(&collision, &member, "clamav-1", &sources)
                .is_err()
        );
        assert_eq!(m.images.len(), 2);
        assert_eq!(
            m.images["clamav-1"].versions["1"]
                .config
                .as_ref()
                .unwrap()
                .image
                .as_deref(),
            Some("url-b")
        );
    }

    /// A pipeline that pins the image by its manifest key follows the renamed variant
    #[test]
    fn rename_repoints_pipeline_pinned_by_manifest_key() {
        let m = manifest(
            vec![
                (
                    "sqlitediff",
                    ImageManifest {
                        versions: HashMap::from([(
                            "1.0".to_string(),
                            image_version("a", "sqldiff", "url-1"),
                        )]),
                    },
                ),
                (
                    "sqldiff",
                    ImageManifest {
                        versions: HashMap::from([(
                            "2.0".to_string(),
                            image_version("b", "sqldiff", "url-2"),
                        )]),
                    },
                ),
            ],
            vec![(
                "pa",
                pipeline_pinned("a", "pa", json!(["sqldiff"]), &[("sqlitediff", "1.0")]),
            )],
        );
        let sources = m.capture_source_groups();
        let mut m = m.override_group("shared");
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        let old = collision
            .members
            .iter()
            .find(|mem| mem.version == "1.0")
            .unwrap()
            .clone();
        let warnings = m
            .rename_image_member(&collision, &old, "sqldiff-v1", &sources)
            .unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(order_of(&m, "pa"), vec![vec!["sqldiff-v1".to_string()]]);
        assert!(
            m.pipelines["pa"].versions["latest"]
                .images
                .contains_key("sqldiff-v1")
        );
    }

    /// When neither pin nor source group tells the variants apart, dependents stay on
    /// the variant keeping the name and a warning is returned
    #[test]
    fn ambiguous_rename_leaves_dependents_and_warns() {
        let m = manifest(
            vec![
                (
                    "scan-a",
                    ImageManifest {
                        versions: HashMap::from([(
                            "1".to_string(),
                            image_version("g", "scan", "url-a"),
                        )]),
                    },
                ),
                (
                    "scan-b",
                    ImageManifest {
                        versions: HashMap::from([(
                            "1".to_string(),
                            image_version("g", "scan", "url-b"),
                        )]),
                    },
                ),
            ],
            vec![(
                "p1",
                pipeline_pinned("g", "p1", json!(["scan"]), &[("scan", "1")]),
            )],
        );
        let sources = m.capture_source_groups();
        let mut m = m;
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        let b = collision
            .members
            .iter()
            .find(|mem| mem.manifest_key == "scan-b")
            .unwrap()
            .clone();
        let warnings = m
            .rename_image_member(&collision, &b, "scan-b2", &sources)
            .unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(order_of(&m, "p1"), vec![vec!["scan".to_string()]]);
    }

    /// Renaming a variant repoints another image's result dependency matched by
    /// source group
    #[test]
    fn rename_repoints_image_dependencies() {
        let mut dependent = image_version("dynamic", "y", "url-y");
        dependent
            .config
            .as_mut()
            .unwrap()
            .dependencies
            .results
            .images = vec!["x".to_string()];
        let m = manifest(
            vec![
                ("x", image("static", "x")),
                (
                    "x-dyn",
                    ImageManifest {
                        versions: HashMap::from([(
                            "latest".to_string(),
                            image_version("dynamic", "x", "url-dyn"),
                        )]),
                    },
                ),
                (
                    "y",
                    ImageManifest {
                        versions: HashMap::from([("latest".to_string(), dependent)]),
                    },
                ),
            ],
            vec![],
        );
        let sources = m.capture_source_groups();
        let mut m = m.override_group("static");
        let collision = m.detect_image_collisions(&sources).unwrap().remove(0);
        let dynamic = collision
            .members
            .iter()
            .find(|mem| mem.source_group == "dynamic")
            .unwrap()
            .clone();
        m.rename_image_member(&collision, &dynamic, "x-dynamic", &sources)
            .unwrap();
        let deps = &m.images["y"].versions["latest"]
            .config
            .as_ref()
            .unwrap()
            .dependencies;
        assert_eq!(deps.results.images, vec!["x-dynamic".to_string()]);
    }

    /// Dropped-item reports come back sorted by label
    #[test]
    fn dropped_items_are_sorted() {
        let mut m = manifest(
            vec![],
            (0..8)
                .map(|n| {
                    (
                        ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7"][n],
                        pipeline("g", "p", json!(["missing"]), &["missing"]),
                    )
                })
                .collect(),
        );
        let dropped = m.validate_structural();
        let labels: Vec<&String> = dropped.pipelines.iter().map(|(label, _)| label).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        assert_eq!(labels.len(), 8);
        assert_eq!(labels, sorted);
    }
}
