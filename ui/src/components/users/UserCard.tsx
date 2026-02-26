import React, { useState } from 'react';
import { Badge } from 'react-bootstrap';
import { ThoriumRole } from '@models';
import { getThoriumRole } from '@utilities';
import EditUserPanel from './EditUserPanel';
import UserActions from './UserActions';
import { GroupsContainer, RoleBadge, RoleCol, UserCardWrapper, UserHeaderRow, Username, UsernameCol } from './styles';

interface UserCardProps {
  username: string;
  role: ThoriumRole;
  email: string;
  groups: string[];
  token: string;
  expires: string;
  theme: string;
  verified: boolean;
  local: boolean;
  onDelete: () => void;
  onMasquerade: (token: string, expires: string) => void;
}

const UserCard: React.FC<UserCardProps> = ({
  username,
  role,
  email,
  groups,
  token,
  expires,
  theme,
  verified,
  local,
  onDelete,
  onMasquerade,
}) => {
  const [currentRole, setCurrentRole] = useState<ThoriumRole>(role);
  const [isEditing, setIsEditing] = useState(false);

  return (
    <UserCardWrapper className="panel">
      <UserHeaderRow>
        <UsernameCol>
          <Username className="text">{username}</Username>
        </UsernameCol>
        <RoleCol>
          <RoleBadge>{getThoriumRole(currentRole)}</RoleBadge>
        </RoleCol>
        <GroupsContainer>
          {[...groups].sort().map((group) => (
            <Badge bg="" key={group} className="bg-cadet">
              {group}
            </Badge>
          ))}
        </GroupsContainer>
        <UserActions
          username={username}
          token={token}
          expires={expires}
          isEditing={isEditing}
          onEditToggle={() => setIsEditing((prev) => !prev)}
          onDelete={onDelete}
          onMasquerade={onMasquerade}
        />
      </UserHeaderRow>
      <EditUserPanel
        username={username}
        role={currentRole}
        email={email}
        theme={theme}
        verified={verified}
        local={local}
        isOpen={isEditing}
        onRoleUpdated={(newRole) => setCurrentRole(newRole)}
        onClose={() => setIsEditing(false)}
      />
    </UserCardWrapper>
  );
};

export default UserCard;
