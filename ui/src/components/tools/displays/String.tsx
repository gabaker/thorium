import { useState, useEffect } from 'react';
import { Card, Row } from 'react-bootstrap';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';

// project imports
import { formatResultBody, getAlerts } from '../alerts';
import { ResultRenderProps } from '../props';
import { Value } from '@models/results';

// spec: ../ToolResult.spec.md

type StringResultRenderProps = ResultRenderProps & {
  warnings: string[];
  errors: string[];
};

const String: React.FC<StringResultRenderProps> = ({ result, warnings, errors }) => {
  const [parsedErrors, setParsedErrors] = useState<string[]>([]);
  const [parsedWarnings, setParsedWarnings] = useState<string[]>([]);
  const [resultsJson, setResultsJson] = useState<Value>({});
  const [isJson, setIsJson] = useState(true);

  // Check to see if this is json or string
  // it might be json in cases where results were too large to display
  // in which case an object w/ warning will be returned
  useEffect(() => {
    // set alerts and process results to json
    getAlerts(result.result, setResultsJson, setParsedWarnings, setParsedErrors, setIsJson, true);
  }, [result]);

  // show the alerts passed in by the caller alongside any parsed from this result; derive these
  // during render so they reflect the latest parsed state without mutating the props arrays
  const allErrors = [...errors, ...parsedErrors];
  const allWarnings = [...warnings, ...parsedWarnings];
  const newResult = formatResultBody(result.result, isJson, resultsJson);

  return (
    <Card className="scroll-log tool-result">
      <Row>
        {allErrors.map((err, idx) => (
          <AlertBanner key={idx}>{err}</AlertBanner>
        ))}
        {allWarnings.map((warn, idx) => (
          <AlertBanner key={idx} severity={Severity.Warning}>
            {warn}
          </AlertBanner>
        ))}
      </Row>
      <Row>
        <pre>{newResult}</pre>
      </Row>
    </Card>
  );
};

export default String;
