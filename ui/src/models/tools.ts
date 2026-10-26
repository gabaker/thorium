// project imports
import { AgenticEntity, AgenticKinds } from './agentic';

/// The kinds of tool (the top-level key of a tool spec)
export enum ToolKind {
  /// A workload that runs on a scaler; the settings an image used to hold, regrouped
  Image = 'Image',
  /// A function a model can call
  Agentic = 'Agentic',
  /// The program that runs an AI agent's model and tool loop
  Harness = 'Harness',
}

/// A version of a tool: a semantic version or any other label
export type ToolVersion = { SemVer: string } | { Custom: string };

/// A tool instance as listed on its tool
export interface ToolInstanceSummary {
  /// The tool instance's id
  id: string;
  /// The version label of this instance
  version: ToolVersion;
  /// The content hash of the instance's spec
  spec_hash: string;
  /// Whether this instance is deprecated
  deprecated: boolean;
  /// When this instance was created
  created: string;
}

/// An editable tool whose versions are immutable tool instances
export interface ToolEntity {
  /// The kind of tool this is (fixed for the life of the tool)
  kind: ToolKind;
  /// The newest created instance that isn't deprecated
  latest: string | null;
  /// The version label of the latest instance
  latest_version: ToolVersion | null;
  /// The spec hash of the latest instance
  latest_spec_hash: string | null;
  /// When this tool was last modified
  modified: string;
  /// Every instance of this tool, newest first (left out by the API when there are none)
  instances?: ToolInstanceSummary[];
  /// The settings a harness's pod env holds (Harness tools only)
  harness_env?: Record<string, unknown> | null;
  /// Exact model ids no run of this harness may resolve to (Harness tools only)
  disabled_models?: string[];
}

/// A single immutable version of a tool
export interface ToolInstanceEntity {
  /// The tool this is a version of
  tool: string;
  /// The version label of this instance
  version: ToolVersion;
  /// The instance this one follows
  previous: string | null;
  /// The content hash of the spec
  spec_hash: string;
  /// Whether this instance is deprecated
  deprecated: boolean;
  /// The spec, keyed by the tool kind (`{ Image: ... }`, `{ Agentic: ... }`, or `{ Harness: ... }`)
  spec: Record<string, unknown>;
  /// How portable an agentic spec is across model APIs
  portability?: { strict_compatible: boolean; strict_issues: string[]; gemini_compatible: boolean; gemini_rewrites: string[] } | null;
  /// The native agentic tool instances this harness instance provides
  provides?: string[];
}

/// A tool entity
export type Tool = AgenticEntity<AgenticKinds.Tool, ToolEntity>;

/// A tool instance entity
export type ToolInstance = AgenticEntity<AgenticKinds.ToolInstance, ToolInstanceEntity>;

/// The fields of the form that creates a tool and its first version
export interface ToolCreateForm {
  /// The tool's name
  name: string;
  /// The groups to create the tool in
  groups: string[];
  /// A description of the tool
  description: string;
  /// The version label of the first instance
  version: string;
  /// The JSON encoded spec of the first instance
  spec: string;
  /// The kind of tool being created
  kind: ToolKind;
}

/// The fields of the form that publishes a new version of a tool
export interface ToolPublishForm {
  /// The id of the tool
  tool: string;
  /// The version label of the new instance
  version: string;
  /// The JSON encoded spec of the new instance
  spec: string;
}
