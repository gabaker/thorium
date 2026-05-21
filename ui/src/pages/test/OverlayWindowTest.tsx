import { useEffect, useRef, useState } from 'react';
import styled from 'styled-components';

// project imports
import Page from '@components/pages/Page';
import Title from '@components/shared/titles/Title';
import Subtitle from '@components/shared/titles/Subtitle';
import { OverlayWindow } from '@components/shared/windows/OverlayWindow';
import { Placement, PositionType } from '@components/shared/windows';

const Section = styled.div`
  margin-bottom: 2rem;
`;

const AnchorButton = styled.button`
  padding: 8px 16px;
  border: 1px solid var(--thorium-panel-border);
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  border-radius: 4px;
  cursor: pointer;
  margin-right: 8px;
`;

const WindowContent = styled.div`
  padding: 8px;
  font-size: 14px;
`;

const StatusBar = styled.div`
  margin-top: 1rem;
  padding: 8px;
  background: var(--thorium-secondary-panel-bg);
  border-radius: 4px;
  font-size: 13px;
  font-family: monospace;
`;

interface WindowVisibility {
  fixedCenter: boolean;
  fixedBottomRight: boolean;
  fixedCustom: boolean;
  absoluteRef: boolean;
  absoluteCustom: boolean;
  fixedNoDrag: boolean;
}

const OverlayWindowTest = () => {
  const [visibility, setVisibility] = useState<WindowVisibility>({
    fixedCenter: false,
    fixedBottomRight: false,
    fixedCustom: false,
    absoluteRef: false,
    absoluteCustom: false,
    fixedNoDrag: false,
  });

  const [closedWindows, setClosedWindows] = useState<string[]>([]);

  // ref for the anchor button used in absolute+parentRef test
  const anchorRef = useRef<HTMLButtonElement>(null);

  const toggle = (key: keyof WindowVisibility) => {
    setVisibility((prev) => ({ ...prev, [key]: !prev[key] }));
  };

  // expose helpers on window for E2E tests
  useEffect(() => {
    (window as unknown as Record<string, unknown>)['__overlayWindowTestHelpers'] = {
      toggle: (key: string) => toggle(key as keyof WindowVisibility),
      getVisibility: () => visibility,
      getClosedWindows: () => closedWindows,
    };
    return () => {
      delete (window as unknown as Record<string, unknown>)['__overlayWindowTestHelpers'];
    };
  }, [visibility, closedWindows]);

  const onHide = (windowName: string) => {
    setVisibility((prev) => ({ ...prev, [windowName]: false }));
    setClosedWindows((prev) => [...prev, windowName]);
  };

  return (
    <Page title="OverlayWindow Test">
      <Title>OverlayWindow Component Test</Title>

      <Section>
        <Subtitle>Controls</Subtitle>
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: '8px' }}>
          <AnchorButton data-testid="toggle-fixed-center" onClick={() => toggle('fixedCenter')}>
            Fixed Center {visibility.fixedCenter ? '(ON)' : '(OFF)'}
          </AnchorButton>
          <AnchorButton data-testid="toggle-fixed-bottom-right" onClick={() => toggle('fixedBottomRight')}>
            Fixed BottomRight {visibility.fixedBottomRight ? '(ON)' : '(OFF)'}
          </AnchorButton>
          <AnchorButton data-testid="toggle-fixed-custom" onClick={() => toggle('fixedCustom')}>
            Fixed Custom {visibility.fixedCustom ? '(ON)' : '(OFF)'}
          </AnchorButton>
          <AnchorButton data-testid="toggle-absolute-ref" ref={anchorRef} onClick={() => toggle('absoluteRef')}>
            Absolute + Ref {visibility.absoluteRef ? '(ON)' : '(OFF)'}
          </AnchorButton>
          <AnchorButton data-testid="toggle-absolute-custom" onClick={() => toggle('absoluteCustom')}>
            Absolute Custom {visibility.absoluteCustom ? '(ON)' : '(OFF)'}
          </AnchorButton>
          <AnchorButton data-testid="toggle-fixed-no-drag" onClick={() => toggle('fixedNoDrag')}>
            Fixed No-Drag {visibility.fixedNoDrag ? '(ON)' : '(OFF)'}
          </AnchorButton>
        </div>
      </Section>

      <StatusBar data-testid="closed-windows">Closed: {closedWindows.join(', ') || 'none'}</StatusBar>

      {/* Fixed + Center */}
      <OverlayWindow
        show={visibility.fixedCenter}
        positioning={PositionType.Fixed}
        placement={Placement.Center}
        title="Fixed Center"
        width={300}
        height={200}
        onHide={() => onHide('fixedCenter')}
        className="test-window-fixed-center"
      >
        <WindowContent data-testid="window-fixed-center">Fixed positioning, centered in viewport.</WindowContent>
      </OverlayWindow>

      {/* Fixed + BottomRight */}
      <OverlayWindow
        show={visibility.fixedBottomRight}
        positioning={PositionType.Fixed}
        placement={Placement.BottomRight}
        title="Fixed BottomRight"
        width={280}
        height={180}
        onHide={() => onHide('fixedBottomRight')}
        className="test-window-fixed-bottom-right"
      >
        <WindowContent data-testid="window-fixed-bottom-right">Fixed positioning, bottom-right corner.</WindowContent>
      </OverlayWindow>

      {/* Fixed + Custom position */}
      <OverlayWindow
        show={visibility.fixedCustom}
        positioning={PositionType.Fixed}
        placement={Placement.Custom}
        customPosition={{ top: 100, left: 150 }}
        title="Fixed Custom Position"
        width={320}
        height={220}
        onHide={() => onHide('fixedCustom')}
        className="test-window-fixed-custom"
      >
        <WindowContent data-testid="window-fixed-custom">
          Fixed positioning at explicit coordinates (top: 100, left: 150).
        </WindowContent>
      </OverlayWindow>

      {/* Absolute + parentRef */}
      <OverlayWindow
        show={visibility.absoluteRef}
        positioning={PositionType.Absolute}
        placement={Placement.Bottom}
        parentRef={anchorRef}
        title="Absolute + Ref"
        width={300}
        height={200}
        onHide={() => onHide('absoluteRef')}
        className="test-window-absolute-ref"
      >
        <WindowContent data-testid="window-absolute-ref">
          Absolute positioning relative to the &quot;Absolute + Ref&quot; button.
        </WindowContent>
      </OverlayWindow>

      {/* Absolute + Custom position */}
      <OverlayWindow
        show={visibility.absoluteCustom}
        positioning={PositionType.Absolute}
        placement={Placement.Custom}
        customPosition={{ top: 400, left: 500 }}
        title="Absolute Custom Position"
        width={300}
        height={200}
        onHide={() => onHide('absoluteCustom')}
        className="test-window-absolute-custom"
      >
        <WindowContent data-testid="window-absolute-custom">
          Absolute positioning at explicit document coordinates (top: 400, left: 500).
        </WindowContent>
      </OverlayWindow>

      {/* Fixed + non-draggable */}
      <OverlayWindow
        show={visibility.fixedNoDrag}
        positioning={PositionType.Fixed}
        placement={Placement.TopLeft}
        locked={true}
        title="Fixed No-Drag"
        width={260}
        height={160}
        onHide={() => onHide('fixedNoDrag')}
        className="test-window-fixed-no-drag"
      >
        <WindowContent data-testid="window-fixed-no-drag">Fixed positioning, top-left, not draggable.</WindowContent>
      </OverlayWindow>
    </Page>
  );
};

export default OverlayWindowTest;
