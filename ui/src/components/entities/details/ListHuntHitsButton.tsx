import { Button } from 'react-bootstrap';
import { FaCrosshairs } from 'react-icons/fa';
import { useNavigate } from 'react-router';

// project imports
import { OverlayTipBottom } from '@components/shared/overlay/tips';
import { buildTagBrowseHref } from '@components/tags/utilities';
import { Hunt, HUNT_ENGINES, HUNT_ID_TAG } from '@models/entities/hunts';

// spec: ./EntityDetails.spec.md

/**
 * Build the browse URL listing a hunt's findings: the engine's hit kind pre-filtered by the
 * `HuntId` tag the hit route stamps on every decorated finding.
 *
 * The engine registry picks the finding kind, so a future engine's hunts link to their own
 * hit browse without touching this helper. The tag browse is the exact findings record —
 * counters are telemetry and can drift under replay.
 *
 * @param hunt - The hunt whose findings to list.
 * @returns The pre-filtered browse URL, or `undefined` when the hunt is the blank
 *          placeholder or the hit kind has no browse route.
 */
export const buildHuntHitsHref = (hunt: Hunt): string | undefined => {
  // on a missing or mismatched route id the details factory leaves the BlankHunt
  // placeholder in place (mirror the refetchIntervalMs guard); there is no findings
  // record to link to, so the button hides rather than browse an empty HuntId filter
  if (hunt.id === '') {
    return undefined;
  }
  // an engine this UI build does not know (a newer server during a rolling upgrade) has
  // no hit kind to link to; the button hides instead of crashing the page
  const info = HUNT_ENGINES[hunt.metadata.Hunt.engine];
  if (!info) {
    return undefined;
  }
  return buildTagBrowseHref(info.hitKind, HUNT_ID_TAG, hunt.id);
};

type ListHuntHitsProps = {
  hunt: Hunt;
};

/**
 * The hunt page's open-in-browse action, rendered in the top button row next to the
 * dashboard button: navigates to the engine's full hit browse (omnibar and time
 * filtering included) pre-filtered by this hunt's `HuntId` tag.
 */
const ListHuntHitsButton: React.FC<ListHuntHitsProps> = ({ hunt }) => {
  const navigate = useNavigate();
  const href = buildHuntHitsHref(hunt);
  if (!href) {
    return null;
  }
  return (
    <OverlayTipBottom tip={`Open the full hit browse for hunt "${hunt.name}"`}>
      <Button className="icon-btn mx-1" variant="" aria-label="View hunt hits" onClick={() => void navigate(href)}>
        <FaCrosshairs size={20} />
      </Button>
    </OverlayTipBottom>
  );
};

export default ListHuntHitsButton;
