//! The associations for objects in Thorium
//!
//! Associations are directional relationships between two objects in Thorium.

use chrono::prelude::*;
use std::str::FromStr;
use uuid::Uuid;

use crate::models::{Directionality, EntityKinds};

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum AssociationTarget {
    /// This assocation is associated with another entity
    Entity { id: Uuid, name: String },
    /// This association is associated with a file
    File(String),
    /// This association is associated with a repo
    Repo(String),
}

/// The different possible associations
///
/// Archived backups store each variant as its position in this list, so variants are
/// append-only: never reorder, remove, or insert one; a new variant goes at the end. The
/// golden-bytes test of archived enums in thoradm's offline suite fails if the order changes.
#[derive(
    Debug,
    Serialize,
    Deserialize,
    Copy,
    Clone,
    Eq,
    PartialEq,
    Hash,
    strum::EnumString,
    strum::EnumIter,
    clap::ValueEnum,
)]
#[value(rename_all = "verbatim")]
#[cfg_attr(
    feature = "rkyv-support",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "rkyv-support",
    archive_attr(derive(Debug, bytecheck::CheckBytes))
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreAsStr))]
pub enum AssociationKind {
    /// This file is associated with something else
    FileFor,
    /// This is documentation for something else
    DocumentationFor,
    /// This file or repo is or contains firmware for a device
    FirmwareFor,
    /// This file/repo/entity is associated with something else
    AssociatedWith,
    /// This was developed or created by
    DevelopedBy,
    /// This contains a CVE
    ContainsCVE,
    /// This contains a CWE
    ContainsCWE,
    /// This is based in specific countries
    BasedIn,
    /// This person was or is employed by
    EmployedBy,
    /// This is the parent company of another company
    ParentCompanyOf,
    /// This is used by a specific person or group
    UsedBy,
    /// This was used in a specific campaign or engagement
    UsedIn,
    /// This campaign was performed by
    PerformedBy,
    /// This filesystem was extracted/carved from
    FileSystemIn,
    /// This is a folder within a filesystem or another folder
    FolderIn,
    /// This is a file in a folder in a filesytem
    FileIn,
    /// A Process tree in or from something
    ProcessTreeIn,
    /// A Process in a process tree or a child process
    ChildProcess,
    /// Opens or receives data from a network connection
    HasNetworkConnection,
    /// A PE section within a file/binary
    SectionIn,
    /// A library imported by a file/binary
    ImportIn,
    /// A flag on some data in Thorium
    FlagFor,
    /// This entity was created by
    CreatedBy,
    /// A Sigma rule hit
    SigmaRuleHit,
    /// This was involved in something
    InvolvedIn,
    /// This tool instance was published from this tool
    PublishedFrom,
    /// This model endpoint authenticates with this model credential
    AuthenticatesWith,
    /// This model profile calls a model on this model endpoint
    Serves,
    /// This workflow catalog orchestrates this agent
    Orchestrates,
    /// This agent can delegate to this other agent
    DelegatesTo,
    /// This agent calls this tool or toolset (Tool or `ToolSet` target = latest, `ToolInstance` or
    /// `ToolSetInstance` target = pinned)
    Calls,
    /// This agent runs in this harness (Tool target = latest, `ToolInstance` target = pinned)
    RunsIn,
    /// This workflow instance or toolset instance was built from this workflow catalog or toolset
    BuiltFrom,
    /// This toolset includes this tool (Tool target = latest, `ToolInstance` target = pinned)
    Includes,
    /// This harness tool instance provides this native agentic tool instance
    Provides,
}

impl std::fmt::Display for AssociationKind {
    /// Cleanly print an association kind
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // get the str for our enum variant
        let variant_str = self.as_str();
        // write this variant to our formatter
        write!(f, "{variant_str}")
    }
}

impl AssociationKind {
    /// Check that an edge of this kind keeps its target's groups a superset of its source's
    ///
    /// Returns the source groups the target is missing, in the source's order, for kinds whose
    /// edges must be contained; every other kind accepts any groups.
    ///
    /// # Arguments
    ///
    /// * `source_groups` - The groups of the edge's source
    /// * `target_groups` - The groups of the edge's target
    pub fn group_policy(
        &self,
        source_groups: &[String],
        target_groups: &[String],
    ) -> Result<(), Vec<String>> {
        // only kinds whose edges must be contained check the groups
        if !self.is_contained() {
            return Ok(());
        }
        // get the source groups the target is missing
        let missing = crate::utils::helpers::missing_groups(source_groups, target_groups);
        // a target missing no groups covers the source
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }

