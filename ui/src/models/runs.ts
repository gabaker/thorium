// project imports
import type { GenericJobArgs, RepoDependencyRequest } from './reactions';
import type { ImageVersion } from './images';
import { ToolKind } from './tools';

/// The inputs to a workflow or tool run (at least one file or repo)
export interface RunInputs {
  /// Specific files to run on, by SHA256
  samples: string[];
  /// Specific repos (and optional commitish) to run on
  repos: RepoDependencyRequest[];
}

/// A request to run a single tool instance on one or more inputs
export interface ToolRunRequest {
  /// The files and repos to run the tool on
  inputs: RunInputs;
  /// The group to run in
  group?: string | null;
  /// The args for the tool's job
  args?: GenericJobArgs;
  /// The reaction this run is a sub reaction of
  parent?: string | null;
  /// The tags to add to the run's reaction
  tags?: string[];
  /// The SLA for this run in seconds
  sla?: number | null;
}

/// A request to run a workflow instance on one or more inputs
export interface WorkflowRunRequest {
  /// The files and repos to run the workflow on
  inputs: RunInputs;
  /// The group to run in
  group?: string | null;
  /// The tags to add to the run's reaction
  tags?: string[];
  /// The SLA for this run in seconds
  sla?: number | null;
}

/// One agent of a workflow run and the hashes of its configuration and tool surface
export interface AgentProvenance {
  /// The agent's id
  agent: string;
  /// The agent's name at instancing time
  name: string;
  /// The hash of the agent's configuration
  config_hash: string;
  /// The digest of the agent's effective tools
  toolset_digest: string;
}

/// One tool instance a run runs or may call
export interface ToolProvenance {
  /// The id of the tool this is an instance of
  tool: string;
  /// The id of the tool instance
  tool_instance: string;
  /// The name of the tool instance
  name: string;
  /// The kind of tool this is
  kind: ToolKind;
  /// The version label
  version: string;
  /// The content hash of the tool instance
  spec_hash: string;
  /// The container image as written in the spec
  image?: string | null;
}

/// The exact versions a reaction was created with
export interface RunProvenance {
  /// Every agent of a workflow run
  agents: AgentProvenance[];
  /// Every tool instance the run runs or may call
  tools: ToolProvenance[];
}

/// The model, endpoint, and credential one agent of a workflow run resolved to
export interface ReactionModelBinding {
  /// The agent this binding is for
  agent: string;
  /// The position of the winning matcher in the agent's model list
  priority?: number | null;
  /// The level whose auth was used
  level: string;
  /// The endpoint the model is called on
  endpoint: Record<string, string>;
  /// The provider's exact model id
  model_id: string;
  /// The protocol the model is called with
  api: string;
  /// The profile whose parameters apply
  profile?: string | null;
  /// The credential used
  credential?: Record<string, string> | null;
}

/// The workflow instance a reaction is running
export interface ReactionWorkflowRun {
  /// The workflow instance being run
  workflow_instance: string;
  /// The workflow instance's digest at run time
  digest: string;
  /// The entry agent
  agent: string;
  /// The harness tool instance running the agent
  harness: string;
  /// The result tool name the run's results are stored under
  result_key: string;
  /// The model each agent resolved to
  models: ReactionModelBinding[];
  /// The exact versions of the agents and tools
  provenance: RunProvenance;
}

/// What a reaction runs, externally tagged
export type ReactionSource =
  | { Pipeline: { name: string; provenance: RunProvenance } }
  | { Tool: { tool_instance: string; provenance: RunProvenance } }
  | { WorkflowRun: ReactionWorkflowRun };

/// A single stage of a reaction's plan
export interface PlannedStage {
  /// The stage name that args, logs, and progress are keyed by
  name: string;
  /// The tool instance this stage's jobs run
  tool_instance: string;
}

/// A reaction's stages resolved when it was created
export interface ReactionPlan {
  /// The stages in order; stages in an inner list run in parallel
  stages: PlannedStage[][];
}

/// The kinds of run
export enum RunKind {
  /// A single tool instance run
  Tool = 'Tool',
  /// A workflow instance run
  Workflow = 'Workflow',
  /// A pipeline reaction
  Pipeline = 'Pipeline',
}

/// The response from starting a run
export interface RunCreated {
  /// The reaction id of the run
  id: string;
}

/// One result of a tool or workflow on an input
export interface RunOutput {
  /// The result's id
  id: string;
  /// The result itself
  result: unknown;
  /// The result files (left out by the API when there are none)
  files?: string[];
  /// When the result was created
  uploaded?: string;
  /// The tool version
  tool_version?: ImageVersion | null;
}

/// A tool-keyed set of results of one input
export interface RunResults {
  /// The results by tool name
  results: Record<string, RunOutput[]>;
}
