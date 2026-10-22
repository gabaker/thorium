//! Wrappers for all objects within Thorium

mod associations;
mod bans;
pub mod conversions;
pub mod cursors;
pub mod deadlines;
pub mod elastic;
pub mod entities;
mod errors;
pub mod events;
pub mod files;
pub mod git;
pub mod graphics;
pub mod grep;
pub mod groups;
pub mod helpers;
pub mod images;
pub mod jobs;
pub mod logs;
pub mod network_policies;
pub mod notifications;
mod oauth;
pub mod pipelines;
pub mod reactions;
pub mod requisitions;
pub mod results;
pub mod runtime;
#[cfg(feature = "scylla-utils")]
pub mod schema;
mod scylla_utils;
pub mod search;
pub mod streams;
pub mod system;
pub mod tags;
mod trees;
#[cfg(any(feature = "api", feature = "client"))]
pub mod upgrades;
pub mod users;
mod version;
mod volumes;

pub use associations::{
    AssociatedSubmissions, Association, AssociationDeleteRequest, AssociationKind,
    AssociationListOpts, AssociationListParams, AssociationRequest, AssociationSupport,
    AssociationTarget,
};
pub use deadlines::Deadline;
pub use elastic::{ElasticDoc, ElasticIndex, ElasticSearchOpts, ElasticSearchParams};
pub use entities::agents::{
    AgentConfig, AgentEntity, AgentEntityRequest, AgentUpdateFields, ToolChoice,
};
pub use entities::collections::{CollectionEntity, CollectionEntityRequest, CollectionKind};
pub use entities::countries::Country;
pub use entities::devices::{DeviceEntity, DeviceEntityRequest};
pub use entities::filesystem::{FileSystemEntity, FileSystemEntityBuilder, FileSystemFolderEntity};
pub use entities::flags::{Confidence, Flag};
pub use entities::functions::{CompiledFunction, CompiledInstruction, DecompiledFunction};
pub use entities::incident::{Incident, IncidentRequest};
pub use entities::models::{
    AWS_CONFIG_FILE, AWS_PROFILE, AgentModelMismatch, CREDENTIALS_DIR, HarnessCredential,
    HarnessModelEntry, HarnessModels, MismatchNames, ModelCandidateRejection, ModelRejectReason,
    ModelResolutionError, RunModels,
};
pub use entities::models::{
    AwsRoleSource, AzureApiMode, AzureClientAuth, BedrockEndpointKind, CatalogEntrySource,
    CredentialKind, CredentialSecrets, CredentialType, GeminiApiVersion, LimitsSource, ListedModel,
    MAX_LISTED_MODELS, ModelApi, ModelAuthLevel, ModelAuthSource, ModelBan, ModelBanKind,
    ModelBanUpdate, ModelCapabilities, ModelCatalogEntry, ModelCatalogEntryRequest, ModelConfigRef,
    ModelCredentialEntity, ModelCredentialEntityRequest, ModelEndpointEntity,
    ModelEndpointEntityRequest, ModelLimits, ModelMatcher, ModelParameters, ModelProfileEntity,
    ModelProfileEntityRequest, ModelProvider, ModelProviderKind, ModelRefreshReport,
    ModelRefreshState, ModelRefreshStatus, ModelRequirements, ModelSecret, RedactSecrets,
    ScopedModelBan, VertexLocation, VertexMultiRegion,
};
pub use entities::pe::{PeImportEntity, PeSectionEntity};
pub use entities::processes::{WindowsProcessEntity, WindowsProcessTreeEntity};
pub use entities::rules::{SigmaRule, SigmaRuleAppliesTo};
pub use entities::shared::CriticalSector;
#[cfg(any(feature = "api", feature = "client"))]
pub use entities::tools::tool_portability_json;
pub use entities::tools::{
    AgenticTool, AgenticToolBacking, ApprovalMode, ArgTarget, BindingCheck, ContainerToolBacking,
    DEFAULT_TOOL_RESULTS_DIR, HarnessAgentFile, HarnessArgs, HarnessCapabilities, HarnessEnv,
    HarnessEnvVar, HarnessInputs, HarnessInterface, HarnessOutputs, HarnessRunFile, HarnessSpec,
    HarnessToolAccess, HarnessToolEntry, HarnessToolPolicy, MappedToolInput, McpConfigFormat,
    McpConfigTarget, OrphanedRunPolicy, ToolAnnotations, ToolApproval, ToolCapabilities,
    ToolEffect, ToolEntity, ToolEntityRequest, ToolExposure, ToolIcon, ToolIconTheme,
    ToolInputMapping, ToolInstanceEntity, ToolInstanceEntityRequest, ToolInstanceSummary,
    ToolInvocation, ToolKind, ToolLoading, ToolMechanism, ToolOutputDelivery,
    ToolPortabilityReport, ToolResultFormat, ToolServerAuth, ToolServerDescriptor, ToolServerInput,
    ToolServerTimeouts, ToolServerToolOverride, ToolServerTools, ToolServerTransport,
    ToolServerTrust, ToolServerValue, ToolServersFile, ToolSpec, ToolStrictness, ToolUpdateFields,
    ToolVersion, effective_max_result_bytes, is_valid_server_id,
};
pub use entities::toolsets::{
    SelectorMatches, ToolSetEntity, ToolSetEntityRequest, ToolSetExclusion, ToolSetExclusionReason,
    ToolSetInclusion, ToolSetInstanceEntity, ToolSetInstanceEntityRequest, ToolSetInstanceStatus,
    ToolSetInstanceTrigger, ToolSetLimits, ToolSetMember, ToolSetSelector, ToolSetUpdateFields,
};
pub use entities::vendors::{VendorEntity, VendorEntityRequest};
pub use entities::workflows::{
    CrawlLimits, InstancedAgent, InstancedAgentToolset, InstancedTool, InstancedToolBinding,
    InstancedToolSpec, WorkflowCatalogEntity, WorkflowCatalogEntityRequest,
    WorkflowCatalogUpdateFields, WorkflowInstanceEntity, WorkflowInstanceEntityRequest,
    WorkflowInstanceStatus, WorkflowInstanceTrigger, WorkflowLimits, WorkflowResultKey,
};
pub use entities::{
    Entity, EntityKinds, EntityListLine, EntityListOpts, EntityListParams, EntityMetadata,
    EntityMetadataRequest, EntityMetadataUpdate, EntityParentInfo, EntityRequest, EntityResponse,
    EntityUpdate, GroupPolicy,
};
pub use errors::InvalidEnum;
pub use events::{
    Event, EventCacheStatus, EventCacheStatusOpts, EventData, EventIds, EventMarks, EventPeekOpts,
    EventPopOpts, EventRequest, EventTrigger, EventType, ModelRefreshEvent, ModelRefreshReason,
    ModelRefreshTarget, SigmaScannableResultsEvent, TriggerPotential, WorkflowGraphChange,
    WorkflowGraphEvent,
};
pub use files::{
    Attachment, Buffer, CartedFile, CarvedOrigin, CarvedOriginTypes, Comment, CommentRequest,
    CommentResponse, DeleteCommentParams, DeleteSampleParams, DownloadedFile, FileDeleteOpts,
    FileDownloadOpts, FileListOpts, FileListParams, Origin, OriginRequest, OriginTypes,
    PcapNetworkProtocol, Sample, SampleCheck, SampleCheckResponse, SampleListLine, SampleRequest,
    SampleSubmissionResponse, Submission, SubmissionChunk, SubmissionUpdate, Tag, TagMap,
    ZipDownloadParams,
};
pub use git::{
    Branch, BranchDetails, BranchRequest, Commit, CommitDetails, CommitListOpts, CommitRequest,
    Commitish, CommitishDetails, CommitishKinds, CommitishListParams, CommitishMapRequest,
    CommitishRequest, GitTag, GitTagDetails, GitTagRequest, Repo, RepoCheckout, RepoCreateResponse,
    RepoDataUploadResponse, RepoDependency, RepoDependencyRequest, RepoDownloadOpts, RepoListLine,
    RepoListOpts, RepoListParams, RepoRequest, RepoScheme, RepoSubmission, RepoSubmissionChunk,
    RepoUrlComponents, TarredRepo,
};
pub use groups::{
    Group, GroupAllowAction, GroupAllowed, GroupAllowedUpdate, GroupDetailsList, GroupList,
    GroupListParams, GroupMap, GroupRequest, GroupStats, GroupUpdate, GroupUsers,
    GroupUsersRequest, GroupUsersUpdate, Roles,
};
pub use images::{
    ArgStrategy, BurstableResources, BurstableResourcesRequest, BurstableResourcesUpdate,
    CacheDependencySettings, CacheDependencySettingsUpdate, ChildFilters, ChildFiltersUpdate,
    ChildrenDependencySettings, ChildrenDependencySettingsUpdate, Cleanup, CleanupUpdate,
    Dependencies, DependenciesUpdate, DependencyPassStrategy, EphemeralDependencySettings,
    EphemeralDependencySettingsUpdate, FileNamingStrategy, FileSystemDependencySettings,
    FileSystemDependencySettingsUpdate, GenericCacheDependencySettings,
    GenericCacheDependencySettingsUpdate, Image, ImageArgs, ImageArgsUpdate, ImageBan,
    ImageBanKind, ImageBanUpdate, ImageDetailsList, ImageJobInfo, ImageLifetime, ImageList,
    ImageListParams, ImageNetworkPolicyUpdate, ImageRequest, ImageScaler, ImageUpdate, Kvm,
    KvmUpdate, KwargDependency, RepoDependencySettings, RepoDependencySettingsUpdate, Resources,
    ResourcesRequest, ResourcesUpdate, ResultDependencySettings, ResultDependencySettingsUpdate,
    SampleDependencySettings, SampleDependencySettingsUpdate, SecurityContext,
    SecurityContextUpdate, SpawnLimits, TagDependencySettings, TagDependencySettingsUpdate,
};
pub use jobs::{
    Checkpoint, GenericJob, GenericJobArgs, GenericJobArgsUpdate, GenericJobKwargs, GenericJobOpts,
    HandleJobResponse, JobDetailsList, JobHandleParams, JobHandleStatus, JobList, JobListOpts,
    JobResetRequestor, JobResets, JobStatus, MAX_CLAIM_LIMIT, RawJob, RunningJob, WorkloadRuntimes,
};
pub use logs::{Actions, JobActions, ReactionActions, StatusRequest, StatusUpdate};
pub use network_policies::{
    IpBlock, IpBlockRaw, Ipv4Block, Ipv6Block, K8S_NAME_MAX_CHARS, NO_TOOL_LABEL_VALUE,
    NetworkPolicy, NetworkPolicyCustomK8sRule, NetworkPolicyCustomLabel, NetworkPolicyListLine,
    NetworkPolicyListOpts, NetworkPolicyListParams, NetworkPolicyPort, NetworkPolicyRequest,
    NetworkPolicyRule, NetworkPolicyRuleRaw, NetworkPolicyUpdate, NetworkProtocol, ToolIdsByName,
    k8s_label, k8s_name,
};
pub use oauth::{
    OAuthCallbackParams, OAuthLinkParams, OAuthMaybeAuthed, OAuthRegistrationSessionId,
    OAuthUserCreate, OAuthUsernameCheck,
};
pub use pipelines::{
    DEFAULT_PIPELINE_SLA, Pipeline, PipelineBan, PipelineBanKind, PipelineBanUpdate,
    PipelineDetailsList, PipelineList, PipelineListParams, PipelineRequest, PipelineStats,
    PipelineUpdate, StageStats,
};
pub use reactions::{
    AgentProvenance, BulkReactionResponse, HandleReactionResponse, PlannedStage, Reaction,
    ReactionArgs, ReactionCache, ReactionCacheFileUpdate, ReactionCacheUpdate, ReactionCreation,
    ReactionCursorOpts, ReactionCursorParams, ReactionDetailsList, ReactionExpire,
    ReactionIdResponse, ReactionList, ReactionListParams, ReactionModelBinding, ReactionPlan,
    ReactionRequest, ReactionSource, ReactionStatus, ReactionUpdate, ReactionWorkflowRun,
    RunInputs, RunProvenance, StageLogLine, StageLogs, StageLogsAdd, TOOL_RUN_POLL_MAX_SECS,
    TOOL_RUN_POLL_START_SECS, ToolProvenance, ToolRunRequest, WorkflowRunRequest,
    tool_run_retry_after,
};
pub use requisitions::{
    RUN_LABEL, Requisition, ScopedRequisition, SpawnedUpdate, TOOL_INSTANCE_LABEL, TOOL_LABEL,
};
pub use results::{
    AutoTag, AutoTagLogic, AutoTagUpdate, FilesHandler, FilesHandlerUpdate, OnDiskFile, Output,
    OutputChunk, OutputCollection, OutputCollectionUpdate, OutputDisplayType, OutputHandler,
    OutputKey, OutputResponse, ResultGetParams,
};
pub use runtime::{
    BareMetalEnvironment, DroppedImageField, ExecutionSettings, ImageConversion, JobDataSettings,
    K8sEnvironment, KvmEnvironment, SchedulingSettings, ToolChildrenDependencySettings,
    ToolDependencies, ToolFileSystemDependencySettings, ToolResultDependencySettings,
    WindowsEnvironment, WorkloadEnvironment, WorkloadSpec,
};
pub use search::events::{
    ResultSearchEvent, SearchEvent, SearchEventPopOpts, SearchEventStatus, SearchEventType,
    TagSearchEvent,
};
pub use streams::{Stream, StreamDepth, StreamObj};
pub use system::{
    ActiveJob, AllowedEndpoint, Backup, BackupParams, BackupProblem, BackupProblems,
    HostPathWhitelistUpdate, ModelsConf, ModelsConfUpdate, NetworkPolicySet, Node, NodeGetParams,
    NodeHealth, NodeListLine, NodeListParams, NodeRegistration, NodeUpdate, Pools, PruneAction,
    PrunedReference, PrunedReferent, RestoreParams, ScalerStats, SpawnMap, StreamerInfoUpdate,
    SystemComponents, SystemInfo, SystemInfoParams, SystemSettings, SystemSettingsResetParams,
    SystemSettingsUpdate, SystemSettingsUpdateParams, SystemStats, Worker, WorkerDelete,
    WorkerDeleteMap, WorkerList, WorkerRegistration, WorkerRegistrationList, WorkerStatus,
    WorkerUpdate,
};
pub use tags::{TagCounts, TagKeyCounts};
pub use trees::{
    Directionality, Tree, TreeBounds, TreeBranch, TreeGrowQuery, TreeNode, TreeOpts, TreeParams,
    TreeQuery, TreeRelatedQuery, TreeRelationships, TreeSupport,
};
pub use users::{
    AiEndpoint, AiEndpointUpdate, AiSettings, AiSettingsUpdate, AuthResponse, AuthedUser, Key,
    RunTokenScope, ScopedToken, ScopedTokenRequest, ScopedTokenRole, ScopedTokenUpdate, ScopedUser,
    ScrubbedUser, Theme, UnixInfo, User, UserCreate, UserCredentialRef, UserModelEndpoint,
    UserModelSettings, UserModelSettingsUpdate, UserRole, UserSettings, UserSettingsUpdate,
    UserUpdate,
};
pub use version::{Arch, Component, Os, Version};
pub use volumes::{ConfigMap, HostPath, HostPathTypes, NFS, Secret, Volume, VolumeTypes};

