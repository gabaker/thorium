import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { LaunchPickerSection, LaunchPickerSectionProps } from './LaunchPickerSection';

/**
 * Render a launch picker section with some props changed.
 *
 * @param props - The props to change.
 * @returns The markup.
 */
const render = (props: Partial<LaunchPickerSectionProps> = {}) =>
  renderToStaticMarkup(
    <LaunchPickerSection
      title="Choose things"
      error=""
      errorTestId="things-error"
      loading={false}
      label="Things"
      hint="Pick some things."
      testId="run-thing"
      options={[{ value: 'a', text: 'alpha' }]}
      selected={[]}
      onSelect={() => undefined}
      search=""
      onSearch={() => undefined}
      {...props}
    />,
  );

describe('LaunchPickerSection', () => {
  it('shows the heading and the picker, with no banner for a complete listing', () => {
    const markup = render();
    expect(markup).toContain('Choose things');
    expect(markup).toContain('alpha');
    expect(markup).not.toContain('things-error');
  });
  it('shows an incomplete listing beside the picker', () => {
    const markup = render({ error: 'List Thing: refused' });
    expect(markup).toContain('data-testid="things-error"');
    expect(markup).toContain('List Thing: refused');
    expect(markup).toContain('alpha');
  });
  it('shows no picker while the listing loads', () => {
    expect(render({ loading: true })).not.toContain('alpha');
  });
  it('shows what it is given for an empty listing instead of the picker, once loaded', () => {
    const empty = <p data-testid="nothing">Nothing here</p>;
    expect(render({ empty })).toContain('Nothing here');
    expect(render({ empty })).not.toContain('alpha');
    expect(render({ empty, loading: true })).not.toContain('Nothing here');
  });
});
