import { FC } from 'react';

// project imports
import { LaunchSection, NoticeBanner } from './LaunchNotices';
import { entityOptions, SearchPicker } from '@components/agentic/pickers';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { LoadingSpinner } from '@components/shared/fallback/LoadingSpinner';
import { AgenticListQueryState } from '@utilities/queries/agentic';
import { Tool, ToolKind } from '@models/tools';

/** Props for {@link ToolLaunch}. */
interface ToolLaunchProps {
  /** The tools the user can see */
  tools: AgenticListQueryState<Tool>;
  /** The ids of the chosen tools */
  selected: string[];
  /** Called with the ids of the chosen tools */
  onSelect: (ids: string[]) => void;
  /** The picker's search text, kept by the page so a tab switch doesn't clear it */
  search: string;
  /** Called with the search text as typed */
  onSearch: (search: string) => void;
}

/** The Tools tab: a searchable list of the tools to run directly, each added to the runs to launch. */
export const ToolLaunch: FC<ToolLaunchProps> = ({ tools, selected, onSelect, search, onSearch }) => {
  // Harness tools can't be run directly
  const runnable = tools.items.filter((tool) => tool.metadata.Tool.kind !== ToolKind.Harness);
  return (
    <LaunchSection>
      <h2>Choose tools</h2>
      {/* the picker decides what can be run, so a failed or cut-off tool listing is shown with it */}
      {tools.error !== '' && (
        <NoticeBanner severity={Severity.Error}>
          <span data-testid="tools-error">{tools.error}</span>
        </NoticeBanner>
      )}
      <LoadingSpinner loading={tools.loading} />
      {!tools.loading && (
        <SearchPicker
          label="Tools"
          hint="Each chosen tool starts on its latest version; pick another version in the list below."
          testId="run-tool"
          options={entityOptions(runnable)}
          selected={selected}
          onChange={onSelect}
          emptyText="No tools can be run directly."
          search={search}
          onSearch={onSearch}
        />
      )}
    </LaunchSection>
  );
};
