// project imports
import { AgenticEntity, AgenticKinds, AnyAgenticEntity } from './agentic';

/// A matcher for the models an agent accepts
export interface ModelMatcher {
  /// The exact model id
  model_id: string;
  /// The provider the model must come from
  provider?: string | null;
  /// The protocol the model must be called with
  api?: string | null;
  /// The API versions that are accepted
  api_versions?: string[];
}

/// What every model an agent runs on must provide
export interface ModelRequirements {
  /// The smallest context window
  min_context_window?: number | null;
  /// The smallest max output
  min_output_tokens?: number | null;
  /// The capabilities the model must have
  capabilities?: Record<string, boolean | null>;
}

/// Whether a model must call a tool: `Auto`, `Required`, `None`, or `{ Function: name }`
export type ToolChoice = 'Auto' | 'Required' | 'None' | { Function: string };

/// The configuration of an agent that is snapshotted into workflow instances
export interface AgentConfig {
  /// The system prompt given to the model when this agent is spawned
  system_prompt?: string | null;
  /// The statically defined task prompt; a run's inputs are added to it by the harness
  prompt?: string | null;
  /// The models this agent accepts, highest priority first
  models?: ModelMatcher[];
  /// What every model this agent runs on must provide
  model_requirements?: ModelRequirements;
  /// The max number of tools mapped into a single agent session
  max_tools?: number | null;
  /// The max number of model turns per invocation
  max_turns?: number | null;
  /// The max copies of this agent a single swarm may include
  max_swarm_size?: number | null;
  /// Tags a harness may use to narrow this agent's tools per run
  tool_tags?: Record<string, string[]>;
  /// The function name another agent's model calls to delegate to this agent
  function_name?: string | null;
  /// The description another agent's model sees when it can delegate
  model_description?: string | null;
  /// Whether this agent's model must call a tool on the first call
  tool_choice?: ToolChoice;
  /// Whether this agent's model may call several tools in one turn
  parallel_tool_calls?: boolean | null;
}

/// A configured AI agent and the entities it is linked to
export interface AgentEntity {
  /// The agent's configuration
  config: AgentConfig;
  /// The levels that may supply model auth
  model_auth?: { levels: string[] };
  /// The harness this agent runs in (a Tool follows its latest instance, a ToolInstance is pinned)
  harness?: AnyAgenticEntity | null;
  /// The tools followed at latest
  latest_tools: AnyAgenticEntity[];
  /// The tool instances pinned
  pinned_tools: AnyAgenticEntity[];
  /// The toolsets (a ToolSet follows latest, a ToolSetInstance is pinned)
  tool_sets: AnyAgenticEntity[];
  /// The sub-agents
  sub_agents: AnyAgenticEntity[];
  /// When this agent was last modified
  modified?: string;
}

/// An agent entity
export type Agent = AgenticEntity<AgenticKinds.Agent, AgentEntity>;

/// The form that creates an agent
export interface AgentCreateForm {
  /// The agent's name
  name: string;
  /// The groups to create it in
  groups: string[];
  /// A description
  description: string;
  /// The system prompt
  systemPrompt: string;
  /// The task prompt
  prompt: string;
  /// The model ids, highest priority first, one per line
  models: string[];
  /// The max number of model turns
  maxTurns: string;
  /// The function name another agent's model calls to delegate to this agent, or an empty string
  functionName?: string;
  /// The description another agent's model sees when it can delegate, or an empty string
  modelDescription?: string;
  /// The harness tool id (latest) or tool instance id (pinned), or an empty string
  harness: string;
  /// The tool ids (latest) or tool instance ids (pinned) the agent calls
  tools: string[];
  /// The toolset ids (latest) or toolset instance ids (pinned) the agent includes
  toolSets: string[];
  /// The sub-agent ids the agent delegates to
  subAgents: string[];
}
