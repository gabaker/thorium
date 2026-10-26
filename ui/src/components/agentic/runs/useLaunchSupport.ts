import { useCallback, useEffect, useState } from 'react';

// project imports
import { SampleUploader } from './inputs';
import { listRepos } from '@thorpi/repos';
import { uploadFile } from '@thorpi/files';

/**
 * Get an uploader that sends a sample form to Thorium and reports failures through a callback.
 *
 * @param onError - Called with the upload error message.
 * @returns A function that uploads a form and resolves with the SHA256, or `null` on failure.
 */
export function useSampleUploader(onError: (message: string) => void): SampleUploader {
  return useCallback(
    async (form: FormData) => {
      const res = await uploadFile(form, onError, () => undefined, new AbortController());
      return res ? res.sha256 : null;
    },
    [onError],
  );
}

/**
 * Load the URLs of the repos the user can see, to suggest in the repo field.
 *
 * @returns The repo URLs; empty while loading or if the listing fails.
 */
export function useRepoUrls(): string[] {
  const [urls, setUrls] = useState<string[]>([]);
  useEffect(() => {
    let cancelled = false;
    void listRepos({ limit: 100 }, () => undefined, true).then((page) => {
      if (!cancelled) setUrls(page.entityList.map((repo) => repo.url));
    });
    return () => {
      cancelled = true;
    };
  }, []);
  return urls;
}
