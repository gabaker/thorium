import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { ReactNode } from 'react';

/**
 * Create a query client for tests: no retries, so a failed query settles at once, and no garbage collection timer.
 *
 * @returns A fresh client.
 */
export function createTestQueryClient(): QueryClient {
  return new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity }, mutations: { retry: false } } });
}

/**
 * Wrap a test's tree in a query client provider.
 *
 * @param node - The tree to render.
 * @param client - The client to provide; a fresh test client unless given.
 * @returns The wrapped tree.
 */
export function withQueryClient(node: ReactNode, client: QueryClient = createTestQueryClient()) {
  return <QueryClientProvider client={client}>{node}</QueryClientProvider>;
}
