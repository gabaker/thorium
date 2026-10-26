import { MdRefresh } from 'react-icons/md';
import styled, { css, keyframes } from 'styled-components';

// project imports
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';

const spin = keyframes`
  to {
    transform: rotate(360deg);
  }
`;

// The refresh icon turns while a refresh is in flight; users who ask for reduced motion get a still icon.
const SpinningIcon = styled(MdRefresh)<{ $spinning: boolean }>`
  ${({ $spinning }) =>
    $spinning &&
    css`
      @media (prefers-reduced-motion: no-preference) {
        animation: ${spin} 0.9s linear infinite;
      }
    `}
`;

type ProfileRefreshButtonProps = {
  /// Whether a refresh is in flight
  refreshing: boolean;
  /// Called when the user asks for a refresh
  onRefresh: () => void;
};

/**
 * Button that reloads the profile information and token list in place.
 *
 * @param refreshing - Whether a refresh is in flight; the button is disabled and shows progress while it is.
 * @param onRefresh - Called when the user clicks the button.
 */
const ProfileRefreshButton: React.FC<ProfileRefreshButtonProps> = ({ refreshing, onRefresh }) => (
  <Button variant={ButtonVariant.Secondary} size={ButtonSize.Small} onClick={onRefresh} disabled={refreshing} aria-busy={refreshing}>
    <SpinningIcon $spinning={refreshing} size={16} aria-hidden="true" />
    {refreshing ? 'Refreshing…' : 'Refresh'}
  </Button>
);

export default ProfileRefreshButton;
