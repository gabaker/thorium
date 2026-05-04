import React, { useRef } from 'react';
import { Overlay, Popover } from 'react-bootstrap';

import type { SectionKey } from './types';
import { ToolbarIconButton } from './Toolbar.styled';

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

  return (
    <>
      <ToolbarIconButton ref={buttonRef} $active={isOpen} onClick={() => onToggle(sectionKey)} title={title}>
        {icon}
      </ToolbarIconButton>

      <Overlay target={buttonRef.current} show={isOpen} placement="top" rootClose onHide={() => onToggle(sectionKey)}>
        {(props) => (
          <Popover {...props} id={`popover-${sectionKey}`} className="panel">
            <Popover.Header as="h6">{title}</Popover.Header>
            <Popover.Body>{children}</Popover.Body>
          </Popover>
        )}
      </Overlay>
    </>
  );
};

export default ToolbarButton;
