//! One value of every variant of each enum the fixture must cover
//!
//! Each list is guarded by an index function whose `match` has no wildcard arm, so adding a
//! variant to the enum fails to compile until the list holds it too. The lists cover the enums
//! stored inside archives and every enum in the API's `Backup` schema. Every guard takes its value
//! by reference, whatever the enum's size, so one list builder can take any of them.

#![allow(clippy::trivially_copy_pass_by_ref)]

use serde_json::json;
use thorium::models::{
    ArgStrategy, AssociationKind, AutoTagLogic, CommitishKinds, DependencyPassStrategy,
    Directionality, EntityKinds, EventTrigger, FileNamingStrategy, HostPathTypes, ImageBanKind,
    ImageScaler, ImageVersion, KwargDependency, NodeHealth, NotificationLevel, NotificationType,
    OutputDisplayType, OutputHandler, PipelineBanKind, PruneAction, PrunedReferent, S3Objects,
    ScopedTokenRole, SpawnLimits, TagType, Theme, UserRole, VolumeTypes,
};

/// Builds a value from its JSON form, for variants that carry data
///
/// # Arguments
///
/// * `value` - The variant's JSON form
fn from_json<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> T {
    // the forms below are written by hand, so a failure is a bug in this file
    T::deserialize(value).unwrap_or_else(|err| panic!("bad enum value {value}: {err}"))
}

/// One value of every `TagType` variant, in declaration order
#[must_use]
pub fn tag_types() -> Vec<TagType> {
    vec![TagType::Files, TagType::Repos, TagType::Entities]
}

/// Returns a `TagType` variant's position in `tag_types`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn tag_type_index(value: &TagType) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        TagType::Files => 0,
        TagType::Repos => 1,
        TagType::Entities => 2,
    }
}

/// One value of every `NodeHealth` variant, in declaration order
#[must_use]
pub fn node_healths() -> Vec<NodeHealth> {
    vec![
        NodeHealth::Healthy,
        NodeHealth::Unhealthy,
        NodeHealth::Disabled(Some("maintenance".to_owned())),
        NodeHealth::Registered,
    ]
}

/// Returns a `NodeHealth` variant's position in `node_healths`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn node_health_index(value: &NodeHealth) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        NodeHealth::Healthy => 0,
        NodeHealth::Unhealthy => 1,
        NodeHealth::Disabled(_) => 2,
        NodeHealth::Registered => 3,
    }
}

/// One value of every `EntityKinds` variant, in declaration order
#[must_use]
pub fn entity_kinds() -> Vec<EntityKinds> {
    vec![
        EntityKinds::Device,
        EntityKinds::Vendor,
        EntityKinds::Collection,
        EntityKinds::FileSystem,
        EntityKinds::Folder,
        EntityKinds::WindowsProcessTree,
        EntityKinds::WindowsProcess,
        EntityKinds::NetworkConnection,
        EntityKinds::SigmaRule,
        EntityKinds::Flag,
        EntityKinds::Incident,
        EntityKinds::CompiledFunction,
        EntityKinds::DecompiledFunction,
        EntityKinds::PeSection,
        EntityKinds::PeImport,
        EntityKinds::Other,
    ]
}

/// Returns an `EntityKinds` variant's position in `entity_kinds`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn entity_kind_index(value: &EntityKinds) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        EntityKinds::Device => 0,
        EntityKinds::Vendor => 1,
        EntityKinds::Collection => 2,
        EntityKinds::FileSystem => 3,
        EntityKinds::Folder => 4,
        EntityKinds::WindowsProcessTree => 5,
        EntityKinds::WindowsProcess => 6,
        EntityKinds::NetworkConnection => 7,
        EntityKinds::SigmaRule => 8,
        EntityKinds::Flag => 9,
        EntityKinds::Incident => 10,
        EntityKinds::CompiledFunction => 11,
        EntityKinds::DecompiledFunction => 12,
        EntityKinds::PeSection => 13,
        EntityKinds::PeImport => 14,
        EntityKinds::Other => 15,
    }
}

