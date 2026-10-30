//! The section each launch tab is built from: a heading, the banner of a failed or cut-off listing, a spinner while the
//! listing loads, and a search picker of what can be launched.
import { FC, ReactNode } from 'react';

// project imports
import { LaunchSection, NoticeBanner } from './LaunchNotices';
import { PickerOption, SearchPicker } from '@components/agentic/pickers';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { LoadingSpinner } from '@components/shared/fallback/LoadingSpinner';

/** Props for {@link LaunchPickerSection}. */
export interface LaunchPickerSectionProps {
  /** The section's heading */
  title: string;
  /** Why the listing behind the picker is incomplete, or an empty string */
  error: string;
  /** The test id of the listing error's text */
  errorTestId: string;
  /** How serious an incomplete listing is: an error when nothing may be offered, a warning when some groups failed */
  errorSeverity?: Severity;
  /** Whether the listing is still loading */
  loading: boolean;
  /** What to show instead of the picker once the listing has loaded, such as where to find something to launch */
  empty?: ReactNode;
  /** The picker's label */
  label: string;
  /** The hint below the picker's label */
  hint: string;
  /** The test id prefix of the picker */
  testId: string;
  /** What can be chosen */
  options: PickerOption[];
  /** The values chosen */
  selected: string[];
  /** Called with the values chosen */
  onSelect: (selected: string[]) => void;
  /** The picker's text when there is nothing to choose */
  emptyText?: string;
  /** The picker's search text, kept by the page so a tab switch doesn't clear it */
  search: string;
  /** Called with the search text as typed */
  onSearch: (search: string) => void;
}

/**
 * A launch tab's section: the listing's error beside the picker (the picker decides what can be run, so an incomplete
 * listing is shown with it), a spinner while it loads, then the picker or what to show when there is nothing to pick.
 */
export const LaunchPickerSection: FC<LaunchPickerSectionProps> = ({
  title,
  error,
  errorTestId,
  errorSeverity = Severity.Error,
  loading,
  empty,
  label,
  hint,
  testId,
  options,
  selected,
  onSelect,
  emptyText,
  search,
  onSearch,
}) => (
  <LaunchSection>
    <h2>{title}</h2>
    {error !== '' && (
      <NoticeBanner severity={errorSeverity}>
        <span data-testid={errorTestId}>{error}</span>
      </NoticeBanner>
    )}
    <LoadingSpinner loading={loading} />
    {!loading && empty}
    {!loading && empty === undefined && (
      <SearchPicker
        label={label}
        hint={hint}
        testId={testId}
        options={options}
        selected={selected}
        onChange={onSelect}
        emptyText={emptyText}
        search={search}
        onSearch={onSearch}
      />
    )}
  </LaunchSection>
);
