import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';

// project imports
import StageToolPicker from './StageToolPicker';
import { StageToolOption } from '@components/agentic/pipelines/stages';

const OPTIONS: StageToolOption[] = [{ label: 'scanner · follow latest', value: 't1', base: 'scanner' }];

/** Render the picker with the props a test cares about. */
function render(props: { options?: StageToolOption[]; loading?: boolean; error?: string }): string {
  return renderToStaticMarkup(
    <MemoryRouter>
      <StageToolPicker
        options={props.options ?? []}
        group="ops"
        loading={props.loading ?? false}
        error={props.error ?? ''}
        onSelect={() => undefined}
        onBlur={() => undefined}
      />
    </MemoryRouter>,
  );
}

describe('StageToolPicker', () => {
  it('shows an empty state with a link to create a tool when the group has no stage tools', () => {
    const html = render({});
    expect(html).toContain('data-testid="stage-tool-empty"');
    expect(html).toContain('No stage tools in group ops');
    expect(html).toContain('href="/tools/create"');
    expect(html).toContain('Create a tool');
  });
  it('lists the tools to choose from', () => {
    const html = render({ options: OPTIONS });
    expect(html).not.toContain('stage-tool-empty');
    expect(html).toContain('Choose a tool');
    expect(html).toContain('scanner · follow latest');
  });
  it('says so while the tools load and does not claim the group has none', () => {
    const html = render({ loading: true });
    expect(html).toContain('Loading tools');
    expect(html).not.toContain('stage-tool-empty');
  });
  it('shows a failed tool list rather than an empty state', () => {
    const html = render({ error: 'tool list unavailable' });
    expect(html).toContain('tool list unavailable');
    expect(html).not.toContain('stage-tool-empty');
  });
});