/// One value of every `OutputDisplayType` variant, in declaration order
#[must_use]
pub fn output_display_types() -> Vec<OutputDisplayType> {
    vec![
        OutputDisplayType::Json,
        OutputDisplayType::String,
        OutputDisplayType::Table,
        OutputDisplayType::Image,
        OutputDisplayType::Custom,
        OutputDisplayType::Disassembly,
        OutputDisplayType::Html,
        OutputDisplayType::Markdown,
        OutputDisplayType::Hidden,
        OutputDisplayType::Xml,
        OutputDisplayType::Yaml,
        OutputDisplayType::Decomposition,
    ]
}

/// Returns an `OutputDisplayType` variant's position in `output_display_types`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn output_display_type_index(value: &OutputDisplayType) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        OutputDisplayType::Json => 0,
        OutputDisplayType::String => 1,
        OutputDisplayType::Table => 2,
        OutputDisplayType::Image => 3,
        OutputDisplayType::Custom => 4,
        OutputDisplayType::Disassembly => 5,
        OutputDisplayType::Html => 6,
        OutputDisplayType::Markdown => 7,
        OutputDisplayType::Hidden => 8,
        OutputDisplayType::Xml => 9,
        OutputDisplayType::Yaml => 10,
        OutputDisplayType::Decomposition => 11,
    }
}

/// One value of every `NotificationType` variant, in declaration order
#[must_use]
pub fn notification_types() -> Vec<NotificationType> {
    vec![NotificationType::Images, NotificationType::Pipelines]
}

/// Returns a `NotificationType` variant's position in `notification_types`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn notification_type_index(value: &NotificationType) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        NotificationType::Images => 0,
        NotificationType::Pipelines => 1,
    }
}

/// One value of every `NotificationLevel` variant, in declaration order
#[must_use]
pub fn notification_levels() -> Vec<NotificationLevel> {
    vec![
        NotificationLevel::Info,
        NotificationLevel::Warn,
        NotificationLevel::Error,
    ]
}

/// Returns a `NotificationLevel` variant's position in `notification_levels`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn notification_level_index(value: &NotificationLevel) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        NotificationLevel::Info => 0,
        NotificationLevel::Warn => 1,
        NotificationLevel::Error => 2,
    }
}

/// One value of every `S3Objects` variant, in declaration order
#[must_use]
pub fn s3_objects() -> Vec<S3Objects> {
    vec![S3Objects::File, S3Objects::Repo]
}

/// Returns an `S3Objects` variant's position in `s3_objects`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn s3_object_index(value: &S3Objects) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        S3Objects::File => 0,
        S3Objects::Repo => 1,
    }
}

/// One value of every `AssociationKind` variant, in declaration order
#[must_use]
pub fn association_kinds() -> Vec<AssociationKind> {
    vec![
        AssociationKind::FileFor,
        AssociationKind::DocumentationFor,
        AssociationKind::FirmwareFor,
        AssociationKind::AssociatedWith,
        AssociationKind::DevelopedBy,
        AssociationKind::ContainsCVE,
        AssociationKind::ContainsCWE,
        AssociationKind::BasedIn,
        AssociationKind::EmployedBy,
        AssociationKind::ParentCompanyOf,
        AssociationKind::UsedBy,
        AssociationKind::UsedIn,
        AssociationKind::PerformedBy,
        AssociationKind::FileSystemIn,
        AssociationKind::FolderIn,
        AssociationKind::FileIn,
        AssociationKind::ProcessTreeIn,
        AssociationKind::ChildProcess,
        AssociationKind::HasNetworkConnection,
        AssociationKind::SectionIn,
        AssociationKind::ImportIn,
        AssociationKind::FlagFor,
        AssociationKind::CreatedBy,
        AssociationKind::SigmaRuleHit,
        AssociationKind::InvolvedIn,
    ]
}

