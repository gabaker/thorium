import { useEffect, useMemo, useState, Fragment } from 'react';
import { Col, Form, Row, Table } from 'react-bootstrap';
import styled from 'styled-components';
import AlertBanner from '@components/shared/alerts/AlertBanner';

// project imports
import { useAgenticList } from '@components/agentic/hooks';
import { EntityLink, Mono, TableScroll } from '@components/agentic/shared';
import Page from '@components/pages/Page';
import Subtitle from '@components/shared/titles/Subtitle';
import Title from '@components/shared/titles/Title';
import { getSystemStats } from '@thorpi/system';
import { emptyGroupStatsText, flattenGroupStats, GroupStatsRow, indexWorkloadInstances, matchesGroupStatsSearch } from '@utilities/stats';
import { AgenticKinds } from '@models/agentic';
import { GroupsStats, Stats } from '@models/system';
import { ToolInstance } from '@models/tools';

/**
 * Scrolls the workloads table sideways as a last resort, with cell padding slim enough that its nine columns
 * fit the page at 1024 pixels wide.
 */
const WorkloadScroll = styled(TableScroll)`
  && th,
  && td {
    padding-left: 0.35rem;
    padding-right: 0.35rem;
  }
`;

interface GroupStatsProps {
  stats: GroupsStats;
}

