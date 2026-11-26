// Web Push, owned by the browser tab.
//
// The durable device intent lives in IndexedDB behind `/push-sync.js`
// (`globalThis.MjPush`), which the service worker also imports. That script is
// loaded on demand from this same origin, so the strict `script-src 'self'`
// CSP still holds and a browser without Push support never pays for it.

import { useCallback, useEffect, useRef, useState } from 'react';
import { dispatchAction, getPushStatus } from '../../api/client';

interface MjPushState {
  enabled: boolean;
  endpoints: string[];
  dismissed?: boolean;
}

interface MjPushApi {
  state: () => Promise<MjPushState>;
  dismiss: () => Promise<void>;
  reconcile: (registration: ServiceWorkerRegistration, oldEndpoint?: string) => Promise<boolean>;
  setEnabled: (enabled: boolean, registration: ServiceWorkerRegistration) => Promise<boolean>;
}

type PermissionState = NotificationPermission | 'unsupported';

export interface WebPushController {
  supported: boolean;
  loading: boolean;
  busy: boolean;
  enabled: boolean;
  subscribed: boolean;
  dismissed: boolean;
  permission: PermissionState;
  statusText: string;
  error: string | null;
  showOffer: boolean;
  enable: () => Promise<void>;
  disable: () => Promise<void>;
  test: () => Promise<void>;
  dismissOffer: () => Promise<void>;
  refresh: () => Promise<void>;
}

function currentMjPush(): MjPushApi | undefined {
  return (globalThis as { MjPush?: MjPushApi }).MjPush;
}

let scriptRequest: Promise<MjPushApi> | null = null;

function loadMjPush(): Promise<MjPushApi> {
  const existing = currentMjPush();
  if (existing) return Promise.resolve(existing);
  if (scriptRequest) return scriptRequest;
  const request = new Promise<MjPushApi>((resolve, reject) => {
    if (typeof document === 'undefined') {
      reject(new Error('Notifications are unavailable in this browser.'));
      return;
    }
    const script = document.createElement('script');
    script.src = '/push-sync.js';
    script.async = true;
    script.onload = () => {
      const api = currentMjPush();
      if (api) resolve(api);
      else reject(new Error('Notification support did not initialize. Reload and try again.'));
    };
    script.onerror = () => reject(new Error('Could not load notification support. Reload and try again.'));
    document.head.append(script);
  }).catch(error => {
    scriptRequest = null;
    throw error;
  });
  scriptRequest = request;
  return request;
}

/** Whether this browser can subscribe at all. */
export function pushSupported(): boolean {
  if (typeof window === 'undefined') return false;
  return Boolean(
    window.isSecureContext &&
      'Notification' in window &&
      'serviceWorker' in navigator &&
      'PushManager' in window,
  );
}