/// Returns an `AssociationKind` variant's position in `association_kinds`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn association_kind_index(value: &AssociationKind) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        AssociationKind::FileFor => 0,
        AssociationKind::DocumentationFor => 1,
        AssociationKind::FirmwareFor => 2,
        AssociationKind::AssociatedWith => 3,
        AssociationKind::DevelopedBy => 4,
        AssociationKind::ContainsCVE => 5,
        AssociationKind::ContainsCWE => 6,
        AssociationKind::BasedIn => 7,
        AssociationKind::EmployedBy => 8,
        AssociationKind::ParentCompanyOf => 9,
        AssociationKind::UsedBy => 10,
        AssociationKind::UsedIn => 11,
        AssociationKind::PerformedBy => 12,
        AssociationKind::FileSystemIn => 13,
        AssociationKind::FolderIn => 14,
        AssociationKind::FileIn => 15,
        AssociationKind::ProcessTreeIn => 16,
        AssociationKind::ChildProcess => 17,
        AssociationKind::HasNetworkConnection => 18,
        AssociationKind::SectionIn => 19,
        AssociationKind::ImportIn => 20,
        AssociationKind::FlagFor => 21,
        AssociationKind::CreatedBy => 22,
        AssociationKind::SigmaRuleHit => 23,
        AssociationKind::InvolvedIn => 24,
    }
}

/// One value of every `Directionality` variant, in declaration order
#[must_use]
pub fn directionalities() -> Vec<Directionality> {
    vec![
        Directionality::To,
        Directionality::From,
        Directionality::Bidirectional,
    ]
}

/// Returns a `Directionality` variant's position in `directionalities`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn directionality_index(value: &Directionality) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        Directionality::To => 0,
        Directionality::From => 1,
        Directionality::Bidirectional => 2,
    }
}

/// One value of every `CommitishKinds` variant, in declaration order
#[must_use]
pub fn commitish_kinds() -> Vec<CommitishKinds> {
    vec![
        CommitishKinds::Commit,
        CommitishKinds::Branch,
        CommitishKinds::Tag,
    ]
}

/// Returns a `CommitishKinds` variant's position in `commitish_kinds`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn commitish_kind_index(value: &CommitishKinds) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        CommitishKinds::Commit => 0,
        CommitishKinds::Branch => 1,
        CommitishKinds::Tag => 2,
    }
}

/// One value of every `UserRole` variant, in declaration order
#[must_use]
pub fn user_roles() -> Vec<UserRole> {
    vec![
        UserRole::Admin,
        UserRole::Analyst,
        UserRole::Developer {
            k8s: true,
            bare_metal: true,
            windows: true,
            external: true,
            kvm: true,
        },
        UserRole::User,
        UserRole::Disabled,
    ]
}

/// Returns a `UserRole` variant's position in `user_roles`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn user_role_index(value: &UserRole) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        UserRole::Admin => 0,
        UserRole::Analyst => 1,
        UserRole::Developer { .. } => 2,
        UserRole::User => 3,
        UserRole::Disabled => 4,
    }
}

/// One value of every `ScopedTokenRole` variant, in declaration order
#[must_use]
pub fn scoped_token_roles() -> Vec<ScopedTokenRole> {
    vec![
        ScopedTokenRole::Developer {
            k8s: true,
            bare_metal: true,
            windows: true,
            external: true,
            kvm: true,
        },
        ScopedTokenRole::User,
    ]
}

/// Returns a `ScopedTokenRole` variant's position in `scoped_token_roles`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn scoped_token_role_index(value: &ScopedTokenRole) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        ScopedTokenRole::Developer { .. } => 0,
        ScopedTokenRole::User => 1,
    }
}

/// One value of every `PruneAction` variant, in declaration order
#[must_use]
pub fn prune_actions() -> Vec<PruneAction> {
    vec![
        PruneAction::Dropped,
        PruneAction::Added,
        PruneAction::Deleted,
    ]
}

/// Returns a `PruneAction` variant's position in `prune_actions`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn prune_action_index(value: &PruneAction) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        PruneAction::Dropped => 0,
        PruneAction::Added => 1,
        PruneAction::Deleted => 2,
    }
}