// optional imports
pub mod backends;

// client feature reexports
cfg_if::cfg_if! {
    if #[cfg(feature = "client")] {
        pub use git::UntarredRepo;
        pub use cursors::{Cursor, DateOpts, CountCursor, CountCursorSupport};
        pub use files::UncartedFile;
        pub use grep::{GrepOpts, GrepPage};
    }
}

// sync client feature reexports
cfg_if::cfg_if! {
    if #[cfg(all(feature = "client", feature = "sync"))] {
        pub use cursors::{CursorBlocking, CountCursorBlocking};
    }
}

// api feature exports
cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        pub use cursors::{ApiCursor, CursorMetadataSupport, NoCursorMetadata};
        pub use reactions::{RawGenericJobArgs, RawReactionRequest};
        pub use files::{SampleForm, OriginForm, CommentForm};
        pub use entities::{EntityForm, EntityMetadataUpdateForm, EntityUpdateForm,EntityMetadataForm};
        pub use git::RepoDataForm;
        pub use jobs::JobReactionIds;
        pub use backends::results::ResultFileDownloadParams;
        pub(crate) use backends::search::events::SearchEventBackend;
        pub use trees::{UnhashedTreeBranch, TreeTags};
    }
}

// api/client reexports
cfg_if::cfg_if! {
    if #[cfg(any(feature = "api", feature = "client"))] {
        pub use tags::{TagDeleteRequest, TagRequest, TagType};
        pub use notifications::{
            Notification, NotificationLevel, NotificationParams, NotificationRequest, NotificationType,
        };
        pub use results::{OutputRequest, OutputKind, OutputMap};
        pub use grep::{
            GrepDocumentMatches, GrepMatch, GrepMatches, GrepParams, GrepQueryParams, GrepRequest,
            GrepSource, GrepStats, TruncationReason,
        };
        pub use entities::digests::{DigestTag, DigestValue, ThorDigest};
    }
}

