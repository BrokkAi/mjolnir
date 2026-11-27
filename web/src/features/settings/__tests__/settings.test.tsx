import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { JsonValue, SettingsDocument, SettingsPage } from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { jsonResponse, mockFetch } from '../../../test-utils';
import SettingsView from '../SettingsView';

const initialDraft: JsonValue = {
  review: { enabled: true },
  theme: 'dark',
  model: 'claude',
};

function rootPage(): SettingsPage {
  return {
    path: [],
    breadcrumb: ['Settings'],
    help: 'Change how Mjolnir behaves.',
    groups: [
      {
        heading: 'General',
        rows: [
          {
            path: ['machines'],
            label: 'Machines',
            help: 'Manage the machines Mjolnir may launch on.',
            control: 'page',
            summary: '2 machines',
          },
          {
            path: ['review', 'enabled'],
            label: 'Reviews',
            control: 'checkbox',
            checked: true,
          },
          {
            path: ['theme'],
            label: 'Theme',
            control: 'choice',
            value: 'dark',
            choices: [
              { value: 'dark', label: 'Dark' },
              { value: 'light', label: 'Light' },
            ],
          },
          {
            path: ['model'],
            label: 'Default model',
            control: 'text',
            value: 'claude',
            placeholder: 'model name',
            number: false,
          },
          {
            path: ['version'],
            label: 'Version',
            control: 'read_only',
            text: '1.2.3',
          },
          {
            path: ['secret'],
            label: 'Hidden secret',
            control: 'hidden',
          },
        ],
      },
    ],
    removable: false,
  };
}

function childPage(): SettingsPage {
  return {
    path: ['machines'],
    breadcrumb: ['Settings', 'Machines'],
    help: 'Machines available to this daemon.',
    groups: [
      {
        rows: [
          {
            path: ['machines', 'local', 'kind'],
            label: 'Machine kind',
            control: 'read_only',
            text: 'Local',
          },
        ],
      },
    ],
    removable: false,
  };
}

function settingsDocument(page: SettingsPage = rootPage(), draft: JsonValue = initialDraft): SettingsDocument {
  return { original: initialDraft, draft, page };
}

function defaultGitIdentity() {
  return {
    name: 'Ada Lovelace',
    email: 'ada@example.com',
    per_directory: true,
  };
}

beforeEach(() => {
  useUiStore.setState({ route: { name: 'settings', path: [] }, implicitRoute: false, layers: [] });
  location.hash = '#settings';
});

afterEach(() => {
  cleanup();
  useSessionStore.setState({ meta: null });
  useUiStore.setState({ route: { name: 'workspace' }, implicitRoute: false, layers: [], toasts: [] });
  vi.unstubAllGlobals();
});