/// One value of every `PrunedReferent` variant, in declaration order
#[must_use]
pub fn pruned_referents() -> Vec<PrunedReferent> {
    vec![PrunedReferent::User, PrunedReferent::Group]
}

/// Returns a `PrunedReferent` variant's position in `pruned_referents`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn pruned_referent_index(value: &PrunedReferent) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        PrunedReferent::User => 0,
        PrunedReferent::Group => 1,
    }
}

/// One value of every `Theme` variant, in declaration order
#[must_use]
pub fn themes() -> Vec<Theme> {
    vec![
        Theme::Dark,
        Theme::Light,
        Theme::Crab,
        Theme::Ocean,
        Theme::Automatic,
    ]
}

/// Returns a `Theme` variant's position in `themes`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn theme_index(value: &Theme) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        Theme::Dark => 0,
        Theme::Light => 1,
        Theme::Crab => 2,
        Theme::Ocean => 3,
        Theme::Automatic => 4,
    }
}

/// One value of every `ImageScaler` variant, in declaration order
#[must_use]
pub fn image_scalers() -> Vec<ImageScaler> {
    vec![
        ImageScaler::K8s,
        ImageScaler::BareMetal,
        ImageScaler::Windows,
        ImageScaler::Kvm,
        ImageScaler::External,
    ]
}

/// Returns an `ImageScaler` variant's position in `image_scalers`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn image_scaler_index(value: &ImageScaler) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        ImageScaler::K8s => 0,
        ImageScaler::BareMetal => 1,
        ImageScaler::Windows => 2,
        ImageScaler::Kvm => 3,
        ImageScaler::External => 4,
    }
}

/// One value of every `DependencyPassStrategy` variant, in declaration order
#[must_use]
pub fn dependency_pass_strategies() -> Vec<DependencyPassStrategy> {
    vec![
        DependencyPassStrategy::Paths,
        DependencyPassStrategy::Names,
        DependencyPassStrategy::Directory,
        DependencyPassStrategy::Disabled,
    ]
}

/// Returns a `DependencyPassStrategy` variant's position in `dependency_pass_strategies`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn dependency_pass_strategy_index(value: &DependencyPassStrategy) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        DependencyPassStrategy::Paths => 0,
        DependencyPassStrategy::Names => 1,
        DependencyPassStrategy::Directory => 2,
        DependencyPassStrategy::Disabled => 3,
    }
}

/// One value of every `FileNamingStrategy` variant, in declaration order
#[must_use]
pub fn file_naming_strategies() -> Vec<FileNamingStrategy> {
    vec![FileNamingStrategy::Sha256, FileNamingStrategy::MostRecent]
}

/// Returns a `FileNamingStrategy` variant's position in `file_naming_strategies`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn file_naming_strategy_index(value: &FileNamingStrategy) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        FileNamingStrategy::Sha256 => 0,
        FileNamingStrategy::MostRecent => 1,
    }
}

/// One value of every `ArgStrategy` variant, in declaration order
#[must_use]
pub fn arg_strategies() -> Vec<ArgStrategy> {
    vec![
        ArgStrategy::None,
        ArgStrategy::Append,
        ArgStrategy::Kwarg("--input".to_owned()),
    ]
}

/// Returns an `ArgStrategy` variant's position in `arg_strategies`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn arg_strategy_index(value: &ArgStrategy) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        ArgStrategy::None => 0,
        ArgStrategy::Append => 1,
        ArgStrategy::Kwarg(_) => 2,
    }
}

/// One value of every `KwargDependency` variant, in declaration order
#[must_use]
pub fn kwarg_dependencies() -> Vec<KwargDependency> {
    vec![
        KwargDependency::List("--results".to_owned()),
        KwargDependency::Map([("tool".to_owned(), "--tool".to_owned())].into()),
        KwargDependency::None,
    ]
}

/// Returns a `KwargDependency` variant's position in `kwarg_dependencies`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn kwarg_dependency_index(value: &KwargDependency) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        KwargDependency::List(_) => 0,
        KwargDependency::Map(_) => 1,
        KwargDependency::None => 2,
    }
}

