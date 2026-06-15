import { useState, useEffect } from 'react';
import { Card } from 'react-bootstrap';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import XMLViewer from 'react-xml-viewer';

// project imports
import { formatResultBody, getAlerts } from '../alerts';
import '@styles/main.scss';
import { ResultRenderProps } from '../props';
import { Value } from '@models/results';

const XML: React.FC<ResultRenderProps> = ({ result }) => {
  const [errors, setErrors] = useState<string[]>([]);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [resultsJson, setResultsJson] = useState<Value>({});
  const [isJson, setIsJson] = useState(true);

  useEffect(() => {
    // set alerts and process results to json
    getAlerts(result.result, setResultsJson, setWarnings, setErrors, setIsJson, true);
  }, [result]);

  const parsedResult = formatResultBody(result.result, isJson, resultsJson);

  // Ocean theme from JSON tool renderer
  const thoriumTheme = {
    attributeKeyColor: '#96b5b4',
    attributeValueColor: '#d08770',
    tagColor: '#8fa1b3',
    textColor: '#a3be8c',
    separatorColor: 'tan',
  };

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