describe('SettingsView', () => {
  it('keeps push controls available after the inbox prompt was dismissed', async () => {
    localStorage.setItem('mj.web.push_banner_dismissed', 'true');
    vi.stubGlobal('isSecureContext', true);
    vi.stubGlobal('Notification', { permission: 'granted' });
    vi.stubGlobal('PushManager', class {});
    vi.stubGlobal('MjPush', { state: async () => ({ enabled: true, endpoints: [], dismissed: true }) });
    Object.defineProperty(navigator, 'serviceWorker', {
      configurable: true,
      value: { getRegistration: async () => ({ active: {}, pushManager: { getSubscription: async () => ({ endpoint: 'https://push.test/endpoint' }) } }) },
    });
    mockFetch({
      '/api/v1/settings': () => jsonResponse(settingsDocument()),
      '/api/v1/git-identity': () => jsonResponse(defaultGitIdentity()),
      '/api/v1/push/status': () => jsonResponse({ subscribed: true, error: null }),
    });
    render(<SettingsView path={[]} />);
    const card = await screen.findByRole('region', { name: 'Push notifications' });
    expect(await within(card).findByText('Enabled on this device')).toBeInTheDocument();
    expect(within(card).getByRole('button', { name: 'Disable' })).toBeEnabled();
    expect(within(card).getByRole('button', { name: 'Send test' })).toBeEnabled();
    expect(within(card).queryByRole('button', { name: 'Dismiss notification prompt' })).not.toBeInTheDocument();
    Reflect.deleteProperty(navigator, 'serviceWorker');
    localStorage.clear();
  });

  it('offers Enable in settings when the dismissed device is disabled', async () => {
    localStorage.setItem('mj.web.push_banner_dismissed', 'true');
    vi.stubGlobal('isSecureContext', true);
    vi.stubGlobal('Notification', { permission: 'default' });
    vi.stubGlobal('PushManager', class {});
    vi.stubGlobal('MjPush', { state: async () => ({ enabled: false, endpoints: [], dismissed: true }) });
    Object.defineProperty(navigator, 'serviceWorker', {
      configurable: true,
      value: { getRegistration: async () => undefined },
    });
    mockFetch({
      '/api/v1/settings': () => jsonResponse(settingsDocument()),
      '/api/v1/git-identity': () => jsonResponse(defaultGitIdentity()),
    });
    render(<SettingsView path={[]} />);
    const card = await screen.findByRole('region', { name: 'Push notifications' });
    expect(await within(card).findByText('Not enabled (permission: default)')).toBeInTheDocument();
    expect(within(card).getByRole('button', { name: 'Enable' })).toBeEnabled();
    Reflect.deleteProperty(navigator, 'serviceWorker');
    localStorage.clear();
  });

  it('applies server-driven edits, shows hidden rows, and saves the draft', async () => {
    let draft = structuredClone(initialDraft) as {
      review: { enabled: boolean };
      theme: JsonValue;
      model: JsonValue;
    };
    const edits: { page: string[]; edit: { kind: string; path?: string[]; choice?: JsonValue; text?: string } }[] = [];
    const saves: unknown[] = [];
    const fetchMock = mockFetch({
      '/api/v1/settings': () => jsonResponse(settingsDocument(rootPage(), draft)),
      '/api/v1/settings/edit': (_url, init) => {
        const body = JSON.parse(String(init?.body)) as {
          page: string[];
          edit: { kind: string; path?: string[]; choice?: JsonValue; text?: string };
        };
        edits.push(body);
        if (body.edit.kind === 'toggle') draft.review.enabled = !draft.review.enabled;
        if (body.edit.kind === 'set' && body.edit.path?.join('.') === 'theme') {
          draft.theme = body.edit.choice ?? null;
        }
        if (body.edit.kind === 'set' && body.edit.path?.join('.') === 'model') {
          draft.model = body.edit.text ?? '';
        }
        return jsonResponse({ draft, page: rootPage() });
      },
      '/api/v1/settings/save': (_url, init) => {
        const body = JSON.parse(String(init?.body)) as { draft: JsonValue; page: string[] };
        saves.push(body);
        return jsonResponse({ original: body.draft, draft: body.draft, page: rootPage() });
      },
      '/api/v1/git-identity': () => jsonResponse(defaultGitIdentity()),
    });

    render(<SettingsView path={[]} />);

    expect(await screen.findByRole('button', { name: /Machines/ })).toBeInTheDocument();
    expect(screen.getByText('1.2.3')).toBeInTheDocument();
    expect(screen.getByLabelText('Hidden secret')).toHaveAttribute('type', 'password');

    // Checkbox: exactly a toggle edit.
    fireEvent.click(screen.getByRole('switch', { name: 'Reviews' }));
    await waitFor(() => expect(edits).toHaveLength(1));
    await waitFor(() => expect(screen.getByRole('switch', { name: 'Reviews' })).not.toBeDisabled());
    expect(edits[0]).toMatchObject({
      page: [],
      edit: { kind: 'toggle', path: ['review', 'enabled'] },
    });

    // Choice: a `set` edit carrying the choice's JSON value.
    fireEvent.click(
      within(screen.getByRole('radiogroup', { name: 'Theme' })).getByRole('radio', {
        name: 'Light',
      }),
    );
    await waitFor(() => expect(edits).toHaveLength(2));
    await waitFor(() =>
      expect(
        within(screen.getByRole('radiogroup', { name: 'Theme' })).getByRole('radio', {
          name: 'Light',
        }),
      ).not.toBeDisabled(),
    );
    expect(edits[1]).toMatchObject({
      page: [],
      edit: { kind: 'set', path: ['theme'], choice: 'light' },
    });

    // Text: apply on blur.
    const model = screen.getByRole('textbox', { name: 'Default model' });
    fireEvent.change(model, { target: { value: 'gpt-4' } });
    fireEvent.blur(model);
    await waitFor(() => expect(edits).toHaveLength(3));
    await waitFor(() => expect(screen.getByRole('textbox', { name: 'Default model' })).not.toBeDisabled());
    expect(edits[2]).toMatchObject({
      page: [],
      edit: { kind: 'set', path: ['model'], text: 'gpt-4' },
    });

    // Dirty state and save use the whole original and draft.
    expect(await screen.findByText('Unsaved changes')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(saves).toHaveLength(1));
    expect(saves[0]).toEqual({
      original: initialDraft,
      draft,
      page: [],
    });
    await waitFor(() => expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument());
    expect(fetchMock.calls.some(call => call.url.includes('/api/v1/settings/save'))).toBe(true);
  });

  it('shows an edit refusal beside the page instead of throwing it away', async () => {
    mockFetch({
      '/api/v1/settings': () => jsonResponse(settingsDocument()),
      '/api/v1/settings/edit': () =>
        jsonResponse({
          draft: initialDraft,
          page: rootPage(),
          error: 'Reviews are locked by policy.',
        }),
      '/api/v1/git-identity': () => jsonResponse(defaultGitIdentity()),
    });

    render(<SettingsView path={[]} />);
    await screen.findByRole('button', { name: /Machines/ });

    fireEvent.click(screen.getByRole('switch', { name: 'Reviews' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Reviews are locked by policy.');
    expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument();
  });

  it('shows the per-directory notice and saves the Git identity', async () => {
    const saved: { name: string; email: string }[] = [];
    mockFetch({
      '/api/v1/settings': () => jsonResponse(settingsDocument()),
      '/api/v1/git-identity': (_url, init) => {
        if (init?.method === 'POST') {
          const body = JSON.parse(String(init.body)) as { name: string; email: string };
          saved.push(body);
          return jsonResponse({ name: body.name, email: body.email, per_directory: false });
        }
        return jsonResponse(defaultGitIdentity());
      },
    });

    render(<SettingsView path={[]} />);
    expect(await screen.findByText(/sets the Git author per repository directory/)).toBeInTheDocument();

    const name = screen.getByLabelText('Name');
    const email = screen.getByLabelText('Email');
    fireEvent.change(name, { target: { value: 'Grace Hopper' } });
    fireEvent.change(email, { target: { value: 'grace@example.com' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save Git identity' }));

    await waitFor(() => expect(saved).toHaveLength(1));
    expect(saved[0]).toEqual({ name: 'Grace Hopper', email: 'grace@example.com' });
    expect(await screen.findByText('Git identity saved.')).toBeInTheDocument();
  });

  it('shows the 422 validation sentence inline for Git identity', async () => {
    mockFetch({
      '/api/v1/settings': () => jsonResponse(settingsDocument()),
      '/api/v1/git-identity': (_url, init) => {
        if (init?.method === 'POST') {
          return jsonResponse({ error: 'Git name and email must not be empty.' }, { status: 422 });
        }
        return jsonResponse({ name: null, email: null, per_directory: false });
      },
    });

    render(<SettingsView path={[]} />);
    await screen.findByRole('heading', { name: 'Git identity' });
    const saveButton = await screen.findByRole('button', { name: 'Save Git identity' });
    fireEvent.click(saveButton);
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Git name and email must not be empty.',
    );
  });

  it('follows the route path and loads sub-pages with getSettingsPage', async () => {
    const pageRequests: unknown[] = [];
    mockFetch({
      '/api/v1/settings': () => jsonResponse(settingsDocument()),
      '/api/v1/settings/page': (_url, init) => {
        pageRequests.push(JSON.parse(String(init?.body)));
        return jsonResponse({
          page: childPage(),
          subagent_options: null,
          reviewer_options: null,
        });
      },
      '/api/v1/git-identity': () => jsonResponse(defaultGitIdentity()),
    });

    function Harness() {
      const route = useUiStore(state => state.route);
      return route.name === 'settings' ? <SettingsView path={route.path} /> : null;
    }

    render(<Harness />);
    fireEvent.click(await screen.findByRole('button', { name: /Machines/ }));

    expect(await screen.findByText('Machine kind')).toBeInTheDocument();
    expect(screen.getByText('Local')).toBeInTheDocument();
    expect(pageRequests).toEqual([{ draft: initialDraft, path: ['machines'] }]);
    expect(useUiStore.getState().route).toEqual({ name: 'settings', path: ['machines'] });
  });
});
