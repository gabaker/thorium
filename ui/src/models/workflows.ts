// project imports
import { AgenticEntity, AgenticKinds } from './agentic';
import { AgentConfig, ModelMatcher } from './agents';
import { InstanceStatus } from './toolsets';
import { ToolKind } from './tools';

/// The limits of a workflow's runs
export interface WorkflowLimits {
  /// The most sub-agent invocations a run has going at once
  max_alive_sub_agents?: number | null;
}

/// The editable root that wires agents into a workflow
export interface WorkflowCatalogEntity {
  /// The agent that receives the workflow's inputs first
  entry_agent: string | null;
  /// Whether new instances are built automatically
  auto_instance: boolean;
  /// The limits of the workflow's runs
  limits: WorkflowLimits;
  /// The default models for agents without their own
  default_models: ModelMatcher[];
  /// The levels that may supply model auth
  model_auth: { levels: string[] };
  /// The agents in this workflow (populated on read)
  agents: AgenticEntity<AgenticKinds.Agent, unknown>[];
  /// When this workflow was last modified
  modified: string;
  /// The latest Ready instance
  latest_instance: string | null;
  /// The newest instance of any status
  newest_instance: string | null;
  /// The highest revision built so far
  highest_revision: number;
}

/// An agent of a workflow instance, with the config it ran with
export interface InstancedAgent {
  /// The agent's id
  agent: string;
  /// The agent's name
  name: string;
  /// The agent's groups
  groups: string[];
  /// The agent's config when built
  config: AgentConfig;
  /// The hash of the config
  config_hash: string;
  /// The harness tool instance
  harness: string | null;
  /// The tool instances the agent calls
  tools: string[];
  /// The toolset instances the agent includes
  tool_sets: string[];
  /// How each tool is bound to the harness
  bindings: { tool_instance: string; mechanism: string }[];
  /// The sub-agents
  sub_agents: string[];
  /// The digest of the agent's effective tools
  toolset_digest: string;
}

/// A tool of a workflow instance
export interface InstancedTool {
  /// The tool
  tool: string;
  /// The tool instance
  tool_instance: string;
  /// The tool's name
  name: string;
  /// The tool's groups
  groups: string[];
  /// The tool's kind
  kind: ToolKind;
  /// The version label
  version: string;
  /// The spec hash
  spec_hash: string;
  /// The function name models call it by
  function_name: string | null;
  /// Whether the tool is pinned
  pinned: boolean;
  /// Whether the tool is only a dependency of another
  dependency: boolean;
}

/// A constant, fully resolved snapshot of a workflow's graph
export interface WorkflowInstanceEntity {
  /// The workflow this is an instance of
  workflow: string;
  /// The workflow's groups when built
  workflow_groups: string[];
  /// The revision of this instance
  revision: number;
  /// The instance this one follows
  previous: string | null;
  /// Whether the instance can run
  status: InstanceStatus;
  /// Warnings found while building
  warnings: string[];
  /// What caused the build
  trigger: unknown;
  /// The user whose change caused the build
  triggered_by: string;
  /// The entry agent
  entry_agent: string | null;
  /// The run limits
  limits: WorkflowLimits;
  /// The default models
  default_models: ModelMatcher[];
  /// The agents
  agents: InstancedAgent[];
  /// The tools
  tools: InstancedTool[];
  /// The content digest
  digest: string;
}

/// A workflow entity
export type Workflow = AgenticEntity<AgenticKinds.WorkflowCatalog, WorkflowCatalogEntity>;

/// A workflow instance entity
export type WorkflowInstance = AgenticEntity<AgenticKinds.WorkflowInstance, WorkflowInstanceEntity>;

/// The form that creates a workflow
export interface WorkflowCreateForm {
  /// The workflow's name
  name: string;
  /// The groups to create it in
  groups: string[];
  /// A description
  description: string;
  /// The agent ids in the workflow
  agents: string[];
  /// The entry agent id, or an empty string
  entryAgent: string;
  /// The default model ids, one per entry
  defaultModels: string[];
  /// The most sub-agent invocations at once, or an empty string
  maxAliveSubAgents: string;
  /// Whether new instances are built automatically
  autoInstance: boolean;
}