// scylla feature reexports
cfg_if::cfg_if! {
    if #[cfg(feature = "scylla-utils")] {
        mod census;

        pub use scylla_utils::associations::{AssociationListRow, AssociationTargetColumn, ListableAssociation};
        pub use scylla_utils::repos::{
            CommitishListRow, RepoRow,
            RepoListRow, CommitData, BranchData, GitTagData, RepoAuthRow, RepoDataHashRow,
            CommitishDataRow, CommitishRepoDataRow,
        };
        pub use scylla_utils::entities::{
            EntityGroupKindRow, EntityGroupRow, EntityListRow, EntityListSupplementRow, EntityRow,
        };
        pub use scylla_utils::files::{
            BasicSubmissionRow, CommentIdRow, CommentRow, S3IdRow, Sha256Row, SubmissionListRow,
            SubmissionRow,
        };
        pub use scylla_utils::results::{
            OutputId, OutputIdRow, OutputRow, OutputFormBuilder, OutputForm, ResultIdRow,
            ResultUploadedRow,
        };
        pub use scylla_utils::system::{NodeRow, WorkerName};
        pub use scylla_utils::logs::{LogLineRow, NotificationBackupRow, NotificationRow};
        pub use scylla_utils::tags::{TagRow, FullTagRow, TagListRow, TagBackupRow};
        pub use scylla_utils::s3::S3Objects;
        pub use scylla_utils::network_policies::{
            NetworkPolicyGroupRow, NetworkPolicyListRow, NetworkPolicyRow,
        };
        pub use census::{CensusSupport, CensusKeys};
        pub use tags::TagCensusCaseInsensitive;

        #[cfg(feature = "rkyv-support")]
        pub use scylla_utils::s3::ArchivedS3Objects;
    }
}

// scylla keys needed for the client
cfg_if::cfg_if! {
    if #[cfg(any(feature = "scylla-utils", feature = "client"))] {
        pub use scylla_utils::keys::{KeySupport, PipelineKey, ImageKey};
    }
}

// python mappings
cfg_if::cfg_if! {
    if #[cfg(feature = "python")] {
        pub mod python;

        pub use files::{SamplePy, SubmissionChunkPy, OriginPy, CarvedOriginPy };
    }
}
