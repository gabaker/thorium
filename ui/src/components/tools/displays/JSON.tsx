import React, { useState, useEffect } from 'react';
import { Card, Col, Row } from 'react-bootstrap';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { JSONTree } from 'react-json-tree';

// project imports
import String from './String';
import { getAlerts } from '../alerts';
import { ResultRenderProps } from '../props';
import { OceanJsonTheme } from '@components/shared/renderers/jsonTheme';
import { Value } from '@models/results';

// generic json dump using react-json-view library
const JSON: React.FC<ResultRenderProps> = ({ result, sha256, tool }) => {
  const [errors, setErrors] = useState<string[]>([]);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [resultsJson, setResultsJson] = useState<Value>({});
  const [isJson, setIsJson] = useState(true);

  useEffect(() => {
    // set alerts and process results to json
    getAlerts(result.result, setResultsJson, setWarnings, setErrors, setIsJson, false);
  }, [result]);

  return (
    <>
      {isJson ? (
        <Card className="scroll-log tool-result">
          <Row>
            {errors.map((err, idx) => (
              <AlertBanner key={idx}>{err}</AlertBanner>
            ))}
            {warnings.map((warn, idx) => (
              <AlertBanner key={idx} severity={Severity.Warning}>
                {warn}
              </AlertBanner>
            ))}
          </Row>
          {isJson && (
            <Row>
              <Col>
                <JSONTree
                  data={resultsJson}
                  shouldExpandNodeInitially={() => true}
                  hideRoot={true}
                  theme={OceanJsonTheme}
                  invertTheme={false}
                />
              </Col>
            </Row>
          )}
        </Card>
      ) : (
        <String result={result} sha256={sha256} tool={tool} warnings={warnings} errors={errors} />
      )}
    </>
  );
};

export { JSON as default, OceanJsonTheme };