    /// Whether edges of this kind must be contained
    ///
    /// An edge must be contained when its source depends on a target that keeps one shared
    /// definition across its groups, so the source can use the target in every one of its groups.
    fn is_contained(self) -> bool {
        match self {
            // model configuration references and graph edges must be usable in every group of
            // their source
            Self::AuthenticatesWith
            | Self::Serves
            | Self::Orchestrates
            | Self::DelegatesTo
            | Self::Calls
            | Self::RunsIn
            | Self::Includes
            | Self::Provides => true,
            // these kinds link data that keeps independent copies per group, or an instance to
            // the root whose groups it carries
            Self::PublishedFrom
            | Self::BuiltFrom
            | Self::FileFor
            | Self::DocumentationFor
            | Self::FirmwareFor
            | Self::AssociatedWith
            | Self::DevelopedBy
            | Self::ContainsCVE
            | Self::ContainsCWE
            | Self::BasedIn
            | Self::EmployedBy
            | Self::ParentCompanyOf
            | Self::UsedBy
            | Self::UsedIn
            | Self::PerformedBy
            | Self::FileSystemIn
            | Self::FolderIn
            | Self::FileIn
            | Self::ProcessTreeIn
            | Self::ChildProcess
            | Self::HasNetworkConnection
            | Self::SectionIn
            | Self::ImportIn
            | Self::FlagFor
            | Self::CreatedBy
            | Self::SigmaRuleHit
            | Self::InvolvedIn => false,
        }
    }

    /// Cast our `AssociationKind` to a str
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            AssociationKind::FileFor => "FileFor",
            AssociationKind::DocumentationFor => "DocumentationFor",
            AssociationKind::FirmwareFor => "FirmwareFor",
            AssociationKind::AssociatedWith => "AssociatedWith",
            AssociationKind::DevelopedBy => "DevelopedBy",
            AssociationKind::ContainsCVE => "ContainsCVE",
            AssociationKind::ContainsCWE => "ContainsCWE",
            AssociationKind::BasedIn => "BasedIn",
            AssociationKind::ParentCompanyOf => "ParentCompanyOf",
            AssociationKind::EmployedBy => "EmployedBy",
            AssociationKind::UsedBy => "UsedBy",
            AssociationKind::UsedIn => "UsedIn",
            AssociationKind::PerformedBy => "PerformedBy",
            AssociationKind::FileSystemIn => "FileSystemIn",
            AssociationKind::FolderIn => "FolderIn",
            AssociationKind::FileIn => "FileIn",
            AssociationKind::ProcessTreeIn => "ProcessTreeIn",
            AssociationKind::ChildProcess => "ChildProcess",
            AssociationKind::HasNetworkConnection => "HasNetworkConnection",
            AssociationKind::SectionIn => "SectionIn",
            AssociationKind::ImportIn => "ImportIn",
            AssociationKind::FlagFor => "FlagFor",
            AssociationKind::CreatedBy => "CreatedBy",
            AssociationKind::SigmaRuleHit => "SigmaRuleHit",
            AssociationKind::InvolvedIn => "InvolvedIn",
            AssociationKind::PublishedFrom => "PublishedFrom",
            AssociationKind::AuthenticatesWith => "AuthenticatesWith",
            AssociationKind::Serves => "Serves",
            AssociationKind::Orchestrates => "Orchestrates",
            AssociationKind::DelegatesTo => "DelegatesTo",
            AssociationKind::Calls => "Calls",
            AssociationKind::RunsIn => "RunsIn",
            AssociationKind::BuiltFrom => "BuiltFrom",
            AssociationKind::Includes => "Includes",
            AssociationKind::Provides => "Provides",
        }
    }

    /// Whether edges of this kind make up the agentic graph that workflow instances are built from
    #[must_use]
    pub fn is_graph_edge(self) -> bool {
        // only the edges a workflow crawl follows are graph edges
        matches!(
            self,
            Self::Orchestrates | Self::DelegatesTo | Self::Calls | Self::RunsIn | Self::Includes
        )
    }

    /// Whether an edge of this kind may link a source of one entity kind to a target of another
    ///
    /// The agentic kinds only link the entity kinds they're defined for; every general-purpose
    /// kind accepts any pair.
    ///
    /// # Arguments
    ///
    /// * `source` - The kind of the edge's source
    /// * `target` - The kind of the edge's target
    #[must_use]
    pub fn allows_pair(self, source: EntityKinds, target: EntityKinds) -> bool {
        match self {
            // agents call tools and toolsets at latest or pinned to an instance
            Self::Calls => {
                source == EntityKinds::Agent
                    && matches!(
                        target,
                        EntityKinds::Tool
                            | EntityKinds::ToolInstance
                            | EntityKinds::ToolSet
                            | EntityKinds::ToolSetInstance
                    )
            }
            // agents delegate to agents
            Self::DelegatesTo => source == EntityKinds::Agent && target == EntityKinds::Agent,
            // agents run in a harness tool at latest or pinned to an instance
            Self::RunsIn => {
                source == EntityKinds::Agent
                    && matches!(target, EntityKinds::Tool | EntityKinds::ToolInstance)
            }
            // toolsets include tools at latest or pinned to an instance, never other toolsets
            Self::Includes => {
                source == EntityKinds::ToolSet
                    && matches!(target, EntityKinds::Tool | EntityKinds::ToolInstance)
            }
            // workflows orchestrate agents
            Self::Orchestrates => {
                source == EntityKinds::WorkflowCatalog && target == EntityKinds::Agent
            }
            // the other agentic kinds are written only by their owners, so no pair is allowed
            Self::BuiltFrom | Self::Provides => false,
            // model references and instance roots are written only by their owners, and the
            // general-purpose kinds accept any pair
            Self::PublishedFrom
            | Self::AuthenticatesWith
            | Self::Serves
            | Self::FileFor
            | Self::DocumentationFor
            | Self::FirmwareFor
            | Self::AssociatedWith
            | Self::DevelopedBy
            | Self::ContainsCVE
            | Self::ContainsCWE
            | Self::BasedIn
            | Self::EmployedBy
            | Self::ParentCompanyOf
            | Self::UsedBy
            | Self::UsedIn
            | Self::PerformedBy
            | Self::FileSystemIn
            | Self::FolderIn
            | Self::FileIn
            | Self::ProcessTreeIn
            | Self::ChildProcess
            | Self::HasNetworkConnection
            | Self::SectionIn
            | Self::ImportIn
            | Self::FlagFor
            | Self::CreatedBy
            | Self::SigmaRuleHit
            | Self::InvolvedIn => true,
        }
    }

    /// The association kind for for an entity back to a sample/repo (not an entity)
    ///
    /// This is not exhaustive and is best effort
    #[must_use]
    // tools have their own arm so it's clear they never get a more specific kind
    #[allow(clippy::match_same_arms)]
    pub fn to_parent_nonentity(kind: EntityKinds) -> Self {
        match kind {
            EntityKinds::WindowsProcessTree => Self::ProcessTreeIn,
            EntityKinds::WindowsProcess => Self::ChildProcess,
            EntityKinds::FileSystem => Self::FileSystemIn,
            EntityKinds::Folder => Self::FolderIn,
            EntityKinds::Flag => Self::FlagFor,
            EntityKinds::NetworkConnection => Self::HasNetworkConnection,
            EntityKinds::PeSection => Self::SectionIn,
            EntityKinds::PeImport => Self::ImportIn,
            // tools, agents, toolsets, workflows, and model configuration are never children of
            // samples or repos
            EntityKinds::Tool
            | EntityKinds::ToolInstance
            | EntityKinds::Agent
            | EntityKinds::ToolSet
            | EntityKinds::ToolSetInstance
            | EntityKinds::WorkflowCatalog
            | EntityKinds::WorkflowInstance
            | EntityKinds::ModelEndpoint
            | EntityKinds::ModelProfile
            | EntityKinds::ModelCredential => Self::AssociatedWith,
            _ => Self::AssociatedWith,
        }
    }
}

