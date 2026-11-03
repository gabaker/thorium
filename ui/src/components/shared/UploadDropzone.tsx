import React from 'react';
import { useDropzone } from 'react-dropzone';
import { FaXmark } from 'react-icons/fa6';
import styled, { css } from 'styled-components';

// project imports
import Subtitle from '@components/shared/titles/Subtitle';
import { DropzoneFile } from '@components/pages/files/upload/types';

/** The size of an {@link UploadDropzone}. */
export enum DropzoneSize {
  /** The full-width upload page drop area with headed accepted and rejected file lists */
  Normal = 'normal',
  /** A one-line drop area sized like a form input, with a plain file list, for use inside a form */
  Compact = 'compact',
}

/** The largest file the dropzone accepts, in bytes (10 GiB). */
const MAX_FILE_SIZE = 10737418240;

/** Where a drag over the drop area stands, which colours its border. */
enum DragState {
  Idle = 'idle',
  Active = 'active',
  Accept = 'accept',
  Reject = 'reject',
}

/** The border colour of each drag state; the idle colour differs by size. */
const DRAG_BORDER: Record<Exclude<DragState, DragState.Idle>, string> = {
  [DragState.Active]: 'var(--thorium-info-bg)',
  [DragState.Accept]: 'var(--thorium-ok-bg)',
  [DragState.Reject]: 'var(--thorium-danger-bg)',
};

/** The drop area: a dashed box that opens the file browser when clicked. */
const DropArea = styled.div<{ $size: DropzoneSize; $width: string; $drag: DragState }>`
  display: flex;
  flex-direction: column;
  align-items: center;
  width: ${({ $width }) => $width};
  border-style: dashed;
  outline: none;
  cursor: pointer;
  transition: border 0.24s ease-in-out;
  ${({ $size }) =>
    $size === DropzoneSize.Compact
      ? css`
          gap: 0.1rem;
          padding: 0.5rem 0.75rem;
          border-width: 1px;
          border-radius: 6px;
          border-color: var(--thorium-panel-border);
          background-color: var(--thorium-secondary-panel-bg);
          color: var(--thorium-secondary-text);
          font-size: 0.9rem;
          text-align: center;

          &:focus-visible {
            border-color: var(--thorium-highlight-panel-border);
          }

          p {
            margin: 0;
            color: var(--thorium-text);
          }

          em {
            font-size: 0.8rem;
          }
        `
      : css`
          flex: 1;
          padding: 20px;
          border-width: 2px;
          border-radius: 2px;
          border-color: currentColor;
          background-color: transparent;
          color: inherit;
        `}
  ${({ $drag }) => $drag !== DragState.Idle && `border-color: ${DRAG_BORDER[$drag]};`}
`;

/** The compact file list: one line per file with its size and a remove control. */
const CompactList = styled.ul`
  list-style: none;
  margin: 0.5rem 0 0 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  font-size: 0.85rem;

  li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    overflow-wrap: anywhere;
  }

  .size,
  .reason {
    color: var(--thorium-secondary-text);
  }

  .rejected {
    color: var(--thorium-error-bg);
  }
`;

/** The remove control of a listed file. */
const RemoveFile = styled.button`
  display: inline-flex;
  padding: 0.1rem;
  border: none;
  border-radius: 4px;
  background: transparent;
  color: var(--thorium-secondary-text);
  cursor: pointer;

  &:hover {
    color: var(--thorium-text);
  }

  &:focus-visible {
    outline: 2px solid var(--thorium-link-text);
  }
`;

/**
 * Format a byte count for the compact file list.
 *
 * @param bytes - The size in bytes.
 * @returns The size in B, KB, MB, or GB with at most one decimal.
 */
