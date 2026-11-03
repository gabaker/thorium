import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { groupKeys, groupNamesToRead, readableGroups, useGroupDetailsQuery } from './groups';
import { createTestQueryClient, withQueryClient } from '@utilities/testing/queryClient';
import { Group } from '@models/groups';

/**
 * Build a group whose details were read in full.
 *
 * @param name - The group's name.
 * @returns The group.
 */
function group(name: string): Group {
  const role = { combined: [], direct: [], metagroups: [] };
  return { name, owners: role, managers: role, users: role, monitors: role, allowed: {} } as unknown as Group;
}

/**
 * Render the names of the groups the hook read.
 *
 * @param props - The probe's props.
 * @param props.names - The group names to read.
 * @returns The names, comma separated.
 */
const Probe = ({ names }: { names: string[] }) => (
  <>
    {useGroupDetailsQuery(names)
      .map((entry) => entry.name)
      .join(',')}
  </>
);

describe('group details queries', () => {
  it('reads each named group once, in a stable order', () => {
    expect(groupNamesToRead(['b', 'a', 'b', ''])).toEqual(['a', 'b']);
  });
  it('leaves out a group that could not be read or is still being read', () => {
    expect(readableGroups([group('a'), null, undefined]).map((entry) => entry.name)).toEqual(['a']);
  });
  it('reads cached details from the shared query cache', () => {
    const client = createTestQueryClient();
    client.setQueryData(groupKeys.details('a'), group('a'));
    client.setQueryData(groupKeys.details('b'), null);
    expect(renderToStaticMarkup(withQueryClient(<Probe names={['b', 'a']} />, client))).toBe('a');
  });
});
