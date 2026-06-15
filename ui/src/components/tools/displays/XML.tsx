import { useState, useEffect } from 'react';
import { Card } from 'react-bootstrap';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import XMLViewer from 'react-xml-viewer';

// project imports
import { formatResultBody, getAlerts } from '../alerts';
import '@styles/main.scss';
import { useJsonTreeInvert } from '@components/shared/renderers/jsonTheme';
import { ResultRenderProps } from '../props';
import { Value } from '@models/results';

// spec: ../ToolResult.spec.md

// XMLViewer takes a fixed color palette rather than CSS variables, so provide two hand-tuned palettes.
// The dark palette is the original Ocean pastel set; the light palette darkens the same token roles so
// tags/text stay legible on the near-white/cream panel backgrounds of the Light and Crab themes.
const DARK_XML_THEME = {
  attributeKeyColor: '#96b5b4',
  attributeValueColor: '#d08770',
  tagColor: '#8fa1b3',
  textColor: '#a3be8c',
  separatorColor: 'tan',
};
const LIGHT_XML_THEME = {
  attributeKeyColor: '#2a6f6a',
  attributeValueColor: '#a1441f',
  tagColor: '#1f4b73',
  textColor: '#3d6b2f',
  separatorColor: '#7a5a20',
};

const XML: React.FC<ResultRenderProps> = ({ result }) => {
  const [errors, setErrors] = useState<string[]>([]);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [resultsJson, setResultsJson] = useState<Value>({});
  const [isJson, setIsJson] = useState(true);
  // light-background themes (Light/Crab) need the darkened palette to stay legible
  const useLightTheme = useJsonTreeInvert();

  useEffect(() => {
    // set alerts and process results to json
    getAlerts(result.result, setResultsJson, setWarnings, setErrors, setIsJson, true);
  }, [result]);

  const parsedResult = formatResultBody(result.result, isJson, resultsJson);
  const thoriumTheme = useLightTheme ? LIGHT_XML_THEME : DARK_XML_THEME;

  return (
    <>
      <Card className="scroll-log tool-result">
        <Card.Body>
          {errors.map((err, idx) => (
            <AlertBanner key={idx}>{err}</AlertBanner>
          ))}
          {warnings.map((warn, idx) => (
            <AlertBanner key={idx} severity={Severity.Warning}>
              {warn}
            </AlertBanner>
          ))}
          <XMLViewer xml={parsedResult} theme={thoriumTheme} collapsible={true} initialCollapsedDepth={3} />
        </Card.Body>
      </Card>
    </>
  );
};

export default XML;
