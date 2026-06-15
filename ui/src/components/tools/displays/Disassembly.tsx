import { Card, Row } from 'react-bootstrap';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import SyntaxHighlighter from 'react-syntax-highlighter';
import { atomOneDark } from 'react-syntax-highlighter/dist/esm/styles/hljs';

// project imports
import { ResultRenderProps } from '../props';

const MAX_LENGTH = 100000;

const Disassembly: React.FC<ResultRenderProps> = ({ result }) => {
  const rawCodeString = result?.result && typeof result.result === 'string' ? result.result.replace(/\\n/g, '\n').replace(/["]+/g, '') : '';
  const totalCodeSize = rawCodeString.length;
  const codeString = rawCodeString.substring(0, MAX_LENGTH);
  // trigger warning if code was truncated due to large size
  let truncated = false;
  if (rawCodeString.length > MAX_LENGTH) {
    truncated = true;
  }
  return (
    <Card className="scroll-log tool-result">
      {truncated ? (
        <Row>
          <AlertBanner severity={Severity.Warning}>
            {`The rendered disassembly has been truncated
                due to its large size: ${totalCodeSize} bytes`}
          </AlertBanner>
        </Row>
      ) : null}
      {/* eslint-disable-next-line @typescript-eslint/no-unsafe-assignment -- atomOneDark has mismatched types */}
      <SyntaxHighlighter style={atomOneDark}>{codeString}</SyntaxHighlighter>
    </Card>
  );
};

export default Disassembly;
