import { QueryClient, useQueryClient } from '@tanstack/react-query';
import React, { createContext, JSX, useContext, useEffect, useState } from 'react';
import { Navigate } from 'react-router';

// project imports
import { authUserPass, checkSession, createUser, logout } from '@thorpi/users';
import { UserInfo, RoleKey, ScopedToken, SessionState } from '@models/users';
import { announceIdentityChange, onIdentityChange } from './identityChannel';
import { clearTagDataFromLocalStorage, fetchLocalStorageTags } from './tags';
import { clearScopedSession, getScopedFromSession, setScopedSession } from '@thorpi/client';

/// The outcome of a password login attempt.
export enum LoginOutcome {
  /// Authenticated; the session token has been stored.
  LoggedIn = 'logged_in',
  /// Credentials were valid but the account's email is not yet verified.
  VerifyEmail = 'verify_email',
  /// Login failed (bad credentials or request error).
  Failed = 'failed',
}

/// The outcome of a registration attempt.
export enum RegisterOutcome {
  /// The account was created and the user is logged in (auto-verified deployments).
  LoggedIn = 'logged_in',
  /// The account was created but the user must verify their email before they can log in.
  VerifyEmail = 'verify_email',
  /// Registration failed.
  Failed = 'failed',
}

type AuthContextType = {
  userInfo: UserInfo | null;
  token: string | undefined;
  scopedToken: ScopedToken | undefined;
  refreshUserInfo: (force?: boolean) => Promise<void>;
  checkCookie: () => Promise<unknown>;
  login: (username: string, password: string, handleError: (error: string) => void) => Promise<LoginOutcome>;
  logout: () => Promise<unknown>;
  register: (
    username: string,
    password: string,
    handleError: (error: string) => void,
    email?: string,
    role?: string,
  ) => Promise<RegisterOutcome>;
  revoke: () => Promise<unknown>;
  impersonate: (userToken: string, tokenExpires: string) => void;
  completeOAuth: (token: string, expires: string) => void;
  setScopedToken: (token: ScopedToken) => void;
  clearScopedToken: () => void;
};

// auth context to store info about auth state across app
const authContext = createContext<AuthContextType | undefined>(undefined);

// get document cookie by name
function getCookie(name: string) {
  const cookieArr = document.cookie.split(';');

  for (let i = 0; i < cookieArr.length; i++) {
    const cookiePair = cookieArr[i].trim().split('=');

    if (cookiePair[0] === name) {
      return decodeURIComponent(cookiePair[1]);
    }
  }
  return undefined;
}

// Exported so the cookie contract can be unit-tested. Builds the non-HttpOnly THORIUM_TOKEN
// cookie the axios interceptor reads on every request.
export function buildCookie(token: string, expiration: string) {
  return `THORIUM_TOKEN=${token}; Secure; SameSite=Strict; expires=${expiration}; path=/; domain=${location.hostname}`;
}

// Clears the THORIUM_TOKEN cookie on logout/revoke. A cookie written with an explicit Domain
// is a distinct cookie from one without, so the expiring write MUST mirror buildCookie's
// domain + path or the browser leaves the real cookie in place and the user stays logged in.
export function buildRevokeCookie() {
  return `THORIUM_TOKEN=; Secure; SameSite=Strict; max-age=0; path=/; domain=${location.hostname}`;
}

/**
 * Follow the identity changes other tabs announce: each one drops this tab's cached reads, which belong to the
 * identity the tab had, and then lets the tab take up the session the shared cookie now holds.
 *
 * @param queryClient - This tab's query cache.
 * @param resync - Called after the cache is dropped, to read the session cookie again.
 * @returns A function that stops following the other tabs.
 */
export function followOtherTabs(queryClient: QueryClient, resync: () => void): () => void {
  return onIdentityChange(() => {
    queryClient.clear();
    resync();
  });
}

/*
 * Thorium auth hooks for login, logout and token revocation
 */
