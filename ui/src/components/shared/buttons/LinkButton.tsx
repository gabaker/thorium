import { forwardRef } from 'react';
import { Link, LinkProps } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import { buttonStyles } from './Button';
import { ButtonSize, ButtonVariant } from './types';

/** The styled in-app link. */
const StyledLink = styled(Link)<{ $variant: ButtonVariant; $size: ButtonSize }>`
  ${buttonStyles}

  &:hover {
    text-decoration: none;
  }
`;

/** Props for {@link LinkButton}. */
export interface LinkButtonProps extends LinkProps {
  /** The button colour, Primary unless given */
  variant?: ButtonVariant;
  /** The button size, Medium unless given */
  size?: ButtonSize;
}

/** An in-app link drawn as a {@link Button}, so it can be middle-clicked, opened in a new tab, and copied. */
const LinkButton = forwardRef<HTMLAnchorElement, LinkButtonProps>(
  ({ variant = ButtonVariant.Primary, size = ButtonSize.Medium, ...rest }, ref) => (
    <StyledLink ref={ref} $variant={variant} $size={size} {...rest} />
  ),
);

LinkButton.displayName = 'LinkButton';

export default LinkButton;
