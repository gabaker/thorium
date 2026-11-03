import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { entityOptions, filterOptions, SearchPicker } from './pickers';
import { pipelineOptions } from './runs/PipelineLaunch';
import { AnyAgenticEntity } from '@models/agentic';
import { Pipeline } from '@models/pipelines';

/** Two same-named tools in different groups and one other tool. */
const ENTITIES = [
  { id: '11111111-aaaa', name: 'scan', groups: ['system'] },
  { id: '22222222-bbbb', name: 'scan', groups: ['corp'] },
  { id: '33333333-cccc', name: 'strings', groups: ['system'] },
] as unknown as AnyAgenticEntity[];

describe('entityOptions', () => {
  it('notes the groups of each entity and a short id for same-named ones', () => {
    expect(entityOptions(ENTITIES).map((option) => [option.value, option.note])).toEqual([
      ['11111111-aaaa', 'in system; #11111111'],
      ['22222222-bbbb', 'in corp; #22222222'],
      ['33333333-cccc', 'in system'],
    ]);
  });
  it('marks entities missing a required group as unavailable', () => {
    expect(entityOptions(ENTITIES, ['corp']).map((option) => option.disabled)).toEqual([true, false, true]);
  });
});

describe('filterOptions', () => {
  it('keeps the choices whose text or note holds every word, ignoring case', () => {
    const options = entityOptions(ENTITIES);
    expect(filterOptions(options, '').map((option) => option.value)).toHaveLength(3);
    expect(filterOptions(options, 'SCAN corp').map((option) => option.value)).toEqual(['22222222-bbbb']);
    expect(filterOptions(options, 'str').map((option) => option.text)).toEqual(['strings']);
    expect(filterOptions(options, 'nothing')).toEqual([]);
  });
});

describe('pipelineOptions', () => {
  it('keys pipelines by group and name and notes one that cannot run', () => {
    const pipelines = [
      { group: 'system', name: 'triage', order: [['scan']], stage_tools: { scan: { tool: 't' } } },
      { group: 'system', name: 'legacy', order: [['harvest']], stage_tools: {} },
    ] as unknown as Pipeline[];
    expect(pipelineOptions(pipelines)).toEqual([
      { value: 'system/triage', text: 'triage', note: 'in system' },
      { value: 'system/legacy', text: 'legacy', note: 'in system; cannot be run' },
    ]);
  });
});

describe('SearchPicker', () => {
  it('renders a search box, a count, and one checkbox per choice with the chosen ones checked', () => {
    const markup = renderToStaticMarkup(
      <SearchPicker
        label="Tools"
        testId="run-tool"
        options={entityOptions(ENTITIES)}
        selected={['33333333-cccc']}
        onChange={() => undefined}
      />,
    );
    expect(markup).toContain('data-testid="run-tool-search"');
    expect(markup).toContain('3 available, 1 chosen');
    expect(markup).toContain('data-testid="run-tool-33333333-cccc" data-entity-id="33333333-cccc" checked=""');
  });
  it('shows and filters by the search text a parent keeps for it', () => {
    const markup = renderToStaticMarkup(
      <SearchPicker
        label="Tools"
        testId="run-tool"
        options={[
          { value: 'a', text: 'strings' },
          { value: 'b', text: 'yara' },
        ]}
        selected={[]}
        onChange={() => undefined}
        search="yar"
        onSearch={() => undefined}
      />,
    );
    expect(markup).toContain('value="yar"');
    expect(markup).toContain('1 of 2, 0 chosen');
    expect(markup).toContain('data-testid="run-tool-b"');
    expect(markup).not.toContain('data-testid="run-tool-a"');
  });
});
