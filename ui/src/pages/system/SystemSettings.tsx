import { useEffect, useState } from 'react';
import { Table } from 'react-bootstrap';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import styled from 'styled-components';

// project imports
import Page from '@components/pages/Page';
import Subtitle from '@components/shared/titles/Subtitle';
import Title from '@components/shared/titles/Title';
import { getSystemSettings } from '@thorpi/system';
import { formatSettingLines, formatSettingValue, isEmptySetting, isStructuredSetting } from '@utilities/settings';
import type { SystemSettings as SystemSettingsType } from '@models/system';

const Settings = styled.div`
  display: flex;
  align-items: center;
  flex-direction: column;
  padding-top: 10px;
`;

/** A list or nested setting, left aligned with the values' own case kept (base URLs are case sensitive). */
const StructuredValue = styled.div`
  text-align: left;
  font-family: monospace;
  font-size: 0.85rem;
  color: var(--thorium-text);
`;

/** The width one level of nesting indents a line, and how far a wrapped line hangs past the first one. */
const INDENT = 1.5;

/**
 * One line of a structured setting. It is its own block so a line that wraps keeps a hanging indent: the
 * continuation sits one step in from the line's first character, which keeps one long value from looking like
 * two entries. The pre-wrap keeps the two spaces between a provider and its base URL.
 */
const SettingLineText = styled.span<{ $depth: number }>`
  display: block;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  padding-left: ${({ $depth }) => ($depth + 1) * INDENT}rem;
  text-indent: -${INDENT}rem;
`;

const SystemSettings = () => {
  const [getSettingsError, setGetSettingsError] = useState('');
  const [systemSettings, setSystemSettings] = useState<SystemSettingsType>({});

  // fetch latest settings from API
  const fetchSettings = async () => {
    const settings = await getSystemSettings(setGetSettingsError);
    if (settings) {
      setSystemSettings(settings);
    }
  };

  useEffect(() => {
    void fetchSettings();
  }, []);

  return (
    <Page title="Settings · Thorium" className="settings">
      <Settings>
        {getSettingsError != '' && <AlertBanner>{getSettingsError}</AlertBanner>}
        <Title>System Settings</Title>
        <Table striped bordered hover>
          <tbody>
            {Object.keys(systemSettings).map((setting) => (
              <tr key={setting}>
                <td>
                  <center>
                    <Subtitle>{setting}</Subtitle>
                  </center>
                </td>
                {isStructuredSetting(systemSettings[setting]) && !isEmptySetting(systemSettings[setting]) ? (
                  <td data-testid={`setting-${setting}`}>
                    <StructuredValue>
                      {formatSettingLines(systemSettings[setting]).map(({ depth, text }, index) => (
                        // a line can repeat within a setting, so its position makes the key unique
                        <SettingLineText key={`${index}-${text}`} $depth={depth} data-testid="setting-line">
                          {text}
                        </SettingLineText>
                      ))}
                    </StructuredValue>
                  </td>
                ) : (
                  <td data-testid={`setting-${setting}`}>
                    <center>
                      <Subtitle>
                        {isEmptySetting(systemSettings[setting]) ? '(none)' : formatSettingValue(systemSettings[setting])}
                      </Subtitle>
                    </center>
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </Table>
      </Settings>
    </Page>
  );
};

export default SystemSettings;