impl From<(EntityKinds, EntityKinds)> for AssociationKind {
    fn from(kinds: (EntityKinds, EntityKinds)) -> Self {
        match kinds {
            // edges to or from tools, agents, toolsets, workflows, and model configuration are
            // always created with an explicit kind
            (
                EntityKinds::Tool
                | EntityKinds::ToolInstance
                | EntityKinds::Agent
                | EntityKinds::ToolSet
                | EntityKinds::ToolSetInstance
                | EntityKinds::WorkflowCatalog
                | EntityKinds::WorkflowInstance
                | EntityKinds::ModelEndpoint
                | EntityKinds::ModelProfile
                | EntityKinds::ModelCredential,
                _,
            )
            | (
                _,
                EntityKinds::Tool
                | EntityKinds::ToolInstance
                | EntityKinds::Agent
                | EntityKinds::ToolSet
                | EntityKinds::ToolSetInstance
                | EntityKinds::WorkflowCatalog
                | EntityKinds::WorkflowInstance
                | EntityKinds::ModelEndpoint
                | EntityKinds::ModelProfile
                | EntityKinds::ModelCredential,
            ) => Self::AssociatedWith,
            (
                EntityKinds::WindowsProcessTree | EntityKinds::WindowsProcess,
                EntityKinds::WindowsProcess,
            ) => Self::ChildProcess,
            (EntityKinds::FileSystem, EntityKinds::Folder) => Self::FolderIn,
            (EntityKinds::SigmaRule, EntityKinds::Flag) => Self::CreatedBy,
            (EntityKinds::Flag, _) => Self::FlagFor,
            (EntityKinds::Vendor, EntityKinds::Device) => Self::DevelopedBy,
            (EntityKinds::Incident | EntityKinds::Other, _) | (_, EntityKinds::Other) => {
                Self::AssociatedWith
            }
            _ => Self::AssociatedWith,
        }
    }
}

/// The specific submissions tied to each side of an association
///
/// This lets an association point at the exact submission on either side when that side is a file/sample.
/// For example a `FileIn` association can record which submission placed a file in a folder, allowing that
/// submission's file name to be recovered losslessly.
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq, Hash, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AssociatedSubmissions {
    /// The submission id for the source side of this association, if it is a file/sample
    pub source: Option<Uuid>,
    /// The submission id for the other side of this association, if it is a file/sample
    pub other: Option<Uuid>,
}

