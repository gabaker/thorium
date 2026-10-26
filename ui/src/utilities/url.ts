/**
 * Get the absolute API URL, resolved the same way as the API client's base URL.
 *
 * On `localhost`/`127.0.0.1` it honors the `THORIUM_API_URL` dev-server override; otherwise it uses
 * the page origin (scheme, host and port).
 *
 * @returns The API URL ending in `/api`, without a trailing slash.
 */
export function getApiUrl() {
  const host = window.location.hostname;
  if ((host === 'localhost' || host === '127.0.0.1') && import.meta.env.THORIUM_API_URL) {
    return `${String(import.meta.env.THORIUM_API_URL).replace(/\/+$/, '')}/api`;
  }
  return `${window.location.origin}/api`;
}

/**
 * Update the URL hash to reflect the active section (and optional subsection).
 *
 * Sets `window.location.hash` to `#<section>` or `#<section>-<subsection>` when a subsection is
 * given, so deep links and the back button track the in-page navigation state.
 *
 * @param section - The primary section identifier.
 * @param subsection - The subsection identifier; pass an empty string for none.
 */
export function updateURLSection(section: string, subsection: string | undefined) {
  const updatedHash = subsection ? `#${section}-${subsection}` : `#${section}`;
  window.location.hash = updatedHash;
}
