import React, { useRef } from 'react';
import { Overlay, Popover } from 'react-bootstrap';

import type { SectionKey } from './types';
import { ToolbarIconButton, StyledPopover } from './Toolbar.styled';
import { OverlayTipTop } from '@components/shared/overlay/tips';

interface ToolbarButtonProps {
  sectionKey: SectionKey;
  activeSection: SectionKey | null;
  onToggle: (key: SectionKey) => void;
  icon: React.ReactNode;
  title: string;
  children: React.ReactNode;
}

const ToolbarButton: React.FC<ToolbarButtonProps> = ({ sectionKey, activeSection, onToggle, icon, title, children }) => {
  const buttonRef = useRef<HTMLButtonElement>(null);
  const isOpen = activeSection === sectionKey;

  const button = (
    <ToolbarIconButton ref={buttonRef} $active={isOpen} onClick={() => onToggle(sectionKey)}>
      {icon}
    </ToolbarIconButton>
  );

  return (
    <>
      {isOpen ? button : <OverlayTipTop tip={title}>{button}</OverlayTipTop>}

      <Overlay target={buttonRef.current} show={isOpen} placement="top">
        {(props) => (
          <StyledPopover {...props} id={`popover-${sectionKey}`}>
            <Popover.Header as="h6">{title}</Popover.Header>
            <Popover.Body>{children}</Popover.Body>
          </StyledPopover>
        )}
      </Overlay>
    </>
  );
};

export default ToolbarButton;
