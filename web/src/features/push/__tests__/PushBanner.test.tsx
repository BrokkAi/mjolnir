import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { jsonResponse, mockFetch } from '../../../test-utils';
import { PushBanner } from '../PushBanner';

function browser(permission: NotificationPermission, enabled: boolean, subscribed = false, dismissed = false, error: string | null = null) {
  vi.stubGlobal('isSecureContext', true);
  vi.stubGlobal('Notification', { permission, requestPermission: vi.fn(async () => permission) });
  vi.stubGlobal('PushManager', class {});
  const api = {
    state: vi.fn(async () => ({ enabled, endpoints: [], dismissed })),
    dismiss: vi.fn(async () => {}),
    setEnabled: vi.fn(async () => true),
  };
  vi.stubGlobal('MjPush', api);
  Object.defineProperty(navigator, 'serviceWorker', {
    configurable: true,
    value: { getRegistration: async () => ({ active: {}, pushManager: { getSubscription: async () => subscribed ? { endpoint: 'https://push.test/endpoint' } : null } }) },
  });
  mockFetch({ '/api/v1/push/status': () => jsonResponse({ subscribed, error }) });
  return api;
}

beforeEach(() => localStorage.clear());
afterEach(() => {
  cleanup();
  localStorage.clear();
  vi.unstubAllGlobals();
  Reflect.deleteProperty(navigator, 'serviceWorker');
});

describe('push banner', () => {
  it('hides the banner when enabled', async () => {
    const api = browser('granted', true, true);
    const view = render(<PushBanner />);
    await waitFor(() => expect(api.state).toHaveBeenCalled());
    await waitFor(() => expect(view.container).toBeEmptyDOMElement());
  });

  it.each(['default', 'denied', 'granted', 'unsupported'] as const)(
    'permanently dismisses the disabled prompt for %s', async permission => {
      const api = permission === 'unsupported' ? null : browser(permission, false);
      if (!api) vi.stubGlobal('isSecureContext', false);
      const view = render(<PushBanner />);
      const dismiss = await screen.findByRole('button', { name: 'Dismiss notification prompt' });
      fireEvent.click(dismiss);
      expect(screen.queryByLabelText('Push notifications')).not.toBeInTheDocument();
      if (api) await waitFor(() => expect(api.dismiss).toHaveBeenCalled());
      view.unmount();
      render(<PushBanner />);
      await waitFor(() => expect(screen.queryByLabelText('Push notifications')).not.toBeInTheDocument());
      expect(localStorage.getItem('mj.web.push_banner_dismissed')).toBe('true');
    },
  );

  it.each(['granted', 'denied'] as const)(
    'shows a dismissible warning when enabled intent has no subscription (%s)', async permission => {
      const api = browser(permission, true, false);
      const view = render(<PushBanner />);
      expect(await screen.findByText(permission === 'denied'
        ? 'Notifications are blocked in this browser’s site settings.'
        : 'Not enabled (permission: granted)')).toBeInTheDocument();
      fireEvent.click(screen.getByRole('button', { name: 'Dismiss notification prompt' }));
      await waitFor(() => expect(api.dismiss).toHaveBeenCalled());
      expect(view.container).toBeEmptyDOMElement();
      view.unmount();
      render(<PushBanner />);
      await waitFor(() => expect(screen.queryByLabelText('Push notifications')).not.toBeInTheDocument());
    },
  );

  it.each(['banner', 'settings'] as const)('retries an enabled device without a subscription in %s', async variant => {
    const api = browser('granted', true, false);
    render(<PushBanner variant={variant} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(api.setEnabled).toHaveBeenCalledWith(true, expect.anything()));
  });

  it('dismisses delivery errors permanently while retaining them in settings', async () => {
    const api = browser('granted', true, true, false, 'Delivery failed');
    const view = render(<PushBanner />);
    expect(await screen.findByRole('alert')).toHaveTextContent('Delivery failed');
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss notification prompt' }));
    await waitFor(() => expect(api.dismiss).toHaveBeenCalled());
    expect(view.container).toBeEmptyDOMElement();
    view.unmount();
    const remount = render(<PushBanner />);
    render(<PushBanner variant="settings" />);
    expect(await screen.findByRole('alert')).toHaveTextContent('Delivery failed');
    expect(remount.container).toBeEmptyDOMElement();
    expect(screen.getByRole('button', { name: 'Disable' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Dismiss notification prompt' })).not.toBeInTheDocument();
  });

  it('honors an existing MjPush dismissal', async () => {
    const api = browser('default', false, false, true);
    const view = render(<PushBanner />);
    await waitFor(() => expect(api.state).toHaveBeenCalled());
    await waitFor(() => expect(view.container).toBeEmptyDOMElement());
  });
});
