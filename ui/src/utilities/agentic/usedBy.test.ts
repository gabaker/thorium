import { describe, expect, it } from 'vitest';

import { agentsUsingToolSet, workflowsUsingAgent } from './usedBy';
import { Agent } from '@models/agents';
import { Workflow } from '@models/workflows';

/** Build a workflow that holds the given agent ids. */
function workflow(id: string, agentIds: string[]): Workflow {
  return { id, metadata: { WorkflowCatalog: { agents: agentIds.map((agent) => ({ id: agent })) } } } as unknown as Workflow;
}

/** Build an agent that links the given toolset entities. */
function agent(id: string, toolSets: unknown[] | undefined): Agent {
  return { id, metadata: { Agent: { tool_sets: toolSets } } } as unknown as Agent;
}

describe('workflowsUsingAgent', () => {
  it('lists the workflows that have the agent as a member', () => {
    const all = [workflow('w1', ['a1', 'a2']), workflow('w2', ['a2']), workflow('w3', [])];
    expect(workflowsUsingAgent(all, 'a2').map((item) => item.id)).toEqual(['w1', 'w2']);
    expect(workflowsUsingAgent(all, 'a1').map((item) => item.id)).toEqual(['w1']);
    expect(workflowsUsingAgent(all, 'none')).toEqual([]);
  });
});

describe('agentsUsingToolSet', () => {
  it('finds agents that follow the toolset or pin one of its instances', () => {
    const all = [
      agent('a1', [{ id: 'set1', kind: 'ToolSet' }]),
      agent('a2', [{ id: 'inst1', kind: 'ToolSetInstance', metadata: { ToolSetInstance: { tool_set: 'set1' } } }]),
      agent('a3', [{ id: 'set2', kind: 'ToolSet' }]),
      agent('a4', undefined),
    ];
    expect(agentsUsingToolSet(all, 'set1').map((item) => item.id)).toEqual(['a1', 'a2']);
    expect(agentsUsingToolSet(all, 'set2').map((item) => item.id)).toEqual(['a3']);
  });
});
