import { createContext, useContext } from 'react';

/** Where the results being shown are stored: on a file (the default) or on a repo. */
export const ResultSourceContext = createContext<{ repo: boolean }>({ repo: false });

/**
 * Whether the results in scope belong to a repo, so result files are read from the repo's routes.
 *
 * @returns `true` inside a repo's results, `false` for a file's results.
 */
export const useResultsOnRepo = (): boolean => useContext(ResultSourceContext).repo;