impl AssociatedSubmissions {
    /// Check if this has no submission ids set on either side
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // we are empty if neither side has a submission id
        self.source.is_none() && self.other.is_none()
    }

    /// Get the opposite view of these submissions with source and other swapped
    ///
    /// This is used when storing/reading the inverted (other -> source) row for an association.
    #[must_use]
    pub fn swapped(&self) -> Self {
        AssociatedSubmissions {
            source: self.other,
            other: self.source,
        }
    }
}

/// An association with a specific piece of data
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct Association {
    /// The kind of association this is
    pub kind: AssociationKind,
    /// The other data this directional association is with
    pub other: AssociationTarget,
    /// The creator of this association
    pub submitter: String,
    /// The groups for this association
    pub groups: Vec<String>,
    /// When this association was created
    pub created: DateTime<Utc>,
    /// The direction for this association
    pub direction: Directionality,
    /// The specific submissions tied to each side of this association if any are known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submissions: Option<AssociatedSubmissions>,
}

/// A request to associate one piece of data with another
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AssociationRequest {
    /// The kind of association to make
    pub kind: AssociationKind,
    /// The piece of data this association starts with
    pub source: AssociationTarget,
    /// The data this association is with
    pub targets: Vec<AssociationTarget>,
    /// The groups for this association
    #[serde(default)]
    pub groups: Vec<String>,
    /// Whether this is a bidirectional relationship or not
    pub is_bidirectional: bool,
    /// The specific submissions tied to each side of this association if any are known
    ///
    /// When more than one target is set the `other` submission is applied to every target, so this is only
    /// meaningful for single target requests (the common case for filesystem links).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submissions: Option<AssociatedSubmissions>,
    /// Whether to add the source's groups that a target is missing to that target instead of
    /// rejecting the association (graph edges only)
    #[serde(default)]
    pub extend_target_groups: bool,
}

impl AssociationRequest {
    /// Create a new association request
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of association request to create
    /// * `source` - Where this assocation comes from
    #[must_use]
    pub fn new(kind: AssociationKind, source: AssociationTarget) -> Self {
        // build this association request
        AssociationRequest {
            kind,
            source,
            targets: Vec::default(),
            groups: Vec::default(),
            is_bidirectional: false,
            submissions: None,
            extend_target_groups: false,
        }
    }

    /// Create a new association request with a preset capacity
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of association request to create
    /// * `source` - Where this assocation comes from
    /// * `capacity` - The capacity to set
    #[must_use]
    pub fn with_capacity(
        kind: AssociationKind,
        source: AssociationTarget,
        capacity: usize,
    ) -> Self {
        // build this association request
        AssociationRequest {
            kind,
            source,
            targets: Vec::with_capacity(capacity),
            groups: Vec::with_capacity(capacity),
            is_bidirectional: false,
            submissions: None,
            extend_target_groups: false,
        }
    }

    /// Add a target for this association
    ///
    /// # Arguments
    ///
    /// * `target` - The target for this association
    #[must_use]
    pub fn target(mut self, target: AssociationTarget) -> Self {
        // add our new target
        self.targets.push(target);
        self
    }

    /// Set the groups to use with this association
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to add to this request
    #[must_use]
    pub fn groups<I>(mut self, groups: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        // extend our groups with our new groups
        self.groups
            .extend(groups.into_iter().map(std::convert::Into::into));
        self
    }

    /// Add the source's groups that a target is missing to that target instead of rejecting the
    /// association
    #[must_use]
    pub fn extend_target_groups(mut self) -> Self {
        self.extend_target_groups = true;
        self
    }

    /// Set this association to be bidirectional
    #[must_use]
    pub fn biderectional(mut self) -> Self {
        self.is_bidirectional = true;
        self
    }

    /// Set the submissions tied to each side of this association
    ///
    /// # Arguments
    ///
    /// * `submissions` - The submissions to tie to this association
    #[must_use]
    pub fn submissions(mut self, submissions: AssociatedSubmissions) -> Self {
        // set the submissions for this association
        self.submissions = Some(submissions);
        self
    }

    /// Tie a specific submission to the other/target side of this association
    ///
    /// This is a convenience for the common single target case (e.g. linking a file into a folder).
    ///
    /// # Arguments
    ///
    /// * `submission` - The submission id to tie to the other side of this association
    #[must_use]
    pub fn submission_other(mut self, submission: Uuid) -> Self {
        // get an entry to our submissions or insert a default and set the other side
        self.submissions
            .get_or_insert_with(AssociatedSubmissions::default)
            .other = Some(submission);
        self
    }

    /// Tie a specific submission to the source side of this association
    ///
    /// # Arguments
    ///
    /// * `submission` - The submission id to tie to the source side of this association
    #[must_use]
    pub fn submission_source(mut self, submission: Uuid) -> Self {
        // get an entry to our submissions or insert a default and set the source side
        self.submissions
            .get_or_insert_with(AssociatedSubmissions::default)
            .source = Some(submission);
        self
    }
}

