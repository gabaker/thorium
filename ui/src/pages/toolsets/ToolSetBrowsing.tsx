import { FC } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import {
  Actions,
  BannerSlot,
  Badge,
  BadgeTone,
  EntityLink,
  FlexName,
  InlineList,
  LinkRow,
  Muted,
  NoWrap,
  PageHead,
  PageTitle,
  Section,
  ShortId,
  statusTone,
  Table,
  TableScroll,
  WrapBadge,
} from '@components/agentic/shared';
import ListErrorBanner from '@components/shared/alerts/ListErrorBanner';
import { useAgenticList } from '@components/agentic/hooks';
import { selectorText } from '@components/agentic/toolsets/forms';
import { Button } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import Page from '@components/pages/Page';
import { duplicateNames } from '@utilities/agentic/groups';
import { countLabel, instanceStatusText, isInstanceReady, isShortName, SHORT_NAME_LIMIT } from '@utilities/agentic/display';
import { agentsUsingToolSet } from '@utilities/agentic/usedBy';
import { AgenticKinds } from '@models/agentic';
import { Agent } from '@models/agents';
import { ToolSet, ToolSetInstance } from '@models/toolsets';

/** Props for {@link ToolSetName}. */
interface ToolSetNameProps {
  /** The toolset name, or an instance name that ends in a `#rN` revision suffix. */
  name: string;
}

/** A toolset or instance name that stays whole when short, so one long name wraps instead of widening the whole table. */
const ToolSetName: FC<ToolSetNameProps> = ({ name }) => {
  // an instance name ends in a `#rN` revision suffix, and a name without one is shown as a plain name
  const at = name.lastIndexOf('#');
  if (at <= 0) return <FlexName $short={isShortName(name)}>{name}</FlexName>;
  // a whole instance name that is short stays on one line, suffix included
  if (isShortName(name)) return <NoWrap>{name}</NoWrap>;
  // the last name segment travels with the revision suffix so a break never leaves the suffix alone on a line
  const tailStart = name.lastIndexOf('-', at) + 1;
  const tail = name.slice(tailStart);
  if (tailStart > 0 && tail.length <= SHORT_NAME_LIMIT) {
    return (
      <>
        <FlexName $short={isShortName(name.slice(0, tailStart))}>{name.slice(0, tailStart)}</FlexName>
        <NoWrap>{tail}</NoWrap>
      </>
    );
  }
  // with no hyphen to break at, the name may break right before the suffix, which never splits
  return (
    <>
      <FlexName $short={isShortName(name.slice(0, at))}>{name.slice(0, at)}</FlexName>
      <wbr />
      <NoWrap>{name.slice(at)}</NoWrap>
    </>
  );
};

