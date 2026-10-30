import styled from 'styled-components';

// project imports
import { Badge } from '@components/shared/badges/Badge';
import { OverlayTipBottom } from '@components/shared/overlay/tips';
import { getGroupRoleBadge } from '@utilities/role';
import { Group } from '@models/groups';
import { UserInfo } from '@models/users';

/** A role badge of one width, so the badges line up down the group list. */
const RoleBadge = styled(Badge)`
  width: 75px;
`;

/** Props for {@link GroupRoleBadge}. */
interface GroupRoleBadgeProps {
  /** The group whose role to show */
  group: Group;
  /** The user whose role in the group is shown */
  user: UserInfo;
}

/** The user's role in a group as a badge in the role's colour, with a tooltip saying what the role may do. */
const GroupRoleBadge: React.FC<GroupRoleBadgeProps> = ({ group, user }) => {
  const badge = getGroupRoleBadge(group, user);
  // the user has no role in this group and is not an admin
  if (!badge) {
    return null;
  }
  return (
    <OverlayTipBottom tip={badge.tooltip}>
      <RoleBadge color={badge.color} data-testid="group-role-badge">
        {badge.label}
      </RoleBadge>
    </OverlayTipBottom>
  );
};

export default GroupRoleBadge;
