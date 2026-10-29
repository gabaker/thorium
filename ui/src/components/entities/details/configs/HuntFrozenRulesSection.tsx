import { useEffect, useMemo, useState, JSX } from 'react';
import { Link } from 'react-router-dom';
import { Badge, Card, Col, Pagination, Row } from 'react-bootstrap';

// project imports
import { DetailsSectionProps } from '../EntityDetails';
import { clampPage, filterFrozenRules, frozenRuleDetailsUrl, pageCount } from './huntFrozenRules';
import { DEFAULT_LIST_LIMIT } from '@entities/utilities';
import { BrowsingCard, BrowsingContents, EntityName, EntitySecondary, LinkFields } from '@entities/browsing/shared';
import { OmnibarRuleSetRules } from '@components/shared/inputs/omnibar/Bars';
import { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { defaultTimeSelection, TimeSelection } from '@components/shared/inputs/omnibar/timepicker/utils';
import { getLimitFromClauses } from '@components/shared/inputs/omnibar/utils';
import NoResultsBanner from '@components/shared/alerts/NoResultsBanner';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import FieldBadge from '@components/shared/badges/FieldBadge';
import Subtitle from '@components/shared/titles/Subtitle';
import { listHuntRules } from '@thorpi/hunts';
import { Entities, entityLabel } from '@models/entities';
import { HuntRuleLine } from '@models/entities/hunts';

// drain the frozen list in pages of 1000: even cap-sized hunts resolve in a few requests
const DRAIN_PAGE_SIZE = 1000;

interface FrozenRuleRowProps {
  line: HuntRuleLine;
}

/**
 * One frozen rule row: hydrated lines link to the rule's details page; a rule deleted
 * after the freeze (or hidden from the caller) renders its id, linkless, with a muted
 * marker. Current quarantine status values render as warning badges and the admin-set
 * exemption as its own badge, matching the metadata card's excluded-count badges.
 *
 * @param props - The frozen rule line to render.
 * @returns The frozen rule row.
 */
const FrozenRuleRow = ({ line }: FrozenRuleRowProps): JSX.Element => {
  // lines with no hydrated or routable kind render linkless rather than linking wrong
  const url = frozenRuleDetailsUrl(line);
  const fields = (
    <LinkFields>
      <EntityName>
        {line.name ?? line.id}
        {line.status.map((status) => (
          <Badge bg="warning" className="ms-2" key={`frozen-rule-status-${line.id}-${status}`}>
            {status}
          </Badge>
        ))}
        {line.quarantine_exempt && (
          <Badge bg="info" className="ms-2">
            Exempt
          </Badge>
        )}
      </EntityName>
      <EntitySecondary>
        {line.kind ? (
          <FieldBadge color="gray" field={entityLabel(line.kind)} />
        ) : (
          // nothing hydrated: the rule was deleted after the freeze or the caller can't see it
          <small className="text-muted">(deleted or hidden)</small>
        )}
      </EntitySecondary>
    </LinkFields>
  );
  return (
    <BrowsingCard>
      <BrowsingContents>
        {url ? (
          <Link to={url} className="no-decoration">
            {fields}
          </Link>
        ) : (
          fields
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

/**
 * Props for the frozen rules panel: the details section contract plus whether to render
 * the in-card "Frozen Rules" title (the findings tabs already name the view, so the
 * tabbed usage hides it; the unknown-engine standalone fallback keeps it).
 */
type HuntFrozenRulesSectionProps = DetailsSectionProps<Entities.Hunt> & {
  showTitle?: boolean;
};

/**
 * The hunt's frozen rule snapshot as a filterable, paged list; the findings card shows
 * it behind the Frozen Rules tab (the hits list is the default tab).
 *
 * The snapshot is immutable, so the whole list is drained once per hunt id through
 * `listHuntRules` and then filtered/paged entirely client side — the page's 10s status
 * poll re-renders this section every tick, but keying the drain on `entity.id` alone
 * means those ticks never refetch. The omnibar narrows by name/id substring, engine
 * kind, and page size, and the time picker by rule creation date, mirroring the
 * ruleset members section.
 *
 * @param props - The saved hunt entity (the blank placeholder's empty id never fetches)
 *                and the optional title suppression flag.
 * @returns The frozen rules section.
 */
const HuntFrozenRulesSection = ({ entity, showTitle = true }: HuntFrozenRulesSectionProps): JSX.Element => {
  const [lines, setLines] = useState<HuntRuleLine[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [clauses, setClausesState] = useState<Clause[]>([]);
  const [time, setTimeState] = useState<TimeSelection>(defaultTimeSelection());
  const [page, setPage] = useState(0);

  // drain the immutable snapshot once per hunt id; poll-tick re-renders change no deps
  useEffect(() => {
    // the details factory leaves the blank placeholder in place until the hunt lands
    if (entity.id === '') {
      return;
    }
    // drop responses landing after an unmount (or after the route moved to another hunt)
    let active = true;
    setLoading(true);
    setError('');
    const drain = async () => {
      // walk the cursor until the API stops returning one (the frozen cursors never expire)
      const drained: HuntRuleLine[] = [];
      let failed = '';
      let cursor: string | null = null;
      do {
        const result = await listHuntRules(entity.id, cursor, DRAIN_PAGE_SIZE, (err: string) => {
          failed = err;
        });
        drained.push(...result.lines);
        cursor = result.cursor;
      } while (cursor !== null && failed === '' && active);
      if (active) {
        // keep whatever drained so a mid-walk failure still shows the pages that landed
        setLines(drained);
        setError(failed);
        setLoading(false);
      }
    };
    void drain();
    return () => {
      active = false;
    };
  }, [entity.id]);

  // a filter edit restarts paging; a remembered page index over a reshaped set is confusing
  const setClauses = (next: Clause[]) => {
    setClausesState(next);
    setPage(0);
  };

  // a time range edit restarts paging exactly like a clause edit
  const setTime = (next: TimeSelection) => {
    setTimeState(next);
    setPage(0);
  };

  // filter + name-sort the drained lines whenever they, the clauses, or the range change
  const filtered = useMemo(() => filterFrozenRules(lines, clauses, time), [lines, clauses, time]);
  // the page size rides in the clauses like every other omnibar-driven list
  const limit = getLimitFromClauses(clauses, DEFAULT_LIST_LIMIT);
  const maxPage = pageCount(filtered.length, limit);
  // clamp at render (not setState) so a shrink via filtering can never show an empty page
  const safePage = clampPage(page, filtered.length, limit);
  const pageLines = filtered.slice(safePage * limit, (safePage + 1) * limit);

  return (
    <Card className="panel mt-4" data-testid="hunt-frozen-rules">
      <Card.Body>
        {/* the tabbed usage drops the title (the active tab already names the view) */}
        {showTitle && (
          <div className="d-flex justify-content-center">
            <Subtitle>Frozen Rules</Subtitle>
          </div>
        )}
        <Row>
          <Col className="d-flex justify-content-center">
            <OmnibarRuleSetRules clauses={clauses} setClauses={setClauses} time={time} setTime={setTime} />
          </Col>
        </Row>
        {error !== '' && <AlertBanner className="mt-3">{error}</AlertBanner>}
        {loading ? (
          <Row className="mt-3">
            <Col className="d-flex justify-content-center">
              <small>Loading frozen rules...</small>
            </Col>
          </Row>
        ) : lines.length === 0 ? (
          error === '' && <NoResultsBanner type="Frozen Rules" />
        ) : filtered.length === 0 ? (
          <NoResultsBanner type="Matching Rules" />
        ) : (
          <>
            <Row className="d-flex justify-content-center g-0">
              <BrowsingCard>
                <BrowsingContents>
                  <Row>
                    <EntityName>Name</EntityName>
                    <EntitySecondary>Kind</EntitySecondary>
                  </Row>
                </BrowsingContents>
              </BrowsingCard>
            </Row>
            {pageLines.map((line) => (
              <Row key={`hunt-frozen-rule-${line.id}`} className="d-flex justify-content-center g-0">
                <FrozenRuleRow line={line} />
              </Row>
            ))}
            <Row className="mt-3">
              <Col className="d-flex justify-content-center">
                {/* visible count so the filter's effect is obvious even mid-page */}
                <small>
                  {filtered.length} of {lines.length} rules
                </small>
              </Col>
            </Row>
            {filtered.length > limit && (
              <Row>
                <Col className="d-flex justify-content-center">
                  <Pagination>
                    <Pagination.Item onClick={() => setPage(safePage - 1)} disabled={safePage === 0}>
                      Back
                    </Pagination.Item>
                    <Pagination.Item onClick={() => setPage(safePage + 1)} disabled={safePage + 1 >= maxPage}>
                      Next
                    </Pagination.Item>
                  </Pagination>
                </Col>
              </Row>
            )}
          </>
        )}
      </Card.Body>
    </Card>
  );
};

export default HuntFrozenRulesSection;
