import React from 'react';
import { Link } from 'react-router-dom';

import { useIncidentData } from './IncidentDataProvider';
import { Tile, TileHeader, ScrollableBody, Table, Th, Td, TrClickable, Mono, EmptyState, LoadingContainer, Spinner } from './styles';

function formatNames(submissions: { name?: string }[]): string {
  const names = submissions.map((s) => s.name).filter(Boolean);
  const unique = [...new Set(names)];
  if (unique.length === 0) return '(unnamed)';
  const joined = unique.join(', ');
  return joined.length > 60 ? joined.substring(0, 57) + '...' : joined;
}

function truncateHash(hash: string): string {
  return hash.length > 16 ? hash.substring(0, 8) + '...' + hash.substring(hash.length - 8) : hash;
}

const FileListPanel: React.FC = () => {
  const { files, loading } = useIncidentData();

  return (
    <Tile>
      <TileHeader>Files ({files.length})</TileHeader>
      {loading && files.length === 0 ? (
        <LoadingContainer>
          <Spinner />
          Loading files...
        </LoadingContainer>
      ) : files.length === 0 ? (
        <EmptyState>No files discovered in this incident graph</EmptyState>
      ) : (
        <ScrollableBody>
          <Table>
            <thead>
              <tr>
                <Th>Name</Th>
                <Th>SHA256</Th>
                <Th>Submissions</Th>
              </tr>
            </thead>
            <tbody>
              {files.map((file) => (
                <TrClickable key={file.sha256} as={Link as any} to={`/file/${file.sha256}`}>
                  <Td>{formatNames(file.submissions)}</Td>
                  <Td>
                    <Mono title={file.sha256}>{truncateHash(file.sha256)}</Mono>
                  </Td>
                  <Td>{file.submissions.length}</Td>
                </TrClickable>
              ))}
            </tbody>
          </Table>
        </ScrollableBody>
      )}
    </Tile>
  );
};

export default FileListPanel;
