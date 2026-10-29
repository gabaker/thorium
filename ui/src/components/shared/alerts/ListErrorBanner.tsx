// project imports
import AlertBanner, { Severity } from './AlertBanner';
import { CodeSpans } from '@components/shared/syntax/CodeSpans';

interface ListErrorBannerProps {
  errors: string[]; // the messages from failed list reads; empty strings are skipped and each distinct message is shown once
  className?: string; // defaults to 'mb-3' to match the info banner above a browsing list
}

/**
 * Error banner shown when a list could not be read, so an unreadable list is not mistaken for an empty one.
 *
 * A page that reads several lists can pass every list's error, using an empty string for a list that loaded cleanly.
 *
 * @param errors - The messages from the failed list reads; empty strings are skipped and duplicates are shown once.
 * @param className - Optional wrapper class; defaults to `'mb-3'`.
 * @returns An error-severity {@link AlertBanner} listing the messages with their `backticked` names as inline code, or
 * nothing when there are none.
 */
const ListErrorBanner: React.FC<ListErrorBannerProps> = ({ errors, className = 'mb-3' }) => {
  // two lists that fail the same way (the API being down) show the message once
  const distinct = [...new Set(errors.filter((message) => message !== ''))];
  if (distinct.length === 0) return null;
  return (
    <AlertBanner severity={Severity.Error} className={className}>
      {distinct.map((message) => (
        <div key={message} data-testid="list-error">
          <CodeSpans text={message} />
        </div>
      ))}
    </AlertBanner>
  );
};

export default ListErrorBanner;
