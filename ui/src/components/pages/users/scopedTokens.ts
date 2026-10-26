// project imports
import { RoleKey, ScopedToken, ThoriumDeveloperRoleValue, ThoriumRole } from '@models/users';
import { getThoriumRole } from '@utilities/role';

/**
 * Whether a scoped token is a run token.
 *
 * Run tokens are minted and ended by the API for a workflow run or tool run, so a user can see them
 * but never activates, edits, or deletes them.
 *
 * @param tok - The scoped token to check.
 * @returns `true` if the token is limited to a run.
 */
export const isRunToken = (tok: ScopedToken): boolean => Boolean(tok.run);

/** The scoped tokens of a user split by who manages them. */
export type SplitScopedTokens = {
  /// Tokens the user created and can manage, with the active token first
  userTokens: ScopedToken[];
  /// Run tokens, which are shown for reference only
  runTokens: ScopedToken[];
};

/**
 * Split scoped tokens into user-managed and run tokens, each sorted by name.
 *
 * @param tokens - The scoped tokens returned by the API.
 * @param active - The scoped token currently in use, which sorts first among the user tokens.
 * @returns The user-managed tokens and the run tokens.
 */
export const splitScopedTokens = (tokens: ScopedToken[], active?: ScopedToken): SplitScopedTokens => {
  const byName = (a: ScopedToken, b: ScopedToken) => a.name.localeCompare(b.name);
  const runTokens = tokens.filter(isRunToken).sort(byName);
  // the active token leads so the user sees what they are acting as without scanning the list
  const userTokens = tokens
    .filter((tok) => !isRunToken(tok))
    .sort((a, b) => {
      if (active) {
        if (active.name === a.name) return -1;
        if (active.name === b.name) return 1;
      }
      return byName(a, b);
    });
  return { userTokens, runTokens };
};

/**
 * Work out which developer permissions a user may give a scoped token.
 *
 * The API refuses a developer scoped token to a user with the User role, and refuses any permission the
 * user does not hold themselves; admins and analysts may give every permission.
 *
 * @param role - The serialized role of the user creating or editing the token.
 * @returns The permissions the user may grant, or `null` when the user may not create a developer token.
 */
export const grantableDeveloperPermissions = (role: ThoriumRole | undefined): ThoriumDeveloperRoleValue | null => {
  if (!role) return null;
  const key = getThoriumRole(role);
  if (key === RoleKey.Admin || key === RoleKey.Analyst) {
    return { k8s: true, bare_metal: true, windows: true, external: true, kvm: true };
  }
  return key === RoleKey.Developer && role.Developer ? role.Developer : null;
};

/**
 * Format a token expiration for a table cell.
 *
 * @param expires - An ISO datetime, or `null` when the token never expires.
 * @returns The date part of the datetime, or `-` when there is none.
 */
export const formatExpire = (expires: string | null): string => {
  if (!expires) return '-';
  return expires.length > 10 ? expires.slice(0, 10) : expires;
};

/** What a profile refresh needs to reload the profile and set its busy state. */
export type ProfileRefreshDeps = {
  /// Reloads the user's whoami info; called with `true` to bypass the freshness cache
  refreshUserInfo: (force?: boolean) => Promise<void>;
  /// Reloads the user's scoped token list
  reloadTokens: () => Promise<void>;
  /// Records whether a refresh is in flight
  setRefreshing: (refreshing: boolean) => void;
};

/**
 * Reload the whoami info and the scoped token list together.
 *
 * Both requests finish before the busy state clears, and one failing never leaves the page stuck
 * refreshing or skips the other.
 *
 * @param deps - The reload functions and the busy-state setter.
 */
export const runProfileRefresh = async ({ refreshUserInfo, reloadTokens, setRefreshing }: ProfileRefreshDeps): Promise<void> => {
  setRefreshing(true);
  try {
    // force whoami past its 60 second cache so the refresh always shows current data
    await Promise.allSettled([refreshUserInfo(true), reloadTokens()]);
  } finally {
    setRefreshing(false);
  }
};
