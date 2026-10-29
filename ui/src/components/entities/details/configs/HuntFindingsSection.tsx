import React, { useMemo, useState, JSX } from 'react';
import { Card } from 'react-bootstrap';

// project imports
import { DetailsSectionProps } from '../EntityDetails';
import HuntFrozenRulesSection from './HuntFrozenRulesSection';
import { huntFindingsTabs, HuntFindingsView, huntHitKind, huntHitsFilters, INLINE_HIT_HIDDEN_TAG_KEYS } from './huntFindings';
import { DEFAULT_LIST_LIMIT } from '@entities/utilities';
import EntityList from '@entities/browsing/EntityList';
// one specific config on purpose: importing the all-configs map (browsing/configs/config.ts)
// from a details config would evaluate every browse config whenever the hunt page loads and
// risks the TDZ circular-import failure documented in browsing/configs/entityFetcher.ts
import YaraRuleHitBrowsingConfig, { YaraRuleHitItem, YaraRuleHitListHeaders } from '@entities/browsing/configs/YaraRuleHitBrowsingConfig';
import { Tabs } from '@components/shared/tabs';
import { LoadingSpinner } from '@components/shared/fallback/LoadingSpinner';
import { Entities } from '@models/entities';
import { Hunt } from '@models/entities/hunts';
import { YaraRuleHit } from '@models/entities/rules/yara_hits';

interface HitsListProps {
  hunt: Hunt;
}

/**
 * The keyed inner half of the inline hit list: owns the loading flag and the
 * `EntityList` it drives.
 *
 * The loading state must live inside the keyed boundary: `EntityList` skips its
 * mount fetch while `loading` is true, so a parent-owned flag that survived a
 * key remount landing mid-fetch would leave the fresh list permanently empty
 * ("No Hits Found") — after the terminal status transition no further key
 * change would ever recover it.
 *
 * @param props - The saved hunt whose findings to list.
 * @returns The cursor-paged hit list.
 */
const YaraHitsListInner = ({ hunt }: HitsListProps): JSX.Element => {
  // start false so EntityList's mount fetch is not skipped by its loading guard
  const [loading, setLoading] = useState(false);
  const filters = useMemo(() => huntHitsFilters(hunt.id, DEFAULT_LIST_LIMIT), [hunt.id]);
  return (
    <EntityList<YaraRuleHit>
      type="Hits"
      entityHeaders={<YaraRuleHitListHeaders />}
      displayEntity={(hit) => <YaraRuleHitItem hit={hit} excludeKeys={INLINE_HIT_HIDDEN_TAG_KEYS} />}
      filters={filters}
      fetchEntities={YaraRuleHitBrowsingConfig.fetchEntities}
      loading={loading}
      setLoading={setLoading}
    />
  );
};

/**
 * The inline hit list for a yara hunt: the hit browse page's rows and fetcher pinned to
 * this hunt's `HuntId` tag.
 *
 * Refreshing rides on a React key, not on filters identity: the list (and its loading
 * flag, which lives inside the keyed component) remounts — and so refetches, since
 * mounting `EntityList` with non-empty filters always fetches — whenever the hunt's
 * status or written counter changes: hits appear as checkpoints record them and once
 * more on the terminal transition, while the page's 10s status poll (which hands this
 * component a fresh entity object every tick) never refetches by itself. Dev
 * StrictMode double-fires the mount fetch; both are reset fetches, harmless. Rows
 * hide the HuntId/submitter chips the panel's context already shows.
 *
 * @param props - The saved hunt whose findings to list.
 * @returns The keyed cursor-paged hit list.
 */
const YaraHitsList = ({ hunt }: HitsListProps): JSX.Element => {
  const meta = hunt.metadata.Hunt;
  // counters are serde-defaulted on the wire; hunts from older builds carry none
  const written = meta.counters?.written ?? 0;
  return <YaraHitsListInner key={`hunt-hits-${hunt.id}-${meta.status}-${written}`} hunt={hunt} />;
};

// the inline hit list per engine hit kind; a future engine adds a list component here
// and its kinds in HUNT_ENGINES — an unmapped kind falls back to the rules-only view
const INLINE_HIT_LISTS: Partial<Record<Entities, React.ComponentType<HitsListProps>>> = {
  [Entities.YaraRuleHit]: YaraHitsList,
};

/**
 * The hunt details page's replacement for the default Associations graph card: a tab
 * strip (the shared Tabs the file details page uses) over the hunt's findings, with the
 * Hits view as the default tab and the frozen rule snapshot behind the Frozen Rules tab.
 *
 * The inactive panel is hidden rather than unmounted once visited: a conditional render
 * would re-drain the frozen snapshot's 1000-per-page cursor walk and reset the hit
 * list's accumulated pages on every tab switch. The rules panel still mounts lazily so
 * a user who never opens it never drains the snapshot. The active tab is plain local
 * state, so the page's 10s status poll never resets it.
 *
 * @param props - The saved hunt entity (the blank placeholder renders a bare shell).
 * @returns The findings section.
 */
const HuntFindingsSection = ({ entity, editing }: DetailsSectionProps<Entities.Hunt>): JSX.Element => {
  const [view, setView] = useState<HuntFindingsView>('hits');
  const [rulesMounted, setRulesMounted] = useState(false);

  // resolve the engine's hit kind to its inline list; undefined hides the hits view
  const hitKind = huntHitKind(entity);
  const HitsList = hitKind !== undefined ? INLINE_HIT_LISTS[hitKind] : undefined;

  // the details factory leaves the blank placeholder in place until the hunt lands;
  // render a bare shell so EntityList never mounts pinned to an empty HuntId
  if (entity.id === '') {
    return (
      <Card className="panel mt-4">
        <Card.Body>
          <LoadingSpinner loading={true} />
        </Card.Body>
      </Card>
    );
  }

  // an engine with no inline hit list (a newer server during a rolling upgrade) keeps
  // the frozen rules panel as the whole section, with no tab to a dead hits view
  if (!HitsList) {
    return <HuntFrozenRulesSection entity={entity} editing={editing} />;
  }

  const showingHits = view === 'hits';
  return (
    <div data-testid="hunt-findings">
      <Tabs<HuntFindingsView>
        tabs={huntFindingsTabs(entity.metadata.Hunt.rules.length)}
        active={view}
        onChange={(key) => {
          // mount the rules panel on its first visit, then only hide it (see docstring)
          if (key === 'rules') {
            setRulesMounted(true);
          }
          setView(key);
        }}
        aria-label="Hunt findings sections"
        className="mt-4"
      />
      {/* the panels carry the tabpanel ids the Tabs' aria-controls point at */}
      <div role="tabpanel" id="tabpanel-hits" aria-labelledby="tab-hits" className={showingHits ? '' : 'd-none'}>
        <Card className="panel mt-4" data-testid="hunt-hits">
          <Card.Body>
            <HitsList hunt={entity} />
          </Card.Body>
        </Card>
      </div>
      {rulesMounted && (
        <div role="tabpanel" id="tabpanel-rules" aria-labelledby="tab-rules" className={showingHits ? 'd-none' : ''}>
          <HuntFrozenRulesSection entity={entity} editing={editing} showTitle={false} />
        </div>
      )}
    </div>
  );
};

export default HuntFindingsSection;
