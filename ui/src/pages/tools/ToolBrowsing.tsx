import { FC, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import { deprecatedCount, filterTools, NO_TOOL_FILTER, ToolFilter } from '@components/agentic/tools/filters';
import { useAgenticList, useCreateBlockReason, useGroupNames } from '@components/agentic/hooks';
import {
  Actions,
  Badge,
  BadgeTone,
  BannerSlot,
  CheckRow,
  CodeSpans,
  EntityLink,
  LinkRow,
  Muted,
  NoWrap,
  PageHead,
  PageTitle,
  Row,
  Section,
  Select,
  statusTone,
  Table,
  TableScroll,
} from '@components/agentic/shared';
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonVariant } from '@components/shared/buttons';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import TextInput from '@components/shared/inputs/TextInput';
import { countLabel, summarizeDescription, toolVersionLabel } from '@utilities/agentic/display';
import { scaling } from '@styles';
import { AgenticKinds } from '@models/agentic';
import { Tool, ToolKind } from '@models/tools';

/** A tool's description cut to two lines in a list row. */
const Description = styled.div`
  color: var(--thorium-secondary-text);
  max-width: 40rem;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  /* an unbreakable word such as a URL wraps instead of widening the table past its card */
  overflow-wrap: anywhere;
`;

/**
 * The modified cell: on a window wide enough for the whole timestamp it stays on one line so every row has the same
 * height, and below that the date and time may sit on two lines (see `ModifiedTime`) to leave the name column its room.
 */
const ModifiedCell = styled.td`
  @media (min-width: ${scaling.xl}) {
    white-space: nowrap;
  }
`;

/**
 * A modified timestamp in the compact `<date> <time>` form of the shared time display, with the date and the time
 * each kept whole so a narrow cell wraps between them and never inside the date at its hyphens.
 *
 * # Arguments
 *
 * * `props` - The ISO timestamp to show
 */
const ModifiedTime: FC<{ children: string }> = ({ children }) => {
  // split the ISO timestamp into its date and time parts
  const [date, fullTime] = children.split('T');
  // show a timestamp without a time part as it came
  if (!fullTime) return <>{children}</>;
  // drop fractional seconds and a trailing UTC designator as the shared time display does
  const time = fullTime.split('.')[0].replace(/Z$/, '');
  return (
    <>
      <NoWrap>{date}</NoWrap> <NoWrap>{time}</NoWrap>
    </>
  );
};

/** Browse the tools the user can see, with their kinds, latest versions, and groups. */
const ToolBrowsing: FC = () => {
  const navigate = useNavigate();
  const { items, loading, error } = useAgenticList<Tool>(AgenticKinds.Tool);
  const groups = useGroupNames();
  const createReason = useCreateBlockReason();
  const [filter, setFilter] = useState<ToolFilter>(NO_TOOL_FILTER);
  const shown = useMemo(() => filterTools(items, filter), [items, filter]);
  // a list that failed to load with nothing read says so in its banner, not with a zero count or an empty-state message
  const listFailed = error !== '' && items.length === 0;
  return (
    <Page title="Tools · Thorium">
      <PageHead>
        <PageTitle>Tools</PageTitle>
        <Actions>
          <Button
            variant={ButtonVariant.Ok}
            data-testid="create-tool"
            disabled={createReason !== ''}
            title={createReason || undefined}
            onClick={() => void navigate('/tools/create')}
          >
            + New tool
          </Button>
        </Actions>
      </PageHead>
      <BannerSlot>
        <AlertBanner severity={Severity.Info}>
          Tools replace images. Each tool has immutable versions (tool instances); publish a new version to change what runs. Images created
          before tools existed are still listed on the Images page; pipelines run tools.
        </AlertBanner>
        {error !== '' && (
          <AlertBanner severity={Severity.Error}>
            <CodeSpans text={error} />
          </AlertBanner>
        )}
      </BannerSlot>
      <Row style={{ marginBottom: '1rem' }}>
        <TextInput
          style={{ maxWidth: '20rem' }}
          placeholder="Filter by name, description, or version"
          aria-label="Filter tools by name, description, or version"
          data-testid="tool-filter-text"
          value={filter.text}
          onChange={(event) => setFilter({ ...filter, text: event.target.value })}
        />
        <Select
          style={{ maxWidth: '10rem' }}
          data-testid="tool-filter-kind"
          aria-label="Filter tools by kind"
          value={filter.kind}
          onChange={(event) => setFilter({ ...filter, kind: event.target.value as ToolKind | '' })}
        >
          <option value="">All kinds</option>
          {Object.values(ToolKind).map((kind) => (
            <option key={kind} value={kind}>
              {kind}
            </option>
          ))}
        </Select>
        <Select
          style={{ maxWidth: '12rem' }}
          data-testid="tool-filter-group"
          aria-label="Filter tools by group"
          value={filter.group}
          onChange={(event) => setFilter({ ...filter, group: event.target.value })}
        >
          <option value="">All groups</option>
          {groups.map((group) => (
            <option key={group} value={group}>
              {group}
            </option>
          ))}
        </Select>
        <CheckRow>
          <input
            type="checkbox"
            data-testid="tool-filter-live"
            checked={filter.hideDeprecated}
            onChange={(event) => setFilter({ ...filter, hideDeprecated: event.target.checked })}
          />
          Hide tools with no live version
        </CheckRow>
        {!listFailed && <Muted data-testid="tool-count">{countLabel(shown.length, 'tool')}</Muted>}
      </Row>
      <LoadingSpinner loading={loading} />
      {!loading && !listFailed && shown.length === 0 && <Muted data-testid="no-tools">No tools found.</Muted>}
      {shown.length > 0 && (
        <Section>
          {/* scrolls sideways instead of poking out of the card when the columns need more room than the window gives */}
          <TableScroll>
            <Table data-testid="tool-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Kind</th>
                  <th>Latest</th>
                  <th>Versions</th>
                  <th>Groups</th>
                  <th>Modified</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((tool) => {
                  const meta = tool.metadata.Tool;
                  const deprecated = deprecatedCount(tool);
                  const summary = summarizeDescription(tool.description);
                  return (
                    <LinkRow key={tool.id} to={`/tools/${tool.id}`} testId={`tool-row-${tool.name}`}>
                      <td>
                        {/* a hyphenated name stays on one line, as on the agent and toolset lists */}
                        <EntityLink to={`/tools/${tool.id}`}>
                          <NoWrap>{tool.name}</NoWrap>
                        </EntityLink>
                        {summary && <Description title={summarizeDescription(tool.description, 1000)}>{summary}</Description>}
                      </td>
                      <td>
                        <Badge $tone={BadgeTone.Info}>{meta.kind}</Badge>
                      </td>
                      <td>
                        {meta.latest_version ? (
                          <Badge>{toolVersionLabel(meta.latest_version)}</Badge>
                        ) : (
                          <Badge $tone={statusTone('no live version')}>no live version</Badge>
                        )}
                      </td>
                      <td>
                        {(meta.instances ?? []).length}
                        {/* the space sits outside the nowrap span so a narrow column wraps before the note, not inside it */}
                        {deprecated > 0 && (
                          <>
                            {' '}
                            <Muted style={{ whiteSpace: 'nowrap' }}>({deprecated} deprecated)</Muted>
                          </>
                        )}
                      </td>
                      <td>{tool.groups.join(', ')}</td>
                      {/* one line on a wide window; on a narrow one the date and time may sit on two lines, each staying whole */}
                      <ModifiedCell>
                        <ModifiedTime>{meta.modified}</ModifiedTime>
                      </ModifiedCell>
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

export default ToolBrowsing;
