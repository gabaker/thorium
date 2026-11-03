/** The broadcast channel the tabs of one browser use to tell each other the signed-in identity changed. */
export const IDENTITY_CHANNEL = 'thorium-identity';

/** The message a tab posts when its identity changes; receivers re-read the session cookie rather than trust the message. */
export const IDENTITY_CHANGED = 'identity-changed';

/** This tab's channel; `null` where the browser has no BroadcastChannel, `undefined` until first used. */
let channel: BroadcastChannel | null | undefined;

/**
 * Get this tab's identity channel, opening it on first use.
 *
 * One channel serves both announcing and listening, because a channel never receives its own messages, so a tab is
 * not told about the changes it made itself.
 *
 * @returns The channel, or `null` where the browser has no BroadcastChannel.
 */
function identityChannel(): BroadcastChannel | null {
  if (channel === undefined) {
    channel = typeof BroadcastChannel === 'undefined' ? null : new BroadcastChannel(IDENTITY_CHANNEL);
  }
  return channel;
}

/**
 * Tell this browser's other tabs that the session cookie now names another identity, or none.
 *
 * The session cookie is shared by every tab, so a login, logout, or expired session in one tab changes the identity
 * behind the requests of all of them.
 */
export function announceIdentityChange(): void {
  identityChannel()?.postMessage(IDENTITY_CHANGED);
}

/**
 * Call a listener whenever another tab announces an identity change.
 *
 * Only the identity-change announcement counts; any other message on the channel is ignored.
 *
 * @param listener - Called once per announcement from another tab.
 * @returns A function that stops calling the listener.
 */
export function onIdentityChange(listener: () => void): () => void {
  const tabs = identityChannel();
  if (!tabs) return () => undefined;
  const handle = (event: MessageEvent) => {
    if (event.data === IDENTITY_CHANGED) listener();
  };
  tabs.addEventListener('message', handle);
  return () => tabs.removeEventListener('message', handle);
}