function useAuthProvider() {
  const [userInfo, setUserInfo] = useState<UserInfo | null>(null);
  const [token, setToken] = useState(getCookie('THORIUM_TOKEN')); // set time of last userInfo update
  const [lastUpdateDate, setLastUpdateDate] = useState(Date.now());
  const [activeScopedToken, setActiveScopedToken] = useState<ScopedToken | undefined>(getScopedFromSession());
  const queryClient = useQueryClient();
  // the query cache holds what one identity could read, so it is dropped whenever the identity behind requests changes
  // (login, registration, logout, impersonation, an expired session, or a scoped token starting or ending) and the next
  // session in the tab reads afresh
  const clearCachedReads = () => queryClient.clear();
  // the session cookie is shared by every tab, so a change to it also drops the other tabs' cached reads; a scoped
  // token lives in this tab's session storage, so starting or ending one stays out of this
  const changeSharedIdentity = () => {
    clearCachedReads();
    announceIdentityChange();
  };
  // the server refused the session (401 or 403): forget it in this tab and the others; a network error or 5xx never
  // gets here, so a passing outage keeps the session until a later check reaches the server
  const endExpiredSession = () => {
    clearTagDataFromLocalStorage();
    document.cookie = buildRevokeCookie();
    setUserInfo(null);
    changeSharedIdentity();
  };

  // options for set/get of a secure cookie
  const getUserInfo = async () => {
    // get user details
    if (token != undefined) {
      const session = await checkSession();
      if (session.state === SessionState.Valid) {
        setUserInfo(session.user);
        setToken(session.user.token);
        setLastUpdateDate(Date.now());
        void fetchLocalStorageTags();
      } else if (session.state === SessionState.Expired) {
        endExpiredSession();
        setToken('');
      }
    }
  };

  useEffect(() => {
    // update theme if userInfo changes
    if (userInfo?.settings?.theme) {
      const root = document.getElementById('root');
      // automatic theme will use browser defaults
      if (userInfo.settings['theme'] == 'Automatic') {
        if (window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) {
          root?.setAttribute('theme', 'Dark');
        } else {
          root?.setAttribute('theme', 'Light');
        }
      } else {
        root?.setAttribute('theme', userInfo.settings.theme);
      }
    }
  }, [userInfo]);

  useEffect(() => {
    void getUserInfo();
    // need to run whoami to get user info after login
  }, [token, activeScopedToken]);

  // another tab logged in, out, or as someone else: this tab ends its scoped token, which belonged to the old session,
  // and takes up whatever session the shared cookie now holds (setting the token reruns whoami for a new one)
  useEffect(
    () =>
      followOtherTabs(queryClient, () => {
        clearScopedSession();
        setActiveScopedToken(undefined);
        const next = getCookie('THORIUM_TOKEN');
        if (next === undefined) setUserInfo(null);
        setToken(next);
      }),
    [queryClient],
  );

  return {
    userInfo,
    token,
    scopedToken: activeScopedToken,
    // verify userInfo is not out-of-date
    async refreshUserInfo(force = false) {
      // check if userInfo is fresher than 60 seconds (60k msec)
      if (force || Date.now() - lastUpdateDate > 60000) {
        // refresh user info
        await getUserInfo();
      }
      return;
    },
    // validate cookie with request to Thorium whoami route: a 401 or 403 ends the session, while a network error or
    // 5xx keeps it and returns null
    async checkCookie() {
      if (!getCookie('THORIUM_TOKEN')) return null;
      const session = await checkSession();
      if (session.state === SessionState.Valid) {
        setUserInfo(session.user);
        setToken(session.user.token);
        setLastUpdateDate(Date.now());
        return session.user;
      }
      if (session.state === SessionState.Expired) {
        endExpiredSession();
        setToken(undefined);
      }
      return null;
    },
    // login via password to get Thorium token
    async login(username: string, password: string, handleError: (error: string) => void): Promise<LoginOutcome> {
      const result = await authUserPass(username, password, handleError);
      // the request failed (error already surfaced via handleError)
      if (!result) {
        return LoginOutcome.Failed;
      }
      // credentials were valid but the email must be verified before we can issue a session
      if (result.status === 'verify_email') {
        return LoginOutcome.VerifyEmail;
      }
      // set cookie with name THORIUM_TOKEN and store the user's Thorium token
      document.cookie = buildCookie(result.token, result.expires);
      changeSharedIdentity();
      setToken(result.token);
      return LoginOutcome.LoggedIn;
    },
    // remove token and clear user info on logout
    logout() {
      return new Promise((resolve) => {
        setToken(undefined);
        clearScopedSession();
        setUserInfo(null);
        document.cookie = buildRevokeCookie();
        changeSharedIdentity();
        resolve(true);
      });
    },
    // register with Thorium
    async register(
      username: string,
      password: string,
      handleError: (error: string) => void,
      email = 'thorium@sandia.gov',
      role = 'User',
    ): Promise<RegisterOutcome> {
      const result = await createUser(username, email, password, role, handleError);
      // the request itself failed (error already surfaced via handleError)
      if (!result) {
        return RegisterOutcome.Failed;
      }
      // account created but the user must verify their email before logging in;
      // do not set a cookie/token — they must log in again after verifying
      if (result.status === 'verify_email') {
        return RegisterOutcome.VerifyEmail;
      }
      // auto-verified deployment: log the user in immediately
      document.cookie = buildCookie(result.token, result.expires);
      changeSharedIdentity();
      setToken(result.token);
      return RegisterOutcome.LoggedIn;
    },
    // revoke token and clear cookie user info from session
    async revoke() {
      const response = await logout();
      const result = response?.status == 200 ? true : false;
      setToken(undefined);
      setUserInfo(null);
      document.cookie = buildRevokeCookie();
      changeSharedIdentity();
      return result;
    },
    // logout of any current session and impersonate a user
    impersonate(userToken: string, tokenExpires: string) {
      // set cookie with name THORIUM_TOKEN
      document.cookie = buildCookie(userToken, tokenExpires);
      changeSharedIdentity();
      setToken(userToken);
    },
    // finalize an OAuth login/registration by storing the issued Thorium token.
    // Setting the token triggers the whoami effect, so the rest of the app (interceptor,
    // RequireAuth) behaves exactly as it does after a password login.
    completeOAuth(token: string, expires: string) {
      document.cookie = buildCookie(token, expires);
      changeSharedIdentity();
      setToken(token);
    },
    setScopedToken(token: ScopedToken) {
      setActiveScopedToken(token);
      setScopedSession(token);
      clearCachedReads();
    },
    clearScopedToken() {
      setActiveScopedToken(undefined);
      clearScopedSession();
      clearCachedReads();
    },
  };
}

