import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { reconcilePendingPush } from '../useWebPush';

const registration = { active: {}, pushManager: {} } as unknown as ServiceWorkerRegistration;

function installBrowser(registrationResult: ServiceWorkerRegistration | undefined) {
  vi.stubGlobal('isSecureContext', true);
  vi.stubGlobal('Notification', { permission: 'granted' });
  vi.stubGlobal('PushManager', class {});
  Object.defineProperty(navigator, 'serviceWorker', {
    configurable: true,
    value: { getRegistration: vi.fn(async () => registrationResult) },
  });
}

function installMjPush(state: { enabled: boolean; endpoints: string[] }) {
  const api = {
    state: vi.fn(async () => state),
    reconcile: vi.fn(async () => true),
    dismiss: vi.fn(),
    setEnabled: vi.fn(),
  };
  vi.stubGlobal('MjPush', api);
  return api;
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
  Reflect.deleteProperty(navigator, 'serviceWorker');
});

describe('reconcilePendingPush', () => {
  it('sends nothing when no durable push intent or endpoint exists', async () => {
    installBrowser(registration);
    const api = installMjPush({ enabled: false, endpoints: [] });
    await reconcilePendingPush();
    expect(api.state).toHaveBeenCalledTimes(1);
    expect(api.reconcile).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('does not even read the device state without a service worker registration', async () => {
    installBrowser(undefined);
    const api = installMjPush({ enabled: true, endpoints: ['https://push/e1'] });
    await reconcilePendingPush();
    expect(api.state).not.toHaveBeenCalled();
    expect(api.reconcile).not.toHaveBeenCalled();
  });

  it.each([
    ['enabled intent', { enabled: true, endpoints: [] }],
    ['an unacknowledged endpoint', { enabled: false, endpoints: ['https://push/e1'] }],
  ])('reconciles with the registration for %s', async (_name, state) => {
    installBrowser(registration);
    const api = installMjPush(state);
    await reconcilePendingPush();
    expect(api.reconcile).toHaveBeenCalledWith(registration);
  });

  it('reports a failed reconcile as a warning instead of throwing', async () => {
    installBrowser(registration);
    const api = installMjPush({ enabled: true, endpoints: [] });
    api.reconcile.mockRejectedValueOnce(new Error('offline'));
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    await expect(reconcilePendingPush()).resolves.toBeUndefined();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});