const SelectableGroupStats: React.FC<GroupStatsProps> = ({ stats }) => {
  const [searchQuery, setSearchQuery] = useState('');
  const [sortConfig, setSortConfig] = useState<{ key: keyof GroupStatsRow; direction: string }>({ key: 'group', direction: 'ascending' });
  // the instances the workloads run, so each row can name its tool instead of showing a bare id
  const instanceList = useAgenticList<ToolInstance>(AgenticKinds.ToolInstance);
  const instances = useMemo(() => indexWorkloadInstances(instanceList.items), [instanceList.items]);
  // the name a workload's instance is shown and searched by (its id when the instance isn't visible)
  const instanceLabel = (id: string) => instances.get(id)?.name ?? id;

  const getSortedData = (data: GroupStatsRow[]) => {
    const sortedData = [...data];
    // sort the tool instance column by the name shown rather than by id
    const sortValue = (row: GroupStatsRow) => (sortConfig.key === 'toolInstance' ? instanceLabel(row.toolInstance) : row[sortConfig.key]);
    sortedData.sort((a, b) => {
      if (sortValue(a) < sortValue(b)) {
        return sortConfig.direction === 'ascending' ? -1 : 1;
      }
      if (sortValue(a) > sortValue(b)) {
        return sortConfig.direction === 'ascending' ? 1 : -1;
      }
      return 0;
    });
    return sortedData;
  };

  const handleSort = (key: keyof GroupStatsRow) => {
    let direction = 'ascending';
    if (sortConfig.key === key && sortConfig.direction === 'ascending') {
      direction = 'descending';
    }
    setSortConfig({ key, direction });
  };

  const filteredData = stats ? flattenGroupStats(stats) : [];

  const sortedData = getSortedData(
    filteredData.filter((row) => matchesGroupStatsSearch(row, searchQuery, instanceLabel(row.toolInstance))),
  );

  // the message to show in place of rows when the table is empty
  const emptyText = sortedData.length === 0 ? emptyGroupStatsText(filteredData.length, searchQuery) : '';
  if (!stats) {
    return null;
  } else {
    return (
      <>
        <Form.Group>
          <Form.Control type="text" placeholder="Search..." value={searchQuery} onChange={(e) => setSearchQuery(e.target.value)} />
        </Form.Group>
        <Row>
          <Col>
            <WorkloadScroll>
              <Table striped bordered className="mt-2">
                <thead>
                  <tr>
                    <th onClick={() => handleSort('group')}>Group</th>
                    <th onClick={() => handleSort('toolInstance')}>Tool Instance</th>
                    <th onClick={() => handleSort('user')}>User</th>
                    <th onClick={() => handleSort('created')}>Created</th>
                    <th onClick={() => handleSort('running')}>Running</th>
                    <th onClick={() => handleSort('completed')}>Completed</th>
                    <th onClick={() => handleSort('failed')}>Failed</th>
                    <th onClick={() => handleSort('sleeping')}>Sleeping</th>
                    <th onClick={() => handleSort('total')}>Total</th>
                  </tr>
                </thead>
                <tbody>
                  {emptyText !== '' && (
                    <tr>
                      <td colSpan={9} className="text-center" data-testid="stats-workload-empty">
                        {emptyText}
                      </td>
                    </tr>
                  )}
                  {sortedData.map((row, index) => (
                    <tr key={index}>
                      <td>{row.group}</td>
                      <td data-testid="workload-instance">
                        {instances.get(row.toolInstance) ? (
                          <EntityLink to={`/tools/${instances.get(row.toolInstance)?.toolId}`}>
                            {instanceLabel(row.toolInstance)}
                          </EntityLink>
                        ) : (
                          <Mono>{row.toolInstance}</Mono>
                        )}
                      </td>
                      <td>{row.user}</td>
                      <td>{row.created}</td>
                      <td>{row.running}</td>
                      <td>{row.completed}</td>
                      <td>{row.failed}</td>
                      <td>{row.sleeping}</td>
                      <td>{row.total}</td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            </WorkloadScroll>
          </Col>
        </Row>
      </>
    );
  }
};

interface GlobalStatsProps {
  stats: Stats;
}

const GlobalStats: React.FC<GlobalStatsProps> = ({ stats }) => {
  return (
    <Table striped bordered>
      <tbody>
        <tr>
          <td>
            <Subtitle>Deadlines</Subtitle>
          </td>
          <td>
            <center>
              <Subtitle>{stats.deadlines}</Subtitle>
            </center>
          </td>
        </tr>
        <tr>
          <td>
            <Subtitle>Running</Subtitle>
          </td>
          <td>
            <center>
              <Subtitle>{stats.running}</Subtitle>
            </center>
          </td>
        </tr>
        <tr>
          <td>
            <Subtitle>Users</Subtitle>
          </td>
          <td>
            <center>
              <Subtitle>{stats.users}</Subtitle>
            </center>
          </td>
        </tr>
      </tbody>
    </Table>
  );
};

const ScalerStats: React.FC<GlobalStatsProps> = ({ stats }) => {
  const scalers: Array<keyof Pick<Stats, 'k8s' | 'baremetal' | 'external'>> = ['k8s', 'baremetal', 'external'];

  return (
    <Table striped bordered>
      <tbody>
        {scalers.map((scaler) => (
          <Fragment key={scaler}>
            <tr>
              <td rowSpan={2}>
                <Subtitle>{scaler}</Subtitle>
              </td>
              <td>
                <center>
                  <Subtitle>Deadlines</Subtitle>
                </center>
              </td>
              <td>
                <center>
                  <Subtitle>{stats[scaler].deadlines}</Subtitle>
                </center>
              </td>
            </tr>
            <tr>
              <td>
                <center>
                  <Subtitle>Running</Subtitle>
                </center>
              </td>
              <td>
                <center>
                  <Subtitle>{stats[scaler].running}</Subtitle>
                </center>
              </td>
            </tr>
          </Fragment>
        ))}
      </tbody>
    </Table>
  );
};

const SystemStats = () => {
  const [getStatsError, setGetStatsError] = useState('');
  const [systemStats, setSystemStats] = useState<Stats | null>(null);
  const fetchStats = async () => {
    const stats = await getSystemStats(setGetStatsError);
    if (stats) {
      setSystemStats(stats);
    }
  };

  // trigger fetch stats on initial page load
  useEffect(() => {
    void fetchStats();

    // set interval to rerun every 5 seconds
    const intervalId = setInterval(() => {
      void fetchStats();
    }, 10000);

    // cleanup interval on component unmount
    return () => {
      clearInterval(intervalId);
    };
  }, []);

  return (
    <Page title="Stats · Thorium">
      {getStatsError != '' && (
        <Row>
          <AlertBanner>{getStatsError}</AlertBanner>
        </Row>
      )}
      {systemStats != null && (
        <>
          <Row>
            <Col className="d-flex justify-content-center">
              <Title>System</Title>
            </Col>
          </Row>
          <Row>
            <Col className="d-flex justify-content-center">
              <GlobalStats stats={systemStats} />
            </Col>
          </Row>
          <Row>
            <Col className="d-flex justify-content-center">
              <Title>Scaler</Title>
            </Col>
          </Row>
          <Row>
            <Col className="d-flex justify-content-center">
              <ScalerStats stats={systemStats} />
            </Col>
          </Row>
          <Row>
            <Col className="d-flex justify-content-center">
              <Title>Workloads</Title>
            </Col>
          </Row>
          <Row>
            <Col>
              <SelectableGroupStats stats={systemStats.groups} />
            </Col>
          </Row>
        </>
      )}
    </Page>
  );
};

export default SystemStats;
