//! Support for searching an object in Thorium with grep

use super::TagSupport;
use crate::models::KeySupport;

// dependencies required for api
cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use crate::conf::Conf;
        use crate::models::backends::db::GrepCursor;
        use crate::models::backends::grep::{GrepDocument, GrepSearcher};
        use crate::models::{GrepParams, GrepSource, OutputMap, ResultGetParams, User};
        use crate::utils::{ApiError, Shared};
    }
}

/// The trait for grep based search support in Thorium
///
/// Implementing this is all a new type of data needs to be greppable. Result files are
/// handled generically for anything that can hand back an [`OutputMap`], so an
/// implementor only has to expose its own documents.
pub trait GrepSupport: TagSupport + KeySupport + Sized {
    /// Get a single item to grep, authorizing that this user can see it
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is grepping this item
    /// * `key` - The key of the item to get
    /// * `shared` - Shared Thorium objects
    #[cfg(feature = "api")]
    #[allow(async_fn_in_trait)]
    async fn get_greppable(user: &User, key: &str, shared: &Shared) -> Result<Self, ApiError>;

    /// List the next page of keys for this user to grep
    ///
    /// This resumes the inner list cursor held by our grep cursor so that paging over
    /// items comes for free from the existing list cursors.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is grepping these items
    /// * `cursor` - The grep cursor to list the next page of keys for
    /// * `shared` - Shared Thorium objects
    #[cfg(feature = "api")]
    #[allow(async_fn_in_trait)]
    async fn list_greppable(
        user: &User,
        cursor: &mut GrepCursor,
        shared: &Shared,
    ) -> Result<Vec<String>, ApiError>;

    /// Get the results to grep for this item, authorizing the groups we read from
    ///
    /// Types that have no results at all should just return an empty [`OutputMap`], which
    /// causes the [`GrepSource::Results`] and [`GrepSource::ResultFiles`] sources to be
    /// skipped.
    ///
    /// # Arguments
    ///
    /// * `key` - The key of this item
    /// * `user` - The user that is grepping this items results
    /// * `params` - The params to get this items results with
    /// * `shared` - Shared Thorium objects
    #[cfg(feature = "api")]
    #[allow(async_fn_in_trait)]
    async fn greppable_results(
        &self,
        key: &str,
        user: &User,
        params: ResultGetParams,
        shared: &Shared,
    ) -> Result<OutputMap, ApiError>;

    /// Get the key for this item
    fn grep_key(&self) -> String;

    /// Get the earliest timestamp an item of this type can exist at
    ///
    /// This bounds how far back a corpus grep searches when no end timestamp is
    /// given, matching this types list routes.
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config to read the earliest timestamp from
    #[cfg(feature = "api")]
    fn grep_earliest(conf: &Conf) -> i64;

    /// Queue this items own documents to be searched
    ///
    /// This covers everything that is not a result or a result file, so submission info,
    /// comments, tags, and any type specific metadata.
    ///
    /// # Arguments
    ///
    /// * `searcher` - The searcher to queue documents in
    /// * `params` - The params for this grep query
    #[cfg(feature = "api")]
    fn queue_documents(&self, searcher: &mut GrepSearcher, params: &GrepParams);
}