/// A request to delete associations between one piece of data and others
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AssociationDeleteRequest {
    /// The kind of association to delete
    pub kind: AssociationKind,
    /// The piece of data these associations start with
    pub source: AssociationTarget,
    /// The data to remove associations with
    ///
    /// The name of entity targets is ignored when matching associations.
    pub targets: Vec<AssociationTarget>,
    /// The groups to delete these associations from (defaults to all groups visible to the user)
    #[serde(default)]
    pub groups: Vec<String>,
    /// The directions relative to the source to delete associations for (defaults to any direction)
    #[serde(default)]
    pub directions: Vec<Directionality>,
}

impl AssociationDeleteRequest {
    /// Create a new association delete request
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of association to delete
    /// * `source` - Where the associations to delete come from
    #[must_use]
    pub fn new(kind: AssociationKind, source: AssociationTarget) -> Self {
        AssociationDeleteRequest {
            kind,
            source,
            targets: Vec::default(),
            groups: Vec::default(),
            directions: Vec::default(),
        }
    }

    /// Add a target to delete associations with
    ///
    /// # Arguments
    ///
    /// * `target` - The target to delete associations with
    #[must_use]
    pub fn target(mut self, target: AssociationTarget) -> Self {
        // add our new target
        self.targets.push(target);
        self
    }

    /// Add multiple targets to delete associations with
    ///
    /// # Arguments
    ///
    /// * `targets` - The targets to delete associations with
    #[must_use]
    pub fn targets<I>(mut self, targets: I) -> Self
    where
        I: IntoIterator<Item = AssociationTarget>,
    {
        // add our new targets
        self.targets.extend(targets);
        self
    }

    /// Restrict this delete to specific groups
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to delete associations from
    #[must_use]
    pub fn groups<I>(mut self, groups: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        // extend our groups with our new groups
        self.groups
            .extend(groups.into_iter().map(|group| group.into()));
        self
    }

    /// Restrict this delete to associations in a specific direction relative to the source
    ///
    /// # Arguments
    ///
    /// * `direction` - The direction to delete associations in
    #[must_use]
    pub fn direction(mut self, direction: Directionality) -> Self {
        // add this direction if it isn't already set
        if !self.directions.contains(&direction) {
            self.directions.push(direction);
        }
        self
    }
}

pub trait AssociationSupport {
    /// Check if this assocition kind is valid
    ///
    /// False means this association is not valid for this entity.
    ///
    /// # Arguments
    ///
    /// * `association` - The association to check the validity off
    fn is_valid(association: &Association) -> bool;

    /// Save this association to the DB
    #[cfg(feature = "api")]
    fn save(&self, association: &Association) -> Result<(), crate::utils::ApiError>;
}

/// The options that you can set when listing associations in Thorium
///
/// Currently this only supports single tag queries but when ES support is added multi tag queries
/// will be supported.
#[derive(Debug, Clone)]
pub struct AssociationListOpts {
    /// The cursor to use to continue this search
    pub cursor: Option<Uuid>,
    /// The latest date to start listing samples from
    pub start: Option<DateTime<Utc>>,
    /// The oldest date to stop listing samples from
    pub end: Option<DateTime<Utc>>,
    /// The max number of objects to retrieve on a single page
    pub page_size: usize,
    /// The total number of objects to return with this cursor
    pub limit: Option<usize>,
    /// The groups limit our search to
    pub groups: Vec<String>,
}

impl Default for AssociationListOpts {
    /// Build a default search
    fn default() -> Self {
        AssociationListOpts {
            start: None,
            cursor: None,
            end: None,
            page_size: 50,
            limit: None,
            groups: Vec::default(),
        }
    }
}

impl AssociationListOpts {
    /// Restrict the association search to start at a specific date
    ///
    /// # Arguments
    ///
    /// * `start` - The date to start listing samples from
    #[must_use]
    pub fn start(mut self, start: DateTime<Utc>) -> Self {
        // set the date to start listing associations at
        self.start = Some(start);
        self
    }

    /// Set the cursor to use when continuing this search
    ///
    /// # Arguments
    ///
    /// * `cursor` - The cursor id to use for this search
    #[must_use]
    pub fn cursor(mut self, cursor: Uuid) -> Self {
        // set cursor for this search
        self.cursor = Some(cursor);
        self
    }

    /// Restrict the association search to stop at a specific date
    ///
    /// # Arguments
    ///
    /// * `end` - The date to stop listing samples at
    #[must_use]
    pub fn end(mut self, end: DateTime<Utc>) -> Self {
        // set the date to end listing associations at
        self.end = Some(end);
        self
    }

