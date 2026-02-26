// import the base client function that loads from the config
// and injects the token via axios intercepts
import client, { parseRequestError } from './client';
import { SystemSettings, SystemSettingsUpdate } from '@models';

/**
 * Get Thorium system stats
 * @async
 * @function
 * @param {(error: string) => void} errorHandler - error handler function
 * @returns {Promise<any | null>} - Request response
 */
export async function getSystemStats(errorHandler: (error: string) => void): Promise<any | null> {
  const url = '/system/stats';
  return client
    .get(url)
    .then((res) => {
      if (res?.status == 200 && res.data) {
        return res.data;
      }
      return null;
    })
    .catch((error) => {
      parseRequestError(error, errorHandler, 'Get System Stats');
      return null;
    });
}

/**
 * Get Thorium system settings
 * @async
 * @function
 * @param {(error: string) => void} errorHandler - error handler function
 * @returns {Promise<any | null>} - Request response
 */
export async function getSystemSettings(errorHandler: (error: string) => void): Promise<SystemSettings | null> {
  const url = '/system/settings';
  return client
    .get(url)
    .then((res) => {
      if (res?.status == 200) {
        return res.data;
      }
      return null;
    })
    .catch((error) => {
      parseRequestError(error, errorHandler, 'Get System Settings');
      return null;
    });
}

/**
 * Update Thorium system settings
 * @async
 * @function
 * @param {SystemSettingsUpdate} update - the settings update body
 * @param {(error: string) => void} errorHandler - error handler function
 * @returns {Promise<SystemSettings | null>} - Updated settings or null
 */
export async function updateSystemSettings(
  update: SystemSettingsUpdate,
  errorHandler: (error: string) => void,
): Promise<SystemSettings | null> {
  const url = '/system/settings?scan=false';
  return client
    .patch(url, update)
    .then((res) => {
      if (res?.status == 200) {
        return res.data;
      }
      return null;
    })
    .catch((error) => {
      parseRequestError(error, errorHandler, 'Update System Settings');
      return null;
    });
}