/// One value of every `ImageBanKind` variant, in declaration order
#[must_use]
pub fn image_ban_kinds() -> Vec<ImageBanKind> {
    vec![
        from_json(&json!({"Generic": {"msg": "banned for testing"}})),
        from_json(&json!({"InvalidImageUrl": {"url": "registry.invalid/image"}})),
        from_json(&json!({"InvalidHostPath": {"volume_name": "data", "host_path": "/forbidden"}})),
    ]
}

/// Returns an `ImageBanKind` variant's position in `image_ban_kinds`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn image_ban_kind_index(value: &ImageBanKind) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        ImageBanKind::Generic(_) => 0,
        ImageBanKind::InvalidImageUrl(_) => 1,
        ImageBanKind::InvalidHostPath(_) => 2,
    }
}

/// One value of every `PipelineBanKind` variant, in declaration order
#[must_use]
pub fn pipeline_ban_kinds() -> Vec<PipelineBanKind> {
    vec![
        from_json(&json!({"Generic": {"msg": "banned for testing"}})),
        from_json(&json!({"BannedImage": {"image": "banned-image"}})),
    ]
}

/// Returns a `PipelineBanKind` variant's position in `pipeline_ban_kinds`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn pipeline_ban_kind_index(value: &PipelineBanKind) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        PipelineBanKind::Generic(_) => 0,
        PipelineBanKind::BannedImage(_) => 1,
    }
}

/// One value of every `EventTrigger` variant, in declaration order
#[must_use]
pub fn event_triggers() -> Vec<EventTrigger> {
    vec![
        EventTrigger::Tag {
            tag_types: vec![TagType::Files],
            required: [("Planet".to_owned(), vec!["Jupiter".to_owned()])].into(),
            not: [("Moon".to_owned(), vec!["Io".to_owned()])].into(),
        },
        EventTrigger::NewSample,
    ]
}

/// Returns an `EventTrigger` variant's position in `event_triggers`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn event_trigger_index(value: &EventTrigger) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        EventTrigger::Tag { .. } => 0,
        EventTrigger::NewSample => 1,
    }
}

/// One value of every `VolumeTypes` variant, in declaration order
#[must_use]
pub fn volume_types() -> Vec<VolumeTypes> {
    vec![
        VolumeTypes::HostPath,
        VolumeTypes::ConfigMap,
        VolumeTypes::Secret,
        VolumeTypes::NFS,
    ]
}

/// Returns a `VolumeTypes` variant's position in `volume_types`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn volume_type_index(value: &VolumeTypes) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        VolumeTypes::HostPath => 0,
        VolumeTypes::ConfigMap => 1,
        VolumeTypes::Secret => 2,
        VolumeTypes::NFS => 3,
    }
}

/// One value of every `HostPathTypes` variant, in declaration order
#[must_use]
pub fn host_path_types() -> Vec<HostPathTypes> {
    vec![
        HostPathTypes::DirectoryOrCreate,
        HostPathTypes::Directory,
        HostPathTypes::FileOrCreate,
        HostPathTypes::File,
        HostPathTypes::Socket,
        HostPathTypes::CharDevice,
        HostPathTypes::BlockDevice,
    ]
}

/// Returns a `HostPathTypes` variant's position in `host_path_types`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn host_path_type_index(value: &HostPathTypes) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        HostPathTypes::DirectoryOrCreate => 0,
        HostPathTypes::Directory => 1,
        HostPathTypes::FileOrCreate => 2,
        HostPathTypes::File => 3,
        HostPathTypes::Socket => 4,
        HostPathTypes::CharDevice => 5,
        HostPathTypes::BlockDevice => 6,
    }
}

/// One value of every `OutputHandler` variant, in declaration order
#[must_use]
pub fn output_handlers() -> Vec<OutputHandler> {
    vec![OutputHandler::Files]
}

/// Returns an `OutputHandler` variant's position in `output_handlers`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn output_handler_index(value: &OutputHandler) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        OutputHandler::Files => 0,
    }
}

