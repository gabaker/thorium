import { beforeEach, describe, expect, it, vi } from 'vitest';

// project imports
import { AgenticReadError } from './agentic';
import { fetchToolRead } from './tools';
import { getAgenticEntity, listToolInstances } from '@thorpi/agentic';
import { AgenticKinds } from '@models/agentic';
import { Tool, ToolInstanceSummary } from '@models/tools';

vi.mock('@thorpi/agentic', () => ({ getAgenticEntity: vi.fn(), listToolInstances: vi.fn(), listAllAgentic: vi.fn() }));

/**
 * Build a tool whose own version list holds some versions.
 *
 * @param instances - The versions on the tool entity.
 * @returns The tool.
 */
const tool = (instances: ToolInstanceSummary[]) =>
  ({ id: 't', kind: AgenticKinds.Tool, metadata: { Tool: { latest: 'i2', instances } } }) as unknown as Tool;

/**
 * Build a version summary.
 *
 * @param id - The version's instance id.
 * @returns The summary.
 */
const version = (id: string) => ({ id, version: { Custom: id } }) as unknown as ToolInstanceSummary;

beforeEach(() => vi.resetAllMocks());

describe('fetchToolRead', () => {
  it('replaces the trimmed version list with the full one', async () => {
    vi.mocked(getAgenticEntity).mockResolvedValue(tool([version('i2')]));
    vi.mocked(listToolInstances).mockResolvedValue([version('i2'), version('i1')]);
    const read = await fetchToolRead('t');
    expect('tool' in read && read.tool.metadata.Tool.instances?.map((entry) => entry.id)).toEqual(['i2', 'i1']);
    expect('tool' in read && read.versionsError).toBe('');
  });

  it("keeps the tool's own versions and the message when the full list can't be read", async () => {
    vi.mocked(getAgenticEntity).mockResolvedValue(tool([version('i2')]));
    vi.mocked(listToolInstances).mockImplementation((_id, onError) => {
      onError('List Tool Instances: refused');
      return Promise.resolve([]);
    });
    const read = await fetchToolRead('t');
    expect('tool' in read && read.tool.metadata.Tool.instances?.map((entry) => entry.id)).toEqual(['i2']);
    expect('tool' in read && read.versionsError).toBe('List Tool Instances: refused');
  });

  it('names the kind of an id that is not a tool, without listing versions', async () => {
    vi.mocked(getAgenticEntity).mockResolvedValue({ id: 'i2', kind: AgenticKinds.ToolInstance } as never);
    await expect(fetchToolRead('i2')).resolves.toEqual({ other: { id: 'i2', kind: AgenticKinds.ToolInstance } });
    expect(listToolInstances).not.toHaveBeenCalled();
  });

  it('throws the message and status of a failed read, so a retry can be offered for a server error only', async () => {
    vi.mocked(getAgenticEntity).mockImplementation((_id, onError, onStatus) => {
      onError('Get Entity: server error');
      onStatus?.(503);
      return Promise.resolve(null);
    });
    const failure = await fetchToolRead('t').catch((error: unknown) => error);
    expect(failure).toBeInstanceOf(AgenticReadError);
    expect(failure).toMatchObject({ message: 'Get Entity: server error', status: 503 });
  });
});
