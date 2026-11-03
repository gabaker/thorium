// project imports
import { AgenticKinds } from '@models/agentic';
import { Agent } from '@models/agents';
import { ToolSetInstanceEntity } from '@models/toolsets';
import { Workflow } from '@models/workflows';

/**
 * Find the workflows that include an agent.
 *
 * @param workflows - The workflows to look through, with their agents populated.
 * @param agentId - The agent's id.
 * @returns The workflows that list the agent as a member, in the order given.
 */
export function workflowsUsingAgent(workflows: Workflow[], agentId: string): Workflow[] {
  return workflows.filter((workflow) => workflow.metadata.WorkflowCatalog.agents.some((member) => member.id === agentId));
}

/**
 * Find the agents that include a toolset, following its latest instance or pinned to one of its instances.
 *
 * @param agents - The agents to look through, with their toolset links populated.
 * @param toolSetId - The toolset's id.
 * @returns The agents that link the toolset or one of its instances, in the order given.
 */
export function agentsUsingToolSet(agents: Agent[], toolSetId: string): Agent[] {
  return agents.filter((agent) =>
    (agent.metadata.Agent.tool_sets ?? []).some((link) => {
      // a pinned link names an instance, which names the toolset it was built from
      if (link.kind === AgenticKinds.ToolSetInstance) {
        return (link.metadata.ToolSetInstance as ToolSetInstanceEntity | undefined)?.tool_set === toolSetId;
      }
      return link.id === toolSetId;
    }),
  );
}
