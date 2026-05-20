import React, { useMemo, useRef, useCallback } from 'react';
import CodeEditor from './CodeEditor';
import { toText } from './serialize';
import type { RuleChecker, FormatType } from '@utilities/rules/types';

export interface ImagePipelineEditorProps {
  value: Record<string, unknown>;
  onChange: (obj: Record<string, unknown> | null) => void;
  checker: RuleChecker;
  format: FormatType;
  height?: string;
  disabled?: boolean;
}

const ImagePipelineEditor: React.FC<ImagePipelineEditorProps> = ({
  value,
  onChange,
  checker,
  format,
  height = '500px',
  disabled = false,
}) => {
  const lastExternalValue = useRef(value);
  const textRef = useRef('');

  const serialized = useMemo(() => {
    lastExternalValue.current = value;
    const t = toText(value, format);
    textRef.current = t;
    return t;
  }, [value, format]);

  const handleChange = useCallback(
    (_text: string, parsed: unknown) => {
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
        onChange(parsed as Record<string, unknown>);
      } else {
        onChange(null);
      }
    },
    [onChange],
  );

  return <CodeEditor value={serialized} onChange={handleChange} checker={checker} format={format} height={height} disabled={disabled} />;
};

export default ImagePipelineEditor;