export function formatFileSize(bytes: number): string {
  const units = ['B', 'KB', 'MB', 'GB'];
  let size = bytes;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${size} B` : `${size.toFixed(1).replace(/\.0$/, '')} ${units[unit]}`;
}

/**
 * The name a dropped or chosen file is listed under.
 *
 * @param file - The file; files from react-dropzone carry a relative `path`.
 * @returns The file's path when it has one, otherwise its name.
 */
const fileLabel = (file: File): string => (file as Partial<DropzoneFile>).path || file.name;

/** Props for {@link UploadDropzone}. */
interface DropzoneProperties {
  /** The width of the drop area */
  width?: string;
  /** Called with the files dropped or chosen; they replace the earlier choice */
  onChange: (files: DropzoneFile[]) => void;
  /** Called with an empty list whenever new files are accepted, clearing validation errors */
  onError?: (errors: string[]) => void;
  /** The files chosen so far; the compact list shows these, so the parent can clear or prune them */
  selectedFiles: File[];
  /** The drop area's size, the full-width Normal area unless given */
  size?: DropzoneSize;
  /** The test id of the hidden file input, which tests use to choose files */
  inputTestId?: string;
}

/** A drag-and-drop file area that also opens the file browser on click, listing the accepted and rejected files. */
const UploadDropzone: React.FC<DropzoneProperties> = ({
  width = '100%',
  onChange,
  onError,
  selectedFiles,
  size = DropzoneSize.Normal,
  inputTestId,
}) => {
  const compact = size === DropzoneSize.Compact;
  const { acceptedFiles, fileRejections, getRootProps, getInputProps, isDragActive, isDragAccept, isDragReject } = useDropzone({
    maxSize: MAX_FILE_SIZE,
    onDrop: (accepted) => {
      onChange(accepted as unknown as DropzoneFile[]);
      if (onError) onError([]);
    },
  });
  // the most specific drag state wins, as accept and reject are both active drags
  let drag = DragState.Idle;
  if (isDragReject) drag = DragState.Reject;
  else if (isDragAccept) drag = DragState.Accept;
  else if (isDragActive) drag = DragState.Active;
  const area = (
    <DropArea {...getRootProps()} $size={size} $width={width} $drag={drag} data-testid={compact ? 'dropzone-compact' : undefined}>
      <input {...getInputProps()} data-testid={inputTestId} />
      {compact ? (
        <>
          <p>Drop files here or click to browse</p>
          <em>(files must be &lt; 10GB)</em>
        </>
      ) : (
        <>
          <p>Drag and drop some files or click to select files</p>
          <em>({`files must be < 10GB`})</em>
        </>
      )}
    </DropArea>
  );
  // the compact list follows the parent's files so a removed file leaves it, and shows a refusal beside its name
  if (compact) {
    return (
      <div>
        {area}
        {(selectedFiles.length > 0 || fileRejections.length > 0) && (
          <CompactList data-testid="dropzone-files">
            {selectedFiles.map((file) => (
              <li key={fileLabel(file)} data-testid="dropzone-file">
                <span>{file.name}</span>
                <span className="size">{formatFileSize(file.size)}</span>
                <RemoveFile
                  type="button"
                  aria-label={`Remove ${file.name}`}
                  data-testid="dropzone-remove"
                  onClick={() => onChange(selectedFiles.filter((other) => other !== file) as DropzoneFile[])}
                >
                  <FaXmark size={12} />
                </RemoveFile>
              </li>
            ))}
            {fileRejections.map(({ file, errors }) => (
              <li key={`rejected-${fileLabel(file)}`} className="rejected">
                <span>{file.name}</span>
                <span className="reason">{errors.map((error) => error.message).join('; ')}</span>
              </li>
            ))}
          </CompactList>
        )}
      </div>
    );
  }
  // the files the dropzone holds, or the parent's when it holds none (a page returned to with files already chosen)
  const listed: readonly File[] = acceptedFiles.length > 0 ? acceptedFiles : selectedFiles;
  const fileAcceptedItems = listed.map((file) => (
    <li key={fileLabel(file)}>
      {fileLabel(file)} - {file.size} bytes
    </li>
  ));
  const fileRejectionItems = fileRejections.map(({ file, errors }) => (
    <li key={fileLabel(file)}>
      {fileLabel(file)} - {file.size} bytes
      <ul>
        {errors.map((e) => (
          <li key={e.code}>{e.message}</li>
        ))}
      </ul>
    </li>
  ));
  return (
    <div>
      {area}
      <aside>
        {(fileAcceptedItems.length > 0 || fileRejectionItems.length > 0) && <hr />}
        {fileAcceptedItems.length > 0 && (
          <>
            <h4>
              <Subtitle>Accepted Files</Subtitle>
            </h4>
            <ul>
              <Subtitle>{fileAcceptedItems}</Subtitle>
            </ul>
          </>
        )}
        {fileRejectionItems.length > 0 && (
          <>
            <h4>
              <Subtitle>Rejected Files</Subtitle>
            </h4>
            <ul>
              <Subtitle>{fileRejectionItems}</Subtitle>
            </ul>
          </>
        )}
      </aside>
    </div>
  );
};

export default UploadDropzone;