/**
 * Wrap application in a shared auth provider
 */
export const Auth: React.FC<AuthHookProps> = ({ children }) => {
  const auth = useAuthProvider();
  return <authContext.Provider value={auth}>{children}</authContext.Provider>;
};

export const useAuth = () => {
  const context = useContext(authContext);
  if (context === undefined) {
    throw new Error('useAuth must be used within a AuthProvider');
  }
  return context;
};

type AuthHookProps = {
  children: JSX.Element;
};

/**
 * Validate that user is logged and redirect on validation failure
 */
export const RequireAuth: React.FC<AuthHookProps> = ({ children }) => {
  const { token } = useAuth();
  // token must be set and cookie must still be set
  // cookie gets cleared when it is expired

  return token != undefined && getCookie('THORIUM_TOKEN') != undefined ? (
    children
  ) : (
    <Navigate to="/auth" replace state={{ path: location.pathname + location.search + location.hash }} />
  );
};

/**
 * Validate user's Thorium role is admin and redirects on authorization failure
 */
export const RequireAdmin: React.FC<AuthHookProps> = ({ children }) => {
  const { userInfo } = useAuth();
  const role = userInfo?.role as unknown as RoleKey;
  // userInfo is still loading (null)
  if (userInfo === null) {
    return null;
  }
  // userInfo loaded, but user is not an Admin
  if (role !== RoleKey.Admin) {
    return <Navigate to="/" replace state={{ path: location.pathname + location.search + location.hash }} />;
  }
  // user is an Admin
  return children;
};
