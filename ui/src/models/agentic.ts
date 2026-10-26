/// The entity kinds that make up Thorium's tools structure (api/src/models/entities.rs)
export enum AgenticKinds {
  /// An editable tool whose versions are tool instances
  Tool = 'Tool',
  /// A single immutable version of a tool
  ToolInstance = 'ToolInstance',
  /// A configured AI agent
  Agent = 'Agent',
  /// A named group of agentic tools that agents attach as one unit
  ToolSet = 'ToolSet',
  /// A constant snapshot of a toolset's resolved members
  ToolSetInstance = 'ToolSetInstance',
  /// The editable root that wires agents into a workflow
  WorkflowCatalog = 'WorkflowCatalog',
  /// A constant, fully resolved snapshot of a workflow catalog's graph
  WorkflowInstance = 'WorkflowInstance',
  /// A connection to a model provider and the models it serves
  ModelEndpoint = 'ModelEndpoint',
  /// A named way to call one model on one endpoint
  ModelProfile = 'ModelProfile',
  /// A credential model endpoints authenticate with
  ModelCredential = 'ModelCredential',
}

/// The tags of an entity: tag key to tag value to the groups the tag is visible in
export type TagMap = Record<string, Record<string, string[]>>;

/// An entity of any agentic kind, with its metadata left untyped; used to constrain generic helpers
export interface AnyAgenticEntity {
  /// The entity's unique ID
  id: string;
  /// The name of this entity
  name: string;
  /// The kind of entity this is
  kind: AgenticKinds;
  /// The metadata for this specific entity kind, keyed by the kind name
  metadata: Partial<Record<AgenticKinds, unknown>>;
  /// A description of this entity
  description: string | null;
  /// The user that submitted this entity
  submitter: string;
  /// The groups this entity was submitted to
  groups: string[];
  /// The tags for this entity: key to value to the groups the tag is visible in
  tags: TagMap;
  /// This entity's image
  image: string | null;
  /// The time this entity was created
  created: string;
}

/// An entity of one agentic kind; `metadata` is externally tagged by the kind's name
export interface AgenticEntity<K extends AgenticKinds, M> {
  /// The entity's unique ID
  id: string;
  /// The name of this entity
  name: string;
  /// The kind of entity this is
  kind: K;
  /// The metadata for this specific entity kind, keyed by the kind name
  metadata: K extends unknown ? { [key in K]: M } : never;
  /// A description of this entity
  description: string | null;
  /// The user that submitted this entity
  submitter: string;
  /// The groups this entity was submitted to
  groups: string[];
  /// The tags for this entity: key to value to the groups the tag is visible in
  tags: TagMap;
  /// This entity's image
  image: string | null;
  /// The time this entity was created
  created: string;
}

/// The response from an entity creation
export interface EntityCreateResponse {
  /// The ID of the created entity
  id: string;
  /// The name of this entity
  name: string;
  /// The ID of the first tool instance created with a tool
  tool_instance?: string | null;
  /// How portable a created agentic tool instance's spec is across model APIs
  portability?: ToolPortabilityReport | null;
}

/// How portable an agentic tool's schema is across model APIs
export interface ToolPortabilityReport {
  /// Whether the spec satisfies strict structured-output mode as written
  strict_compatible: boolean;
  /// Why the spec is not strict compatible
  strict_issues: string[];
  /// Whether the spec works with Gemini as written
  gemini_compatible: boolean;
  /// The rewrites Gemini needs
  gemini_rewrites: string[];
}

/// How serious a notification is
export enum NotificationLevel {
  /// An informational notification
  Info = 'Info',
  /// A warning
  Warn = 'Warn',
  /// An error, such as a ban
  Error = 'Error',
}

/// A notification on an entity, image, or pipeline (api/src/models/notifications.rs)
export interface EntityNotification {
  /// The key of the object this notification is for
  key: string | Record<string, unknown>;
  /// When the notification was created
  created: string;
  /// The notification's unique id
  id: string;
  /// The notification message
  msg: string;
  /// How serious the notification is
  level: NotificationLevel;
  /// The id of the ban that caused the notification, if any
  ban_id?: string | null;
}

/// A page of entities from a list route
export interface AgenticPage<T> {
  /// The entities on this page
  entities: T[];
  /// The cursor for the next page, or null when exhausted
  cursor: string | null;
}
