// project imports
import { AgenticEntity, AgenticKinds, AnyAgenticEntity } from './agentic';

/// A tag selector that adds every agentic tool carrying the tag
export interface ToolSetSelector {
  /// The tag key
  key: string;
  /// The tag value
  value: string;
}

/// How a member got into a toolset instance
export enum ToolSetInclusion {
  /// Explicitly included, following the tool's latest instance
  ExplicitLatest = 'ExplicitLatest',
  /// Explicitly included, pinned to one tool instance
  ExplicitPinned = 'ExplicitPinned',
  /// Matched by the selector
  Selector = 'Selector',
}

/// Why a tool was left out of a toolset instance
export enum ToolSetExclusionReason {
  /// The tool declares effects and the toolset is read only
  NotReadOnly = 'NotReadOnly',
  /// Only agentic tools can be in a toolset
  NotAgentic = 'NotAgentic',
  /// The tool has no instance that isn't deprecated
  NoLatest = 'NoLatest',
  /// The selector's tag is missing in some of the toolset's groups
  NotTaggedInAllGroups = 'NotTaggedInAllGroups',
}

/// Whether a toolset instance can be used
export type InstanceStatus = 'Ready' | { Invalid: { errors: string[] } };

/// An editable toolset
export interface ToolSetEntity {
  /// Only keep tools whose version declares no effects
  read_only: boolean;
  /// The tag selector, if any
  selector: ToolSetSelector | null;
  /// Whether new instances are built automatically
  auto_instance: boolean;
  /// The explicitly included tools (populated on read): a Tool follows its latest instance, a ToolInstance is pinned
  tools: AnyAgenticEntity[];
  /// When this toolset was last modified
  modified: string;
  /// The latest Ready instance
  latest_instance: string | null;
  /// The highest revision built so far
  highest_revision: number;
}

/// One resolved member of a toolset instance
export interface ToolSetMember {
  /// The tool
  tool: string;
  /// The tool instance
  tool_instance: string;
  /// The tool's name
  name: string;
  /// The tool's groups
  groups: string[];
  /// The version label
  version: string;
  /// The spec hash
  spec_hash: string;
  /// The function name models call the tool by
  function_name: string;
  /// How the member got in
  inclusion: ToolSetInclusion;
}

/// A tool left out of a toolset instance
export interface ToolSetExclusion {
  /// The tool
  tool: string;
  /// The tool instance, if one was found
  tool_instance: string | null;
  /// The tool's name
  name: string;
  /// How the tool was meant to get in
  inclusion: ToolSetInclusion;
  /// Why it was left out
  reason: ToolSetExclusionReason;
  /// The groups the selector's tag is missing in
  missing_groups: string[];
}

/// A constant snapshot of a toolset's resolved members
export interface ToolSetInstanceEntity {
  /// The toolset this is an instance of
  tool_set: string;
  /// The toolset's groups when built
  tool_set_groups: string[];
  /// The revision of this instance
  revision: number;
  /// The instance this one follows
  previous: string | null;
  /// Whether the instance can be used
  status: InstanceStatus;
  /// Warnings found while building
  warnings: string[];
  /// What caused the build
  trigger: unknown;
  /// The user whose change caused the build
  triggered_by: string;
  /// Whether the read only filter was on
  read_only: boolean;
  /// The selector at build time
  selector: ToolSetSelector | null;
  /// The resolved members
  members: ToolSetMember[];
  /// The tools left out
  excluded: ToolSetExclusion[];
  /// The content digest
  digest: string;
}

/// A toolset entity
export type ToolSet = AgenticEntity<AgenticKinds.ToolSet, ToolSetEntity>;

/// A toolset instance entity
export type ToolSetInstance = AgenticEntity<AgenticKinds.ToolSetInstance, ToolSetInstanceEntity>;

/// The form that creates a toolset
export interface ToolSetCreateForm {
  /// The toolset's name
  name: string;
  /// The groups to create it in
  groups: string[];
  /// A description
  description: string;
  /// The tool ids to include (following latest)
  tools: string[];
  /// The selector key, or an empty string for none
  selectorKey: string;
  /// The selector value
  selectorValue: string;
  /// Whether the read only filter is on
  readOnly: boolean;
  /// Whether new instances are built automatically
  autoInstance: boolean;
}
