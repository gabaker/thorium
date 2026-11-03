// project imports
import { AgenticEntity, AgenticKinds } from './agentic';

/// A ban on a model endpoint, profile, or credential
export interface ModelBan {
  /// The ban's id
  id: string;
  /// When the ban was made
  time_banned: string;
  /// What kind of ban this is
  ban_kind: Record<string, unknown> | string;
}

/// The token limits of a model
export interface ModelLimits {
  /// The max number of input tokens the model accepts
  context_window?: number | null;
  /// The max number of tokens the model can produce in one response
  max_output_tokens?: number | null;
}

/// What a model can do, by capability name
export type ModelCapabilities = Record<string, boolean>;

/// One model an endpoint serves
export interface ModelCatalogEntry {
  /// The provider's exact model id
  model_id: string;
  /// The model's token limits
  limits?: ModelLimits | null;
  /// What the model can do
  capabilities?: ModelCapabilities | null;
  /// The bans on this catalog model by id
  bans?: Record<string, ModelBan>;
}

/// A connection to a model provider and the models it serves
export interface ModelEndpointEntity {
  /// The provider and its settings
  provider: Record<string, unknown>;
  /// The credential the endpoint authenticates with
  credential: string | null;
  /// The models the endpoint serves
  catalog: ModelCatalogEntry[];
  /// The bans on this endpoint by id
  bans: Record<string, ModelBan>;
  /// The state of the last catalog refresh
  refresh: { state: string; requested?: string | null; finished?: string | null; error?: string | null } | string;
  /// Model ids that are disabled
  disabled_models: string[];
  /// When this endpoint was last modified
  modified: string;
}

/// A named way to call one model on one endpoint
export interface ModelProfileEntity {
  /// The endpoint the model is called on
  endpoint: string;
  /// The provider's exact model id
  model_id: string;
  /// The protocol the model is called with
  api: string;
  /// The sampling parameters
  parameters: Record<string, unknown>;
  /// The limits of the profile
  limits: ModelLimits | null;
  /// The declared capabilities
  capabilities: ModelCapabilities | null;
  /// The bans on this profile by id
  bans: Record<string, ModelBan>;
  /// When this profile was last modified
  modified: string;
}

/// A credential model endpoints authenticate with (secrets are never shown)
export interface ModelCredentialEntity {
  /// What kind of credential this is
  kind: Record<string, unknown> | string;
  /// The non-secret trace of the credential's secret
  secrets?: { fingerprint?: string | null };
  /// When the credential expires
  expires: string | null;
  /// When the credential was last rotated
  rotated: string | null;
  /// The bans on this credential by id
  bans: Record<string, ModelBan>;
  /// When this credential was last modified
  modified: string;
}

/// A model endpoint entity
export type ModelEndpoint = AgenticEntity<AgenticKinds.ModelEndpoint, ModelEndpointEntity>;

/// A model profile entity
export type ModelProfile = AgenticEntity<AgenticKinds.ModelProfile, ModelProfileEntity>;

/// A model credential entity
export type ModelCredential = AgenticEntity<AgenticKinds.ModelCredential, ModelCredentialEntity>;