/** List the toolsets the user can see with their settings and latest instance. */
const ToolSetBrowsing: FC = () => {
  const navigate = useNavigate();
  const sets = useAgenticList<ToolSet>(AgenticKinds.ToolSet);
  const instances = useAgenticList<ToolSetInstance>(AgenticKinds.ToolSetInstance);
  // agents are listed so each toolset can show how many of them use it
  const agents = useAgenticList<Agent>(AgenticKinds.Agent);
  // index instances by id so each toolset can show its latest instance's status and member count
  const byId = new Map(instances.items.map((item) => [item.id, item]));
  // toolsets that share a name show a short id so their rows can be told apart
  const repeated = duplicateNames(sets.items);
  return (
    <Page title="Toolsets · Thorium">
      <PageHead>
        <PageTitle>Toolsets</PageTitle>
        <Actions>
          <Button variant={ButtonVariant.Ok} data-testid="create-toolset" onClick={() => void navigate('/toolsets/create')}>
            + New toolset
          </Button>
        </Actions>
      </PageHead>
      <BannerSlot>
        {/* the page's own list reports first, then the lists that fill in its columns */}
        <ListErrorBanner errors={[sets.error, instances.error, agents.error]} className="" />
      </BannerSlot>
      <LoadingSpinner loading={sets.loading} />
      {!sets.loading && sets.error === '' && sets.items.length === 0 && <Muted data-testid="toolset-empty">No toolsets found.</Muted>}
      {/* nothing else shows in the list's place until it has loaded, and an empty list says so instead of showing a bare table */}
      {!sets.loading && sets.items.length > 0 && (
        <Section>
          {/* short names stay whole, so a window narrower than the table scrolls it instead of letting it spill out of the card */}
          {/* long names wrap so that one of them cannot widen every row */}
          <TableScroll>
            <Table data-testid="toolset-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Groups</th>
                  <th>Settings</th>
                  <th>Latest instance</th>
                </tr>
              </thead>
              <tbody>
                {sets.items.map((set) => {
                  const meta = set.metadata.ToolSet;
                  const latest = meta.latest_instance ? byId.get(meta.latest_instance) : undefined;
                  // with no Ready instance, the newest build is the one at the highest revision, which says why it can't be used
                  const newest = latest
                    ? undefined
                    : instances.items.find(
                        (item) =>
                          item.metadata.ToolSetInstance.tool_set === set.id &&
                          item.metadata.ToolSetInstance.revision === meta.highest_revision,
                      );
                  return (
                    <LinkRow key={set.id} to={`/toolsets/${set.id}`} testId={`toolset-row-${set.name}`}>
                      <td>
                        <EntityLink to={`/toolsets/${set.id}`}>
                          <ToolSetName name={set.name} />
                        </EntityLink>
                        <ShortId id={set.id} show={repeated.has(set.name)} />
                        {/* the agent count sits under the name rather than in a column of its own, so it adds no width to the table */}
                        <div data-testid={`toolset-used-by-${set.name}`}>
                          <Muted>
                            used by{' '}
                            {agents.loading || agents.error ? '-' : countLabel(agentsUsingToolSet(agents.items, set.id).length, 'agent')}
                          </Muted>
                        </div>
                      </td>
                      <td>{set.groups.join(', ')}</td>
                      <td>
                        <InlineList>
                          {meta.read_only && <Badge $tone={BadgeTone.Info}>read only</Badge>}
                          {meta.selector && <WrapBadge>selector {selectorText(meta.selector)}</WrapBadge>}
                          <Badge>{countLabel(meta.tools?.length ?? 0, 'explicit tool')}</Badge>
                          <Badge $tone={meta.auto_instance ? BadgeTone.Ok : BadgeTone.Neutral}>
                            auto {meta.auto_instance ? 'on' : 'off'}
                          </Badge>
                        </InlineList>
                      </td>
                      {/* a short instance name stays whole (no split at a hyphen), a long one wraps; status and members sit under it */}
                      <td data-testid={`toolset-latest-${set.name}`}>
                        {latest ? (
                          <>
                            <ToolSetName name={latest.name} />
                            <div>
                              <InlineList>
                                <WrapBadge tone={statusTone(isInstanceReady(latest.metadata.ToolSetInstance.status) ? 'Ready' : 'Invalid')}>
                                  {instanceStatusText(latest.metadata.ToolSetInstance.status)}
                                </WrapBadge>
                                <Badge>{countLabel(latest.metadata.ToolSetInstance.members.length, 'member')}</Badge>
                              </InlineList>
                            </div>
                          </>
                        ) : instances.error ? (
                          // a failed or cut off instance list cannot tell that no instance is Ready
                          <Muted>-</Muted>
                        ) : meta.highest_revision > 0 ? (
                          <>
                            <ToolSetName name={newest ? newest.name : `r${meta.highest_revision}`} />
                            <div>
                              <InlineList>
                                <WrapBadge tone={statusTone('no ready instance')}>
                                  {newest ? instanceStatusText(newest.metadata.ToolSetInstance.status) : 'not Ready'}
                                </WrapBadge>
                                {newest && <Badge>{countLabel(newest.metadata.ToolSetInstance.members.length, 'member')}</Badge>}
                              </InlineList>
                            </div>
                          </>
                        ) : (
                          <Muted>none</Muted>
                        )}
                      </td>
                    </LinkRow>
                  );
                })}
              </tbody>
            </Table>
          </TableScroll>
        </Section>
      )}
    </Page>
  );
};

export default ToolSetBrowsing;
