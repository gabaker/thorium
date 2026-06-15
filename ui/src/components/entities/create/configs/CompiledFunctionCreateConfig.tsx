import { JSX, useState } from 'react';
import { Row } from 'react-bootstrap';

// project imports
import { EntityCreateConfig } from './config';
import { CreateMetadataProps } from '../EntityCreate';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import NumberInput from '@components/shared/inputs/NumberInput';
import CodeEditor from '@components/shared/inputs/code/CodeEditor/CodeEditor';
import { disassemblyToText, textToDisassembly } from '@utilities/disassembly';
import { FormatType } from '@utilities/rules/types';
import { Entities } from '@models/entities/entities';
import { BlankCreateCompiledFunction, CompiledFunctionCreateMetaFields } from '@models/entities/functions';

// spec: ../EntityCreate.spec.md

const CompiledFunctionMetaInfo = ({ entity, onChange }: CreateMetadataProps<Entities.CompiledFunction>): JSX.Element => {
  // The disassembly is edited as address-per-line text but stored as structured instructions; keep the
  // editor buffer locally and only commit it to metadata when it parses cleanly.
  const [disassemblyText, setDisassemblyText] = useState(() => disassemblyToText(entity.metadata.CompiledFunction.disassembly));
  const [parseError, setParseError] = useState<string | undefined>();

  function updatePendingMeta<T extends keyof CompiledFunctionCreateMetaFields>(field: T, value: CompiledFunctionCreateMetaFields[T]): void {
    const updates: CompiledFunctionCreateMetaFields = structuredClone(entity.metadata.CompiledFunction);
    updates[field] = value;
    onChange('metadata', { CompiledFunction: updates });
  }

  function handleDisassemblyChange(text: string): void {
    setDisassemblyText(text);
    const { instructions, error } = textToDisassembly(text);
    setParseError(error);
    if (!error) updatePendingMeta('disassembly', instructions);
  }

  return (
    <>
      <Row>
        <InfoHeader>Address</InfoHeader>
        <InfoValue>
          <NumberInput value={entity.metadata.CompiledFunction.address} onChange={(v) => updatePendingMeta('address', v ?? 0)} min={0} />
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Disassembly</InfoHeader>
        <InfoValue>
          <CodeEditor value={disassemblyText} onChange={handleDisassemblyChange} format={FormatType.Disassembly} height="400px" />
          {parseError && (
            <AlertBanner className="mt-2" severity={Severity.Error}>
              {parseError}
            </AlertBanner>
          )}
        </InfoValue>
      </Row>
    </>
  );
};

const CompiledFunctionCreateConfig: EntityCreateConfig<Entities.CompiledFunction> = {
  kind: Entities.CompiledFunction,
  EntityMetadata: CompiledFunctionMetaInfo,
  BlankCreateEntity: BlankCreateCompiledFunction,
};

export default CompiledFunctionCreateConfig;