    /// The max number of objects to retrieve in a single page
    ///
    /// # Arguments
    ///
    /// * `page_size` - The max number of documents to return in a single request
    #[must_use]
    pub fn page_size(mut self, page_size: usize) -> Self {
        // set the date to end listing associations at
        self.page_size = page_size;
        self
    }

    /// Limit how many samples this search can return at once
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of objects to return over the lifetime of this cursor
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        // set the date to end listing associations at
        self.limit = Some(limit);
        self
    }

    /// Limit what groups we search in
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to restrict our search to
    #[must_use]
    pub fn groups<T: Into<String>>(mut self, groups: Vec<T>) -> Self {
        // set the date to end listing associations at
        self.groups
            .extend(groups.into_iter().map(std::convert::Into::into));
        self
    }
}

/// Default the association list limit to 50
fn default_list_limit() -> usize {
    50
}

#[derive(Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct AssociationListParams {
    /// The groups to list data from
    #[serde(default)]
    pub groups: Vec<String>,
    /// When to start listing data at
    #[serde(default = "Utc::now")]
    pub start: DateTime<Utc>,
    /// When to stop listing data at
    pub end: Option<DateTime<Utc>>,
    /// The cursor id to use if one exists
    pub cursor: Option<Uuid>,
    /// The max number of items to return in this response
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

impl Default for AssociationListParams {
    /// Create a default file list params
    fn default() -> Self {
        AssociationListParams {
            groups: Vec::default(),
            start: Utc::now(),
            end: None,
            cursor: None,
            limit: default_list_limit(),
        }
    }
}

impl From<AssociationListOpts> for AssociationListParams {
    /// Convert `AssociationListOpts` into `AssociationListParams`
    fn from(opts: AssociationListOpts) -> Self {
        AssociationListParams {
            groups: opts.groups,
            start: opts.start.unwrap_or_else(Utc::now),
            end: opts.end,
            cursor: opts.cursor,
            limit: opts.limit.unwrap_or_else(default_list_limit),
        }
    }
}

impl AssociationListParams {
    /// Get the end timestamp or get a sane default
    #[cfg(feature = "api")]
    pub fn end(
        &self,
        shared: &crate::utils::Shared,
    ) -> Result<DateTime<Utc>, crate::utils::ApiError> {
        match self.end {
            Some(end) => Ok(end),
            None => match Utc.timestamp_opt(shared.config.thorium.associations.earliest, 0) {
                chrono::LocalResult::Single(default_end) => Ok(default_end),
                _ => crate::internal_err!(format!(
                    "default earliest association timestamp is invalid or ambiguous - {}",
                    shared.config.thorium.associations.earliest
                )),
            },
        }
    }
}

/// The number of associations in one partition of the associations table, read to rebuild its
/// census
#[derive(Debug)]
#[cfg_attr(
    feature = "scylla-utils",
    derive(crate::models::schema::ScyllaProjection)
)]
#[cfg_attr(
    feature = "scylla-utils",
    thorium_projection(of = crate::models::schema::scylla::AssociationsTable)
)]
pub struct AssociationCensusRow {
    /// The group the partition's associations are in
    pub group: String,
    /// The year the partition's associations were created in
    pub year: i32,
    /// The partition's bucket within its year
    pub bucket: i32,
    /// The entity or object the partition's associations are from, as stored
    pub source: String,
    /// How many associations the partition holds
    #[cfg_attr(feature = "scylla-utils", thorium(expr = "count(*)"))]
    pub count: i64,
}

#[cfg(feature = "scylla-utils")]
impl super::CensusSupport for Association {
    /// The type returned by our prepared statement
    type Row = AssociationCensusRow;

    /// The generated census statement's text for a namespace
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace the table is in
    fn census_cql(ns: &str) -> String {
        // the census statement generated on the table's queries struct
        crate::models::schema::queries::AssociationsQueries::census_cql(ns)
    }

    /// Get the count for this partition
    ///
    /// # Arguments
    ///
    /// * `row` - The partition's census row
    fn get_count(row: &AssociationCensusRow) -> i64 {
        row.count
    }

    /// Get the bucket for this partition
    ///
    /// # Arguments
    ///
    /// * `row` - The partition's census row
    fn get_bucket(row: &AssociationCensusRow) -> i32 {
        row.bucket
    }

