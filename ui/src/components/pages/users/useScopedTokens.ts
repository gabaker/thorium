import { useCallback, useEffect, useState } from 'react';

// project imports
import { useAuth } from '@utilities/auth';
import { listScopedTokens } from '@thorpi/users';
import { ScopedToken } from '@models/users';

/** The scoped token list of the current user and how it was last loaded. */
export type ScopedTokenList = {
  /// The scoped tokens from the last successful load
  tokens: ScopedToken[];
  /// Whether the list has loaded successfully at least once
  loaded: boolean;
  /// Whether a load is in flight
  loading: boolean;
  /// The error from the last failed load, empty when the last load succeeded
  error: string;
  /// Reloads the list from the API
  reload: () => Promise<void>;
};

/**
 * Load the current user's scoped tokens and reload them whenever the active scoped token changes.
 *
 * @returns The token list, its load state, and a function that reloads it.
 */
export const useScopedTokens = (): ScopedTokenList => {
  const { scopedToken } = useAuth();
  const [tokens, setTokens] = useState<ScopedToken[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const reload = useCallback(async () => {
    setLoading(true);
    const result = await listScopedTokens((msg) => setError(msg));
    // keep the previous list on a failed or malformed load so a transient error does not blank the page
    if (Array.isArray(result)) {
      setError('');
      setTokens(result);
      setLoaded(true);
    }
    setLoading(false);
  }, []);

  useEffect(() => {
    void reload();
    // the list also depends on which scoped token is active, so reload when it changes
  }, [reload, scopedToken]);

  return { tokens, loaded, loading, error, reload };
};