/// One value of every `AutoTagLogic` variant, in declaration order
#[must_use]
pub fn auto_tag_logics() -> Vec<AutoTagLogic> {
    vec![
        AutoTagLogic::Exists,
        AutoTagLogic::Equal(json!("Jupiter")),
        AutoTagLogic::Not(json!("Saturn")),
        AutoTagLogic::Greater(json!(1)),
        AutoTagLogic::GreaterOrEqual(json!(2)),
        AutoTagLogic::LesserOrEqual(json!(3)),
        AutoTagLogic::Lesser(json!(4)),
        AutoTagLogic::In(vec![json!("Io"), json!("Europa")]),
        AutoTagLogic::NotIn(vec![json!("Titan")]),
    ]
}

/// Returns an `AutoTagLogic` variant's position in `auto_tag_logics`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn auto_tag_logic_index(value: &AutoTagLogic) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        AutoTagLogic::Exists => 0,
        AutoTagLogic::Equal(_) => 1,
        AutoTagLogic::Not(_) => 2,
        AutoTagLogic::Greater(_) => 3,
        AutoTagLogic::GreaterOrEqual(_) => 4,
        AutoTagLogic::LesserOrEqual(_) => 5,
        AutoTagLogic::Lesser(_) => 6,
        AutoTagLogic::In(_) => 7,
        AutoTagLogic::NotIn(_) => 8,
    }
}

/// One value of every `SpawnLimits` variant, in declaration order
#[must_use]
pub fn spawn_limits() -> Vec<SpawnLimits> {
    vec![SpawnLimits::Basic(3), SpawnLimits::Unlimited]
}

/// Returns a `SpawnLimits` variant's position in `spawn_limits`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn spawn_limit_index(value: &SpawnLimits) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        SpawnLimits::Basic(_) => 0,
        SpawnLimits::Unlimited => 1,
    }
}

/// One value of every `ImageVersion` variant, in declaration order
#[must_use]
pub fn image_versions() -> Vec<ImageVersion> {
    vec![
        ImageVersion::SemVer(semver::Version::new(1, 2, 3)),
        ImageVersion::Custom("nightly".to_owned()),
    ]
}

/// Returns an `ImageVersion` variant's position in `image_versions`
///
/// # Arguments
///
/// * `value` - The variant to place
#[must_use]
pub fn image_version_index(value: &ImageVersion) -> usize {
    // a new variant makes this match non-exhaustive, so the list can't silently miss it
    match value {
        ImageVersion::SemVer(_) => 0,
        ImageVersion::Custom(_) => 1,
    }
}

/// One guarded enum list, in the form the coverage checks use
pub struct EnumList {
    /// The enum's name, which is also its component name in the `Backup` schema when it has one
    pub name: &'static str,
    /// Whether the enum appears in the `Backup` schema
    pub in_backup: bool,
    /// The serialized name of each listed value, in list order
    pub names: Vec<String>,
    /// The index the guard's `match` gives each listed value, in list order
    pub indexes: Vec<usize>,
    /// The CQL text each listed value is stored as in a text column, when the enum is archived
    pub cql: Option<Vec<String>>,
}

/// Returns a value's serialized variant name: the string of a unit variant, or the key of a
/// variant that carries data
///
/// # Arguments
///
/// * `value` - The value to name
#[must_use]
pub fn variant_name<T: serde::Serialize>(value: &T) -> String {
    // unit variants serialize as strings and data variants as single-key objects
    match serde_json::to_value(value).expect("enum values serialize") {
        serde_json::Value::String(name) => name,
        serde_json::Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
        other => other.to_string(),
    }
}