    /// Build the sorted set key for this census operation; each association is stored from both
    /// of its sides, so counting each side's partition fills the streams of both, as creating it
    /// does
    ///
    /// # Arguments
    ///
    /// * `namespace` - The namespace the census is in
    /// * `row` - The partition's census row
    fn stream_key_from_row(namespace: &str, row: &Self::Row) -> String {
        // build the stream key for this row
        format!(
            "{namespace}:census:associations:stream:{group}:{source}:{year}",
            group = row.group,
            source = row.source,
            year = row.year,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use strum::IntoEnumIterator;

    use super::{AssociationKind, AssociationRequest, AssociationTarget};
    use crate::models::EntityKinds;

    /// The kinds whose edges must be contained
    const CONTAINED: &[AssociationKind] = &[
        AssociationKind::Orchestrates,
        AssociationKind::DelegatesTo,
        AssociationKind::Calls,
        AssociationKind::RunsIn,
        AssociationKind::Includes,
        AssociationKind::Provides,
        AssociationKind::AuthenticatesWith,
        AssociationKind::Serves,
    ];

    /// The association kinds of the agentic graph and model configuration
    const AGENTIC_KINDS: [(AssociationKind, &str); 10] = [
        (AssociationKind::Orchestrates, "Orchestrates"),
        (AssociationKind::DelegatesTo, "DelegatesTo"),
        (AssociationKind::Calls, "Calls"),
        (AssociationKind::RunsIn, "RunsIn"),
        (AssociationKind::PublishedFrom, "PublishedFrom"),
        (AssociationKind::BuiltFrom, "BuiltFrom"),
        (AssociationKind::Includes, "Includes"),
        (AssociationKind::Provides, "Provides"),
        (AssociationKind::AuthenticatesWith, "AuthenticatesWith"),
        (AssociationKind::Serves, "Serves"),
    ];

    /// Build a list of owned group names from string slices
    ///
    /// # Arguments
    ///
    /// * `groups` - The group names to own
    fn groups(groups: &[&str]) -> Vec<String> {
        groups.iter().map(|group| (*group).to_string()).collect()
    }

    /// `AssociationKind::group_policy` returns the source groups a contained edge's target lacks
    #[test]
    fn association_kind_group_policies() {
        // check every kind's policy
        for kind in AssociationKind::iter() {
            // a target that covers the source is accepted by every kind
            assert_eq!(
                kind.group_policy(&groups(&["a"]), &groups(&["a", "b"])),
                Ok(())
            );
            // an empty source is accepted by every kind
            assert_eq!(kind.group_policy(&[], &groups(&["a"])), Ok(()));
            // a target missing one of the source's groups
            let result = kind.group_policy(&groups(&["a", "b"]), &groups(&["a"]));
            // a target missing several of the source's groups
            let many = kind.group_policy(&groups(&["c", "a", "b"]), &[]);
            // contained kinds return the missing groups in order and every other kind accepts them
            if CONTAINED.contains(&kind) {
                assert_eq!(result, Err(groups(&["b"])), "{kind}");
                assert_eq!(many, Err(groups(&["c", "a", "b"])), "{kind}");
            } else {
                assert_eq!(result, Ok(()), "{kind}");
                assert_eq!(many, Ok(()), "{kind}");
            }
        }
    }

    /// The agentic and model configuration association kinds round trip through their strings
    /// and serde
    #[test]
    fn agentic_kinds_round_trip() {
        // each kind's string is its name and parses back
        for (kind, name) in AGENTIC_KINDS {
            assert_eq!(kind.as_str(), name);
            assert_eq!(AssociationKind::from_str(name).unwrap(), kind);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<AssociationKind>(&json).unwrap(),
                kind
            );
        }
        // every kind iterates
        let all = AssociationKind::iter().collect::<Vec<AssociationKind>>();
        assert!(AGENTIC_KINDS.iter().all(|(kind, _)| all.contains(kind)));
    }

    /// Graph edge kinds only link the kinds they're defined for
    #[test]
    fn graph_kind_pairs() {
        // agents call agentic tools and tool instances, run in harnesses, and delegate to agents
        let allowed = [
            (
                AssociationKind::RunsIn,
                EntityKinds::Agent,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::RunsIn,
                EntityKinds::Agent,
                EntityKinds::ToolInstance,
            ),
            (
                AssociationKind::Calls,
                EntityKinds::Agent,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::Calls,
                EntityKinds::Agent,
                EntityKinds::ToolInstance,
            ),
            (
                AssociationKind::DelegatesTo,
                EntityKinds::Agent,
                EntityKinds::Agent,
            ),
        ];
        for (kind, source, target) in allowed {
            assert!(kind.allows_pair(source, target), "{kind} {source} {target}");
        }
        // every other pair of a graph kind is refused
        let refused = [
            (
                AssociationKind::RunsIn,
                EntityKinds::Agent,
                EntityKinds::ToolSet,
            ),
            (
                AssociationKind::RunsIn,
                EntityKinds::Tool,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::Calls,
                EntityKinds::Agent,
                EntityKinds::Agent,
            ),
            (AssociationKind::Calls, EntityKinds::Tool, EntityKinds::Tool),
            (
                AssociationKind::DelegatesTo,
                EntityKinds::Agent,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::Orchestrates,
                EntityKinds::Agent,
                EntityKinds::Agent,
            ),
            (
                AssociationKind::RunsIn,
                EntityKinds::Agent,
                EntityKinds::Agent,
            ),
            (
                AssociationKind::Includes,
                EntityKinds::Agent,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::BuiltFrom,
                EntityKinds::ToolInstance,
                EntityKinds::Tool,
            ),
        ];
        for (kind, source, target) in refused {
            assert!(
                !kind.allows_pair(source, target),
                "{kind} {source} {target}"
            );
        }
        // the graph kinds are exactly the ones that queue edge changes
        for kind in AssociationKind::iter() {
            let graph = matches!(
                kind,
                AssociationKind::Orchestrates
                    | AssociationKind::DelegatesTo
                    | AssociationKind::Calls
                    | AssociationKind::RunsIn
                    | AssociationKind::Includes
            );
            assert_eq!(kind.is_graph_edge(), graph, "{kind}");
        }
        // general-purpose kinds accept any pair
        assert!(AssociationKind::DevelopedBy.allows_pair(EntityKinds::Device, EntityKinds::Agent));
    }

    /// Agents call toolsets, and toolsets include only tools and tool instances
    #[test]
    fn toolset_kind_pairs() {
        // the toolset pairs that are allowed
        let allowed = [
            (
                AssociationKind::Calls,
                EntityKinds::Agent,
                EntityKinds::ToolSet,
            ),
            (
                AssociationKind::Calls,
                EntityKinds::Agent,
                EntityKinds::ToolSetInstance,
            ),
            (
                AssociationKind::Includes,
                EntityKinds::ToolSet,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::Includes,
                EntityKinds::ToolSet,
                EntityKinds::ToolInstance,
            ),
        ];
        for (kind, source, target) in allowed {
            assert!(kind.allows_pair(source, target), "{kind} {source} {target}");
        }
        // toolsets never nest or include agents, and only agents call toolsets
        let refused = [
            (
                AssociationKind::Includes,
                EntityKinds::ToolSet,
                EntityKinds::ToolSet,
            ),
            (
                AssociationKind::Includes,
                EntityKinds::ToolSet,
                EntityKinds::ToolSetInstance,
            ),
            (
                AssociationKind::Includes,
                EntityKinds::ToolSet,
                EntityKinds::Agent,
            ),
            (
                AssociationKind::Includes,
                EntityKinds::ToolSetInstance,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::Calls,
                EntityKinds::ToolSet,
                EntityKinds::Tool,
            ),
            (
                AssociationKind::BuiltFrom,
                EntityKinds::ToolSetInstance,
                EntityKinds::ToolSet,
            ),
        ];
        for (kind, source, target) in refused {
            assert!(
                !kind.allows_pair(source, target),
                "{kind} {source} {target}"
            );
        }
    }

    /// Workflows orchestrate only agents, and no instance is linked to a workflow by hand
    #[test]
    fn workflow_kind_pairs() {
        // a workflow orchestrates an agent
        assert!(
            AssociationKind::Orchestrates
                .allows_pair(EntityKinds::WorkflowCatalog, EntityKinds::Agent)
        );
        // a workflow never orchestrates anything else, and only workflows orchestrate
        let refused = [
            (EntityKinds::WorkflowCatalog, EntityKinds::Tool),
            (EntityKinds::WorkflowCatalog, EntityKinds::ToolInstance),
            (EntityKinds::WorkflowCatalog, EntityKinds::ToolSet),
            (EntityKinds::WorkflowCatalog, EntityKinds::WorkflowCatalog),
            (EntityKinds::WorkflowCatalog, EntityKinds::WorkflowInstance),
            (EntityKinds::WorkflowInstance, EntityKinds::Agent),
            (EntityKinds::Agent, EntityKinds::Agent),
        ];
        for (source, target) in refused {
            assert!(
                !AssociationKind::Orchestrates.allows_pair(source, target),
                "{source} {target}"
            );
        }
        // an instance's link to its workflow is written only when it's submitted
        assert!(
            !AssociationKind::BuiltFrom
                .allows_pair(EntityKinds::WorkflowInstance, EntityKinds::WorkflowCatalog)
        );
        // agents never call workflows
        assert!(
            !AssociationKind::Calls.allows_pair(EntityKinds::Agent, EntityKinds::WorkflowCatalog)
        );
    }

    /// `extend_target_groups` defaults to off, is set by its builder, and round trips
    #[test]
    fn extend_target_groups_field() {
        // a request without the field reads as off
        let source = serde_json::json!({"Entity": {"id": uuid::Uuid::nil(), "name": "a"}});
        let raw = serde_json::json!({
            "kind": "Calls",
            "source": source,
            "targets": [],
            "is_bidirectional": false
        });
        let req: AssociationRequest = serde_json::from_value(raw).unwrap();
        assert!(!req.extend_target_groups);
        // new leaves it off and the builder turns it on
        let target = AssociationTarget::Entity {
            id: uuid::Uuid::nil(),
            name: "a".into(),
        };
        let req = AssociationRequest::new(AssociationKind::Calls, target);
        assert!(!req.extend_target_groups);
        let req = req.extend_target_groups();
        assert!(req.extend_target_groups);
        // it serializes and round trips
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["extend_target_groups"], serde_json::json!(true));
        let back: AssociationRequest = serde_json::from_value(value).unwrap();
        assert!(back.extend_target_groups);
    }
}
