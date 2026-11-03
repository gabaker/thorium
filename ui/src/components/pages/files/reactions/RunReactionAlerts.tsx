import { Link } from 'react-router';
import styled from 'styled-components';

// project imports
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { spacers } from '@styles';
import { ReactionRunResult } from '@models/reactions';

/** The submission banners, one under another. */
const AlertStack = styled.div`
  display: flex;
  flex-direction: column;
  gap: ${spacers.three};
  margin: ${spacers.two} 0;
`;

/** The props of {@link RunReactionAlerts}. */
interface RunReactionAlertsProps {
  /** The outcome of each requested reaction; an empty error means it was submitted. */
  responses: ReactionRunResult[];
}

/**
 * Show one banner per requested reaction: the error for a failure, a link to the reaction for a success.
 *
 * @param props - The submission outcomes.
 * @returns The banners, or nothing when there are none.
 */
const RunReactionAlerts = ({ responses }: RunReactionAlertsProps) => {
  if (responses.length === 0) return null;
  return (
    <AlertStack>
      {responses.map((runResponse, idx) =>
        runResponse.error ? (
          <AlertBanner key={idx}>{runResponse.error}</AlertBanner>
        ) : (
          <AlertBanner key={idx} severity={Severity.Info}>
            <span>
              {`Successfully submitted reaction `}
              <Link className="link-text" to={`/reaction/${runResponse.group}/${runResponse.id}`} target="_blank">
                {runResponse.id}
              </Link>
              {` for pipeline ${runResponse.pipeline} from group ${runResponse.group}!`}
            </span>
          </AlertBanner>
        ),
      )}
    </AlertStack>
  );
};

export default RunReactionAlerts;