/// Builds a list's checkable form from its values and guard
///
/// # Arguments
///
/// * `name` - The enum's name
/// * `in_backup` - Whether the enum appears in the `Backup` schema
/// * `values` - The list's values
/// * `index` - The list's guard
/// * `namer` - How a value is named
/// * `cql` - How a value is stored in a text column, for archived enums
fn checked<T>(
    name: &'static str,
    in_backup: bool,
    values: &[T],
    index: fn(&T) -> usize,
    namer: fn(&T) -> String,
    cql: Option<fn(&T) -> String>,
) -> EnumList {
    // name, place, and store each value
    EnumList {
        name,
        in_backup,
        names: values.iter().map(namer).collect(),
        indexes: values.iter().map(index).collect(),
        cql: cql.map(|cql| values.iter().map(cql).collect()),
    }
}

/// Every guarded list, archived enums first
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn all() -> Vec<EnumList> {
    // pair each list with its guard and, for archived enums, its stored form
    vec![
        checked(
            "TagType",
            true,
            &tag_types(),
            tag_type_index,
            variant_name,
            Some(super::classes::cql_text::<TagType>),
        ),
        checked(
            "NodeHealth",
            false,
            &node_healths(),
            node_health_index,
            variant_name,
            Some(super::classes::cql_text::<NodeHealth>),
        ),
        checked(
            "EntityKinds",
            false,
            &entity_kinds(),
            entity_kind_index,
            variant_name,
            Some(super::classes::cql_text::<EntityKinds>),
        ),
        checked(
            "OutputDisplayType",
            true,
            &output_display_types(),
            output_display_type_index,
            variant_name,
            Some(super::classes::cql_text::<OutputDisplayType>),
        ),
        checked(
            "NotificationType",
            false,
            &notification_types(),
            notification_type_index,
            variant_name,
            Some(super::classes::cql_text::<NotificationType>),
        ),
        checked(
            "NotificationLevel",
            false,
            &notification_levels(),
            notification_level_index,
            variant_name,
            Some(super::classes::cql_text::<NotificationLevel>),
        ),
        checked(
            "S3Objects",
            false,
            &s3_objects(),
            s3_object_index,
            |value| format!("{value:?}"),
            Some(super::classes::cql_text::<S3Objects>),
        ),
        checked(
            "AssociationKind",
            false,
            &association_kinds(),
            association_kind_index,
            variant_name,
            Some(super::classes::cql_text::<AssociationKind>),
        ),
        checked(
            "Directionality",
            false,
            &directionalities(),
            directionality_index,
            variant_name,
            Some(super::classes::cql_text::<Directionality>),
        ),
        checked(
            "CommitishKinds",
            false,
            &commitish_kinds(),
            commitish_kind_index,
            variant_name,
            Some(super::classes::cql_text::<CommitishKinds>),
        ),
        checked(
            "UserRole",
            true,
            &user_roles(),
            user_role_index,
            variant_name,
            None,
        ),
        checked(
            "ScopedTokenRole",
            true,
            &scoped_token_roles(),
            scoped_token_role_index,
            variant_name,
            None,
        ),
        checked("Theme", true, &themes(), theme_index, variant_name, None),
        checked(
            "PruneAction",
            true,
            &prune_actions(),
            prune_action_index,
            variant_name,
            None,
        ),
        checked(
            "PrunedReferent",
            true,
            &pruned_referents(),
            pruned_referent_index,
            variant_name,
            None,
        ),
        checked(
            "ImageScaler",
            true,
            &image_scalers(),
            image_scaler_index,
            variant_name,
            None,
        ),
        checked(
            "DependencyPassStrategy",
            true,
            &dependency_pass_strategies(),
            dependency_pass_strategy_index,
            variant_name,
            None,
        ),
        checked(
            "FileNamingStrategy",
            true,
            &file_naming_strategies(),
            file_naming_strategy_index,
            variant_name,
            None,
        ),
        checked(
            "ArgStrategy",
            true,
            &arg_strategies(),
            arg_strategy_index,
            variant_name,
            None,
        ),
        checked(
            "KwargDependency",
            true,
            &kwarg_dependencies(),
            kwarg_dependency_index,
            variant_name,
            None,
        ),
        checked(
            "ImageBanKind",
            true,
            &image_ban_kinds(),
            image_ban_kind_index,
            variant_name,
            None,
        ),
        checked(
            "PipelineBanKind",
            true,
            &pipeline_ban_kinds(),
            pipeline_ban_kind_index,
            variant_name,
            None,
        ),
        checked(
            "EventTrigger",
            true,
            &event_triggers(),
            event_trigger_index,
            variant_name,
            None,
        ),
        checked(
            "VolumeTypes",
            true,
            &volume_types(),
            volume_type_index,
            variant_name,
            None,
        ),
        checked(
            "HostPathTypes",
            true,
            &host_path_types(),
            host_path_type_index,
            variant_name,
            None,
        ),
        checked(
            "OutputHandler",
            true,
            &output_handlers(),
            output_handler_index,
            variant_name,
            None,
        ),
        checked(
            "AutoTagLogic",
            true,
            &auto_tag_logics(),
            auto_tag_logic_index,
            variant_name,
            None,
        ),
        checked(
            "SpawnLimits",
            true,
            &spawn_limits(),
            spawn_limit_index,
            variant_name,
            None,
        ),
        checked(
            "ImageVersion",
            true,
            &image_versions(),
            image_version_index,
            variant_name,
            None,
        ),
    ]
}

