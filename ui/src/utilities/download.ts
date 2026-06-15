/**
 * Trigger a browser download of in-memory data via a temporary blob URL.
 *
 * Centralizes the anchor-element download dance that was previously duplicated across
 * result-file, image, and sample download call sites.
 *
 * @param data - The bytes/content to download (e.g. an `ArrayBuffer`, `Blob`, string, or `Uint8Array`).
 * @param fileName - The name to save the file as.
 * @param mime - Optional MIME type applied to the created `Blob`.
 */
export function downloadBlob(data: BlobPart, fileName: string, mime?: string): void {
  const blob = data instanceof Blob ? data : new Blob([data], mime ? { type: mime } : undefined);
  const url = window.URL.createObjectURL(blob);
  try {
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', fileName);
    document.body.appendChild(link);
    link.click();
    link.parentNode?.removeChild(link);
  } finally {
    // release the object URL once the click has been dispatched
    window.URL.revokeObjectURL(url);
  }
}
