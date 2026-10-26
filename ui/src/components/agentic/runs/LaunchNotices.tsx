import { FC } from 'react';
import styled from 'styled-components';

// project imports
import { cleanRunError } from './launch';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Field, Section } from '@components/agentic/shared';
import { useAuth } from '@utilities/auth';
import { isDeveloperAny } from '@utilities/permissions';

/** A banner kept clear of the field label or button that follows it. */
export const NoticeBanner = styled(AlertBanner)`
  margin-bottom: 0.75rem;
`;

/**
 * The card of a launch form, set 1rem below the tab strip like the content under the tabs of the runs list. A field or
 * banner that ends the card drops its bottom margin so only the card's padding sits below it.
 */
export const LaunchSection = styled(Section)`
  margin-top: 1rem;
  > ${Field}:last-child, > ${NoticeBanner}:last-child {
    margin-bottom: 0;
  }
`;

/** Props for {@link LaunchError}. */
interface LaunchErrorProps {
  /** The error message to show; nothing is drawn when it is empty */
  error: string;
  /** The test id of the message, `launch-error` unless given */
  testId?: string;
}

/** The banner of a failed launch: the reason as a sentence, with the API's trace id kept as a hover hint. */
export const LaunchError: FC<LaunchErrorProps> = ({ error, testId = 'launch-error' }) => {
  if (error === '') return null;
  const { text, trace } = cleanRunError(error);
  return (
    <NoticeBanner severity={Severity.Error}>
      <span data-testid={testId} title={trace === '' ? undefined : `trace: ${trace}`}>
        {text}
      </span>
    </NoticeBanner>
  );
};

/** A warning, shown before the form is filled, to a user whose role can't start runs on any scaler. */
export const RunRoleHint: FC = () => {
  const { userInfo } = useAuth();
  // the API refuses a run unless the user is an admin, an analyst, or a developer for the scaler of its tools
  if (!userInfo || isDeveloperAny(userInfo)) return null;
  return (
    <NoticeBanner severity={Severity.Warning}>
      <span data-testid="run-role-hint">
        Your Thorium role can&apos;t start runs: running a tool or workflow takes a Developer, Analyst, or Admin role. You can still fill in
        this form, but Thorium will refuse to start the run. Ask an admin to change your role.
      </span>
    </NoticeBanner>
  );
};