/// Returns every rule a guarded list breaks: a length that isn't the enum's variant count,
/// duplicate names, an index that isn't the value's position, and, for `Backup` enums, names that
/// differ from the schema's variants
///
/// # Arguments
///
/// * `list` - The list to check
/// * `schema` - The `Backup` schema
/// * `variants` - How many variants the enum declares
#[must_use]
pub fn list_problems(
    list: &EnumList,
    schema: &super::schema::BackupSchema,
    variants: usize,
) -> Vec<String> {
    // one value per declared variant
    let mut problems = Vec::new();
    if list.names.len() != variants {
        problems.push(format!(
            "{} lists {} values but declares {variants} variants",
            list.name,
            list.names.len()
        ));
    }
    // every name appears once
    let unique: std::collections::BTreeSet<&String> = list.names.iter().collect();
    if unique.len() != list.names.len() {
        problems.push(format!(
            "{} lists a variant twice: {:?}",
            list.name, list.names
        ));
    }
    // each value's guard index is its position, so no variant is skipped or repeated
    if list.indexes != (0..list.indexes.len()).collect::<Vec<_>>() {
        problems.push(format!(
            "{} indexes {:?} aren't the list positions",
            list.name, list.indexes
        ));
    }
    // a Backup enum lists exactly the schema's variants
    if list.in_backup {
        let variants = schema_variants(schema, list.name);
        let mut names = list.names.clone();
        names.sort();
        if variants != names {
            problems.push(format!(
                "{} lists {names:?} but the schema has {variants:?}",
                list.name
            ));
        }
    }
    problems
}

/// Returns the sorted variant names the `Backup` schema gives an enum component
///
/// # Arguments
///
/// * `schema` - The `Backup` schema
/// * `name` - The component's name
#[must_use]
pub fn schema_variants(schema: &super::schema::BackupSchema, name: &str) -> Vec<String> {
    // unit enums list names; enums with data are alternatives labeled by variant
    let mut variants = match schema.component(name) {
        Some(super::schema::Node::Enum(values)) => values,
        Some(super::schema::Node::OneOf(alternatives)) => alternatives
            .into_iter()
            .filter(|(_, node)| !matches!(node, super::schema::Node::Null))
            .flat_map(|(label, node)| match node {
                super::schema::Node::Enum(values) => values,
                _ => vec![label],
            })
            .collect(),
        _ => Vec::new(),
    };
    variants.sort();
    variants
}

/// Returns every enum component of the `Backup` schema
///
/// # Arguments
///
/// * `schema` - The `Backup` schema
#[must_use]
pub fn backup_enums(schema: &super::schema::BackupSchema) -> Vec<String> {
    // a component is an enum when it lists names or alternatives
    schema
        .components
        .iter()
        .filter(|(_, value)| value.get("enum").is_some() || value.get("oneOf").is_some())
        .map(|(name, _)| name.clone())
        .collect()
}
