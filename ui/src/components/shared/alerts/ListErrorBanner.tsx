// project imports
import AlertBanner, { Severity } from './AlertBanner';

interface ListErrorBannerProps {
  errors: string[]; // the messages from failed list reads; each distinct message is shown once
  className?: string; // defaults to 'mb-3' to match the info banner above a browsing list
}

/**
 * Error banner shown when a browsing list could not be read, so an unreadable list is not mistaken for an empty one.
 *
 * @param errors - The messages from the failed list reads; duplicates are shown once.
 * @param className - Optional wrapper class; defaults to `'mb-3'`.
 * @returns An error-severity {@link AlertBanner} listing the messages, or nothing when there are none.
 */
const ListErrorBanner: React.FC<ListErrorBannerProps> = ({ errors, className = 'mb-3' }) => {
  const distinct = [...new Set(errors)];
  if (distinct.length === 0) return null;
  return (
    <AlertBanner severity={Severity.Error} className={className}>
      {distinct.map((message) => (
        <div key={message} data-testid="list-error">
          {message}
        </div>
      ))}
    </AlertBanner>
  );
};

export default ListErrorBanner;
