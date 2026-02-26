import styled, { css, keyframes } from 'styled-components';
import { Card } from 'react-bootstrap';

export const UserCardWrapper = styled(Card)`
  margin-top: 0.25rem;
`;

export const UserHeaderRow = styled.div`
  display: flex;
  align-items: center;
  padding: 0.5rem;
  gap: 0.75rem;
`;

export const UsernameCol = styled.div`
  width: 160px;
  flex-shrink: 0;
`;

export const RoleCol = styled.div`
  width: 80px;
  flex-shrink: 0;
`;

export const Username = styled.h5`
  margin: 0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
`;

export const RoleBadge = styled.small`
  font-style: italic;
  color: var(--thorium-secondary-text);
`;

export const GroupsContainer = styled.div`
  flex: 1;
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem;
  min-width: 0;
`;

export const ActionsContainer = styled.div`
  display: flex;
  gap: 0.25rem;
  margin-left: auto;
  flex-shrink: 0;
`;

const slideDown = keyframes`
  from {
    max-height: 0;
    opacity: 0;
  }
  to {
    max-height: 600px;
    opacity: 1;
  }
`;

const slideUp = keyframes`
  from {
    max-height: 600px;
    opacity: 1;
  }
  to {
    max-height: 0;
    opacity: 0;
  }
`;

export const EditPanelWrapper = styled.div<{ $isOpen: boolean; $isClosing: boolean }>`
  overflow: hidden;

  ${({ $isOpen, $isClosing }) => {
    if ($isClosing) {
      return css`
        animation: ${slideUp} 0.3s ease-out forwards;
      `;
    }
    if ($isOpen) {
      return css`
        animation: ${slideDown} 0.3s ease-out forwards;
      `;
    }
    return css`
      max-height: 0;
      opacity: 0;
    `;
  }}
`;

export const EditPanelContent = styled.div`
  padding: 0.75rem 1rem;
  border-top: 1px solid var(--thorium-border-color, #dee2e6);
  display: flex;
  flex-direction: column;
  gap: 0.35rem;
`;

export const EditSection = styled.div`
  display: flex;
  align-items: center;
  gap: 0.75rem;

  label {
    min-width: 80px;
    font-weight: 600;
    margin: 0;
  }
`;

export const EditSubSection = styled.div`
  padding-left: calc(80px + 0.75rem);
`;

export const DeveloperOptionsRow = styled.div`
  display: flex;
  gap: 1.5rem;
  flex-wrap: wrap;
  padding-left: calc(80px + 0.75rem);
`;

export const PasswordInputWrapper = styled.div`
  position: relative;
  max-width: 300px;

  .password-toggle {
    position: absolute;
    right: 6px;
    top: 50%;
    transform: translateY(-50%);
    background: none;
    border: none;
    padding: 2px 4px;
    cursor: pointer;
    color: var(--thorium-secondary-text);
    display: flex;
    align-items: center;

    &:hover {
      color: var(--thorium-text);
    }
  }

  input {
    padding-right: 30px;
  }
`;

export const EditPanelActions = styled.div`
  display: flex;
  justify-content: center;
  gap: 0.25rem;
  padding-top: 0.5rem;
  border-top: 1px solid var(--thorium-border-color, #dee2e6);
`;