function permissionState(): PermissionState {
  return typeof Notification === 'undefined' ? 'unsupported' : Notification.permission;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function activeRegistration(): Promise<ServiceWorkerRegistration> {
  const registration = await navigator.serviceWorker.getRegistration();
  if (!registration?.active) {
    throw new Error('The app is still preparing notifications. Reload and try again.');
  }
  return registration;
}

let pendingReconcile: Promise<void> | null = null;

/**
 * Finish durable push intent that an earlier session left behind (an enable or
 * disable whose server call failed, a renewed endpoint). Sends no request when
 * the browser cannot do Push, has no service worker, or has nothing pending:
 * the pending check reads IndexedDB only. Never throws.
 */
export function reconcilePendingPush(): Promise<void> {
  if (pendingReconcile) return pendingReconcile;
  const run = (async () => {
    if (!pushSupported()) return;
    const registration = await navigator.serviceWorker.getRegistration();
    if (!registration) return;
    const api = await loadMjPush();
    const state = await api.state();
    if (state.enabled !== true && !(state.endpoints?.length > 0)) return;
    await api.reconcile(registration);
  })()
    .catch(error => console.warn('Could not reconcile notification state', messageOf(error)))
    .finally(() => {
      pendingReconcile = null;
    });
  pendingReconcile = run;
  return run;
}

export function useWebPush(): WebPushController {
  const supported = pushSupported();
  const [loading, setLoading] = useState(supported);
  const [busy, setBusy] = useState(false);
  const [enabled, setEnabled] = useState(false);
  const [subscribed, setSubscribed] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const [permission, setPermission] = useState<PermissionState>(() => permissionState());
  const [statusText, setStatusText] = useState(
    supported ? 'Checking notifications…' : 'Notifications need a supported browser and HTTPS. On iOS, add Mjolnir to the Home Screen.',
  );
  const [error, setError] = useState<string | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const refresh = useCallback(async () => {
    if (!pushSupported()) {
      setLoading(false);
      setPermission(permissionState());
      setStatusText(
        'Notifications need a supported browser and HTTPS. On iOS, add Mjolnir to the Home Screen.',
      );
      return;
    }
    setPermission(permissionState());
    try {
      const api = await loadMjPush();
      const local = await api.state();
      const registration = await navigator.serviceWorker.getRegistration();
      const subscription = registration?.active
        ? await registration.pushManager.getSubscription()
        : null;
      const status = subscription
        ? await getPushStatus(subscription.endpoint)
        : null;
      if (!mounted.current) return;
      setEnabled(local.enabled === true);
      setDismissed(local.dismissed === true);
      setSubscribed(status?.subscribed === true);
      setPermission(permissionState());
      setError(status?.error ?? null);
      if (status?.error) setStatusText(`Notification delivery: ${status.error}`);
      else if (status?.subscribed) setStatusText('Enabled on this device');
      else if (permissionState() === 'denied') {
        setStatusText('Notifications are blocked in this browser’s site settings.');
      } else {
        setStatusText(`Not enabled (permission: ${permissionState()})`);
      }
    } catch (failure) {
      if (mounted.current) {
        setError(messageOf(failure));
        setStatusText('Notifications are unavailable on this device.');
      }
    } finally {
      if (mounted.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const enable = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      // Ask inside the tap gesture: iOS rejects a permission request that
      // happens after an await.
      const permissionResult = await Notification.requestPermission();
      setPermission(permissionResult);
      if (permissionResult !== 'granted') {
        throw new Error(
          'Notification permission was not granted. Allow it in this browser’s site settings to enable notifications.',
        );
      }
      const api = await loadMjPush();
      const registration = await activeRegistration();
      await api.setEnabled(true, registration);
      await refresh();
    } catch (failure) {
      if (mounted.current) setError(messageOf(failure));
    } finally {
      if (mounted.current) setBusy(false);
    }
  }, [busy, refresh]);

  const disable = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const api = await loadMjPush();
      const registration = await activeRegistration();
      await api.setEnabled(false, registration);
      await refresh();
    } catch (failure) {
      if (mounted.current) setError(messageOf(failure));
    } finally {
      if (mounted.current) setBusy(false);
    }
  }, [busy, refresh]);

  const test = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const registration = await activeRegistration();
      const subscription = await registration.pushManager.getSubscription();
      if (!subscription) throw new Error('Enable notifications first.');
      await dispatchAction({ action: 'push-test', endpoint: subscription.endpoint });
      if (mounted.current) setStatusText('Test notification requested.');
    } catch (failure) {
      if (mounted.current) setError(messageOf(failure));
    } finally {
      if (mounted.current) setBusy(false);
    }
  }, [busy]);

  const dismissOffer = useCallback(async () => {
    try {
      const api = await loadMjPush();
      await api.dismiss();
      if (mounted.current) setDismissed(true);
    } catch (failure) {
      if (mounted.current) setError(messageOf(failure));
    }
  }, []);

  const showOffer =
    supported && !loading && !enabled && !dismissed && permission === 'default';

  return {
    supported,
    loading,
    busy,
    enabled,
    subscribed,
    dismissed,
    permission,
    statusText,
    error,
    showOffer,
    enable,
    disable,
    test,
    dismissOffer,
    refresh,
  };
}
