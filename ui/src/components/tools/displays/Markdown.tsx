import { useState, useEffect } from 'react';
import { Card } from 'react-bootstrap';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { default as MarkdownHtml } from 'react-markdown';
import remarkGfm from 'remark-gfm';

// project imports
import { formatResultBody, getAlerts } from '../alerts';
import { ResultRenderProps } from '../props';
import '@styles/main.scss';
import { Value } from '@models/results';

const Markdown: React.FC<ResultRenderProps> = ({ result }) => {
  const [errors, setErrors] = useState<string[]>([]);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [resultsJson, setResultsJson] = useState<Value>({});
  const [isJson, setIsJson] = useState(true);

  useEffect(() => {
    // set alerts and process results to json
    getAlerts(result.result, setResultsJson, setWarnings, setErrors, setIsJson, true);
  }, [result]);

  const parsedResult = formatResultBody(result.result, isJson, resultsJson);

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
          <center>
            <MarkdownHtml remarkPlugins={[remarkGfm]}>{parsedResult}</MarkdownHtml>
          </center>
        </Card.Body>
      </Card>
    </>
  );
};

export default Markdown;