// the api only implementations of grep support
cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use std::collections::HashMap;
        use strum::IntoEnumIterator;
        use uuid::Uuid;

        use crate::models::backends::db;
        use crate::models::{
            Entity, EntityKinds, EntityListParams, EntityMetadata, FileListParams, Repo,
            RepoListParams, Sample, TagMap,
        };
        use crate::serialize;

        /// Queue a tag map to be searched
        ///
        /// # Arguments
        ///
        /// * `searcher` - The searcher to queue documents in
        /// * `params` - The params for this grep query
        /// * `tags` - The tags to queue
        fn queue_tags(searcher: &mut GrepSearcher, params: &GrepParams, tags: &TagMap) {
            // skip our tags if we aren't searching them
            if !params.searches(GrepSource::Tags) {
                return;
            }
            // queue each tag key/value pair as its own document
            for (key, values) in tags {
                for (value, groups) in values {
                    // skip tag values in groups this query is not searching
                    if !params.groups_allow(groups) {
                        continue;
                    }
                    // queue this tags value to be searched
                    let doc = GrepDocument::Tag {
                        key: key.clone(),
                        value: value.clone(),
                    };
                    searcher.add_memory(doc, value.clone());
                }
            }
        }

        /// Queue a single optional submission field to be searched
        ///
        /// # Arguments
        ///
        /// * `searcher` - The searcher to queue documents in
        /// * `id` - The id of the submission this field comes from
        /// * `field` - The name of the field to queue
        /// * `value` - The value of the field to queue if one is set
        fn queue_submission_field(
            searcher: &mut GrepSearcher,
            id: Uuid,
            field: &str,
            value: Option<&String>,
        ) {
            // only queue this field if it has a value
            if let Some(value) = value {
                // queue this submission field to be searched
                let doc = GrepDocument::Submission {
                    id,
                    field: field.to_owned(),
                };
                searcher.add_memory(doc, value.clone());
            }
        }

        /// Queue a single metadata field to be searched
        ///
        /// # Arguments
        ///
        /// * `searcher` - The searcher to queue documents in
        /// * `field` - The name of the field to queue
        /// * `value` - The value of the field to queue if one is set
        fn queue_metadata_field(searcher: &mut GrepSearcher, field: &str, value: Option<&String>) {
            // only queue this field if it has a value
            if let Some(value) = value {
                // queue this metadata field to be searched
                let doc = GrepDocument::Metadata {
                    field: field.to_owned(),
                };
                searcher.add_memory(doc, value.clone());
            }
        }

        impl GrepSupport for Sample {
            /// Get a single sample to grep, authorizing that this user can see it
            ///
            /// # Arguments
            ///
            /// * `user` - The user that is grepping this sample
            /// * `key` - The sha256 of the sample to get
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Sample>::get_greppable", skip(user, shared), err(Debug))]
            async fn get_greppable(user: &User, key: &str, shared: &Shared) -> Result<Self, ApiError> {
                // get this sample, which authorizes that this user can see it
                Sample::get(user, key, shared).await
            }

            /// List the next page of sha256s for this user to grep
            ///
            /// # Arguments
            ///
            /// * `user` - The user that is grepping these samples
            /// * `cursor` - The grep cursor to list the next page of sha256s for
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Sample>::list_greppable", skip_all, err(Debug))]
            async fn list_greppable(
                user: &User,
                cursor: &mut GrepCursor,
                shared: &Shared,
            ) -> Result<Vec<String>, ApiError> {
                // build the list params for our inner cursor
                let mut params = FileListParams {
                    groups: cursor.retain.groups.clone(),
                    start: cursor.retain.start,
                    end: Some(cursor.retain.end),
                    tags: HashMap::default(),
                    cursor: None,
                    limit: cursor.limit,
                    tags_case_insensitive: false,
                };
                // reauthorize our groups every page so revoked access stops a cursor
                user.authorize_groups(&mut params.groups, shared).await?;
                // list the next page of samples, resuming our embedded inner cursor
                let listed =
                    db::files::list_greppable(user, params, cursor.retain.inner.take(), shared)
                        .await?;
                // keep the inner cursors state so the next page can resume it
                cursor.retain.inner = if listed.exhausted() {
                    None
                } else {
                    Some(serialize!(&listed.retain))
                };
                // downselect to just the sha256s for these samples
                Ok(listed.data.into_iter().map(|line| line.sha256).collect())
            }

            /// Get the results to grep for this sample
            ///
            /// # Arguments
            ///
            /// * `key` - The sha256 of this sample
            /// * `user` - The user that is grepping this samples results
            /// * `params` - The params to get this samples results with
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Sample>::greppable_results", skip_all, err(Debug))]
            async fn greppable_results(
                &self,
                key: &str,
                user: &User,
                params: ResultGetParams,
                shared: &Shared,
            ) -> Result<OutputMap, ApiError> {
                // get this samples results, which authorizes the groups we read from
                OutputMap::get::<Sample>(key, self, user, params, shared).await
            }

            /// Get the sha256 for this sample
            fn grep_key(&self) -> String {
                self.sha256.clone()
            }

            /// Get the earliest timestamp a sample can exist at
            ///
            /// # Arguments
            ///
            /// * `conf` - The Thorium config to read the earliest timestamp from
            fn grep_earliest(conf: &Conf) -> i64 {
                conf.thorium.files.earliest
            }

            /// Queue this samples submissions, comments, and tags to be searched
            ///
            /// # Arguments
            ///
            /// * `searcher` - The searcher to queue documents in
            /// * `params` - The params for this grep query
            fn queue_documents(&self, searcher: &mut GrepSearcher, params: &GrepParams) {
                // queue our submission info if we are searching it
                if params.searches(GrepSource::Submissions) {
                    // queue each submissions greppable fields
                    for sub in &self.submissions {
                        // skip submissions in groups this query is not searching
                        if !params.groups_allow(&sub.groups) {
                            continue;
                        }
                        // queue this submissions name and description
                        queue_submission_field(searcher, sub.id, "name", sub.name.as_ref());
                        queue_submission_field(
                            searcher,
                            sub.id,
                            "description",
                            sub.description.as_ref(),
                        );
                        // serialize this submissions origin so we can grep all of its fields at
                        // once; the stored form is used so this document is byte stable across
                        // pages even if its parent is deleted between them
                        if let Ok(origin) = serde_json::to_string(&sub.origin.as_stored()) {
                            let doc = GrepDocument::Submission {
                                id: sub.id,
                                field: "origin".to_owned(),
                            };
                            searcher.add_memory(doc, origin);
                        }
                    }
                }
                // queue our comments if we are searching them
                if params.searches(GrepSource::Comments) {
                    // queue each comments body
                    for comment in &self.comments {
                        // skip comments in groups this query is not searching
                        if !params.groups_allow(&comment.groups) {
                            continue;
                        }
                        let doc = GrepDocument::Comment { id: comment.id };
                        searcher.add_memory(doc, comment.comment.clone());
                    }
                }
                // queue our tags if we are searching them
                queue_tags(searcher, params, &self.tags);
            }
        }

        impl GrepSupport for Repo {
            /// Get a single repo to grep, authorizing that this user can see it
            ///
            /// # Arguments
            ///
            /// * `user` - The user that is grepping this repo
            /// * `key` - The url of the repo to get
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Repo>::get_greppable", skip(user, shared), err(Debug))]
            async fn get_greppable(user: &User, key: &str, shared: &Shared) -> Result<Self, ApiError> {
                // get this repo, which authorizes that this user can see it
                Repo::get(user, key, shared).await
            }

            /// List the next page of repo urls for this user to grep
            ///
            /// # Arguments
            ///
            /// * `user` - The user that is grepping these repos
            /// * `cursor` - The grep cursor to list the next page of repo urls for
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Repo>::list_greppable", skip_all, err(Debug))]
            async fn list_greppable(
                user: &User,
                cursor: &mut GrepCursor,
                shared: &Shared,
            ) -> Result<Vec<String>, ApiError> {
                // build the list params for our inner cursor
                let mut params = RepoListParams {
                    groups: cursor.retain.groups.clone(),
                    start: cursor.retain.start,
                    end: Some(cursor.retain.end),
                    tags: HashMap::default(),
                    cursor: None,
                    limit: cursor.limit,
                    tags_case_insensitive: false,
                };
                // reauthorize our groups every page so revoked access stops a cursor
                user.authorize_groups(&mut params.groups, shared).await?;
                // list the next page of repos, resuming our embedded inner cursor
                let listed =
                    db::repos::list_greppable(user, params, cursor.retain.inner.take(), shared)
                        .await?;
                // keep the inner cursors state so the next page can resume it
                cursor.retain.inner = if listed.exhausted() {
                    None
                } else {
                    Some(serialize!(&listed.retain))
                };
                // downselect to just the urls for these repos
                Ok(listed.data.into_iter().map(|line| line.url).collect())
            }

            /// Get the results to grep for this repo
            ///
            /// # Arguments
            ///
            /// * `key` - The url of this repo
            /// * `user` - The user that is grepping this repos results
            /// * `params` - The params to get this repos results with
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Repo>::greppable_results", skip_all, err(Debug))]
            async fn greppable_results(
                &self,
                key: &str,
                user: &User,
                params: ResultGetParams,
                shared: &Shared,
            ) -> Result<OutputMap, ApiError> {
                // get this repos results, which authorizes the groups we read from
                OutputMap::get::<Repo>(key, self, user, params, shared).await
            }

            /// Get the url for this repo
            fn grep_key(&self) -> String {
                self.url.clone()
            }

            /// Get the earliest timestamp a repo can exist at
            ///
            /// # Arguments
            ///
            /// * `conf` - The Thorium config to read the earliest timestamp from
            fn grep_earliest(conf: &Conf) -> i64 {
                conf.thorium.repos.earliest
            }

            /// Queue this repos submissions and tags to be searched
            ///
            /// Repos have no comments in Thorium today so that source is always empty
            /// for them.
            ///
            /// # Arguments
            ///
            /// * `searcher` - The searcher to queue documents in
            /// * `params` - The params for this grep query
            fn queue_documents(&self, searcher: &mut GrepSearcher, params: &GrepParams) {
                // queue our submission info if we are searching it
                if params.searches(GrepSource::Submissions) {
                    // queue each submissions greppable fields
                    for sub in &self.submissions {
                        // skip submissions in groups this query is not searching
                        if !params.groups_allow(&sub.groups) {
                            continue;
                        }
                        // queue this submissions creator
                        queue_submission_field(searcher, sub.id, "creator", Some(&sub.creator));
                        // serialize this submissions scheme so we can grep all of its fields at once
                        if let Ok(scheme) = serde_json::to_string(&sub.scheme) {
                            let doc = GrepDocument::Submission {
                                id: sub.id,
                                field: "scheme".to_owned(),
                            };
                            searcher.add_memory(doc, scheme);
                        }
                    }
                }
                // queue our tags if we are searching them
                queue_tags(searcher, params, &self.tags);
            }
        }

        impl GrepSupport for Entity {
            /// Get a single entity to grep, authorizing that this user can see it
            ///
            /// # Arguments
            ///
            /// * `user` - The user that is grepping this entity
            /// * `key` - The id of the entity to get
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Entity>::get_greppable", skip(user, shared), err(Debug))]
            async fn get_greppable(user: &User, key: &str, shared: &Shared) -> Result<Self, ApiError> {
                // try to parse this key as an entity id
                let id = match Uuid::parse_str(key) {
                    Ok(id) => id,
                    Err(_) => return crate::bad!(format!("{key} is not a valid entity id")),
                };
                // get this entity, which authorizes that this user can see it
                Entity::get(user, id, shared).await
            }

            /// List the next page of entity ids for this user to grep
            ///
            /// # Arguments
            ///
            /// * `user` - The user that is grepping these entities
            /// * `cursor` - The grep cursor to list the next page of entity ids for
            /// * `shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Entity>::list_greppable", skip_all, err(Debug))]
            async fn list_greppable(
                user: &User,
                cursor: &mut GrepCursor,
                shared: &Shared,
            ) -> Result<Vec<String>, ApiError> {
                // build the list params for our inner cursor
                let mut params = EntityListParams {
                    groups: cursor.retain.groups.clone(),
                    start: cursor.retain.start,
                    end: Some(cursor.retain.end),
                    tags: HashMap::default(),
                    cursor: None,
                    limit: cursor.limit,
                    // grep every kind of entity since a pattern is not kind specific
                    kinds: EntityKinds::iter().collect(),
                };
                // reauthorize our groups every page so revoked access stops a cursor
                user.authorize_groups(&mut params.groups, shared).await?;
                // list the next page of entities, resuming our embedded inner cursor
                let listed =
                    db::entities::list_greppable(user, params, cursor.retain.inner.take(), shared)
                        .await?;
                // keep the inner cursors state so the next page can resume it
                cursor.retain.inner = if listed.exhausted() {
                    None
                } else {
                    Some(serialize!(&listed.retain))
                };
                // downselect to just the ids for these entities
                Ok(listed
                    .data
                    .into_iter()
                    .map(|line| line.id.to_string())
                    .collect())
            }

            /// Entities have no results in Thorium today so there is nothing to grep
            ///
            /// # Arguments
            ///
            /// * `_key` - The id of this entity
            /// * `_user` - The user that is grepping this entities results
            /// * `_params` - The params to get this entities results with
            /// * `_shared` - Shared Thorium objects
            #[tracing::instrument(name = "GrepSupport<Entity>::greppable_results", skip_all, err(Debug))]
            async fn greppable_results(
                &self,
                _key: &str,
                _user: &User,
                _params: ResultGetParams,
                _shared: &Shared,
            ) -> Result<OutputMap, ApiError> {
                Ok(OutputMap::default())
            }

            /// Get the id for this entity
            fn grep_key(&self) -> String {
                self.id.to_string()
            }

            /// Get the earliest timestamp an entity can exist at
            ///
            /// # Arguments
            ///
            /// * `conf` - The Thorium config to read the earliest timestamp from
            fn grep_earliest(conf: &Conf) -> i64 {
                conf.thorium.entities.earliest
            }

            /// Queue this entities metadata and tags to be searched
            ///
            /// Entities have no submissions or comments in Thorium today so those sources
            /// are always empty for them.
            ///
            /// # Arguments
            ///
            /// * `searcher` - The searcher to queue documents in
            /// * `params` - The params for this grep query
            fn queue_documents(&self, searcher: &mut GrepSearcher, params: &GrepParams) {
                // queue our metadata if we are searching it; metadata is entity level
                // rather then per group, so it is gated by this entities own groups
                if params.searches(GrepSource::Metadata) && params.groups_allow(&self.groups) {
                    // queue our name and description
                    queue_metadata_field(searcher, "name", Some(&self.name));
                    queue_metadata_field(searcher, "description", self.description.as_ref());
                    // queue the greppable text for our kind specific metadata
                    match &self.metadata {
                        // sigma rules have their raw unparsed rule
                        EntityMetadata::SigmaRule(sigma) => {
                            queue_metadata_field(searcher, "rule", Some(&sigma.rule));
                        }
                        // decompiled functions have their decompiled source
                        EntityMetadata::DecompiledFunction(decompiled) => {
                            queue_metadata_field(searcher, "content", Some(&decompiled.content));
                        }
                        // flags have their reasoning and the characteristic they flagged
                        EntityMetadata::Flag(flag) => {
                            queue_metadata_field(searcher, "reasoning", Some(&flag.reasoning));
                            queue_metadata_field(searcher, "content", flag.content.as_ref());
                        }
                        // windows processes have their image path and command line
                        EntityMetadata::WindowsProcess(process) => {
                            queue_metadata_field(searcher, "image_path", process.image_path.as_ref());
                            queue_metadata_field(searcher, "command", process.command.as_ref());
                        }
                        // incidents have their cover term
                        EntityMetadata::Incident(incident) => {
                            queue_metadata_field(
                                searcher,
                                "cover_term",
                                incident.cover_term.as_ref(),
                            );
                        }
                        // all other entity kinds have no greppable free form text
                        _ => (),
                    }
                }
                // queue our tags if we are searching them
                queue_tags(searcher, params, &self.tags);
            }
        }
    }
}
