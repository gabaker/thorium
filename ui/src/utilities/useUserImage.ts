import { useEffect, useState } from 'react';

// project imports
import { fetchUserImage } from '@thorpi/users';

/**
 * Lazily load a user's profile icon as an object URL.
 *
 * Fetches the icon from `GET /users/user/{username}/image` only when `hasImage` is true, so the
 * common "no icon" case costs no request. The created object URL is revoked automatically when the
 * user/flag changes or the component unmounts. Call the returned `reload` after the icon changes
 * (upload/remove) to force a refetch even when `hasImage` is unchanged.
 *
 * @param username - The user whose icon to load (skipped when undefined).
 * @param hasImage - Whether the user has an icon set (from `UserInfo.has_image`).
 * @returns The current object URL (or `null`) and a `reload` function to force a refetch.
 */
export function useUserImage(username: string | undefined, hasImage: boolean | undefined): { imageUrl: string | null; reload: () => void } {
  const [imageUrl, setImageUrl] = useState<string | null>(null);
  // bumping this forces a refetch (e.g. after the icon is replaced at the same path)
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let active = true;
    let objectUrl: string | null = null;

    if (username && hasImage) {
      void fetchUserImage(username).then((url) => {
        // the effect was cleaned up before the fetch resolved — revoke and bail
        if (!active) {
          if (url) URL.revokeObjectURL(url);
          return;
        }
        objectUrl = url;
        setImageUrl(url);
      });
    } else {
      setImageUrl(null);
    }

    return () => {
      active = false;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [username, hasImage, version]);

  return { imageUrl, reload: () => setVersion((v) => v + 1) };
}
