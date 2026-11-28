// UI-only state: the hash route, theme, mobile drawer, the back-layer stack
// and toasts. Nothing here talks to the daemon.
//
// The URL is the state. Back, Forward, reload and a shared link work because
// nothing but `navigate` writes `location.hash` and every screen renders from
// the parsed route. Hash formats match the previous viewer so bookmarks and
// push-notification links keep working.

import { create } from 'zustand';

/* -------------------------------------------------------------------------- */
/* Routes                                                                     */
/* -------------------------------------------------------------------------- */

export type ChildKind = 'reviewer';

export type Route =
  | { name: 'notifications' }
  /** The dashboard of one workspace, or of one parent's children. */
  | { name: 'workspace'; workspaceId?: string; subagentParentId?: string; childKind?: ChildKind }
  | {
      name: 'conversation';
      sessionId: string;
      subagentParentId?: string;
      childKind?: ChildKind;
      from?: 'notifications';
    }
  | { name: 'new'; workspaceId: string }
  | {
      name: 'resume';
      workspaceId: string;
      sessionId?: string;
      wikiId?: string;
      from?: 'notifications';
    }
  | { name: 'move'; workspaceId: string; sessionId: string }
  | { name: 'targets' }
  | { name: 'quota' }
  | { name: 'usage' }
  | { name: 'settings'; path: string[] };

export type RouteName = Route['name'];

const ID = '[A-Za-z0-9_-]+';
const ROUTE_PATTERNS: [RegExp, (m: string[]) => Route][] = [
  [new RegExp(`^subagents/(${ID})/(${ID})$`), ([p, s]) => ({ name: 'conversation', sessionId: s, subagentParentId: p })],
  [new RegExp(`^subagents/(${ID})$`), ([p]) => ({ name: 'workspace', subagentParentId: p })],
  [new RegExp(`^reviewers/(${ID})/(${ID})$`), ([p, s]) => ({ name: 'conversation', sessionId: s, subagentParentId: p, childKind: 'reviewer' })],
  [new RegExp(`^reviewers/(${ID})$`), ([p]) => ({ name: 'workspace', subagentParentId: p, childKind: 'reviewer' })],
  [new RegExp(`^workspace/(${ID})/new$`), ([w]) => ({ name: 'new', workspaceId: w })],
  [new RegExp(`^workspace/(${ID})/resume/archive/(${ID})$`), ([w, wiki]) => ({ name: 'resume', workspaceId: w, wikiId: wiki })],
  [new RegExp(`^workspace/(${ID})/resume/(${ID})$`), ([w, s]) => ({ name: 'resume', workspaceId: w, sessionId: s })],
  [new RegExp(`^workspace/(${ID})/resume$`), ([w]) => ({ name: 'resume', workspaceId: w })],
  [new RegExp(`^workspace/(${ID})/move/(${ID})$`), ([w, s]) => ({ name: 'move', workspaceId: w, sessionId: s })],
  [new RegExp(`^workspace/(${ID})$`), ([w]) => ({ name: 'workspace', workspaceId: w })],
  [new RegExp(`^conversation/(${ID})$`), ([s]) => ({ name: 'conversation', sessionId: s })],
  // Routes opened from the notifications screen carry that origin for Back.
  [new RegExp(`^notifications/(${ID})/(${ID})$`), ([w, s]) => ({ name: 'resume', workspaceId: w, sessionId: s, from: 'notifications' })],
  [new RegExp(`^notifications/(${ID})$`), ([s]) => ({ name: 'conversation', sessionId: s, from: 'notifications' })],
  [/^notifications$/, () => ({ name: 'notifications' })],
  [/^targets$/, () => ({ name: 'targets' })],
  [/^quota$/, () => ({ name: 'quota' })],
  [/^usage$/, () => ({ name: 'usage' })],
  // A settings path names keys the user chose, so each segment is URI-encoded.
  [/^settings((?:\/[^/]*)*)$/, ([rest]) => ({ name: 'settings', path: settingsPath(rest ?? '') })],
];

function settingsPath(rest: string): string[] {
  try {
    return rest.split('/').slice(1).map(decodeURIComponent);
  } catch {
    return [];
  }
}

/** Parse `location.hash`. Accepts `#conversation/x` and `#/conversation/x`. An empty or unknown hash is the default workspace route. */
export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#\/?/, '');
  for (const [pattern, build] of ROUTE_PATTERNS) {
    const match = pattern.exec(path);
    if (match) return build(match.slice(1));
  }
  return { name: 'workspace' };
}

/** Whether the hash names no screen, so the app picks one. */
export function isImplicitRoute(hash: string): boolean {
  const path = hash.replace(/^#\/?/, '');
  return !ROUTE_PATTERNS.some(([pattern]) => pattern.test(path));
}

export function routeHash(route: Route): string {
  if ('subagentParentId' in route && route.subagentParentId) {
    const prefix = route.childKind === 'reviewer' ? 'reviewers' : 'subagents';
    return route.name === 'conversation'
      ? `#${prefix}/${route.subagentParentId}/${route.sessionId}`
      : `#${prefix}/${route.subagentParentId}`;
  }
  switch (route.name) {
    case 'new':
      return `#workspace/${route.workspaceId}/new`;
    case 'resume':
      if (route.from === 'notifications' && route.sessionId) {
        return `#notifications/${route.workspaceId}/${route.sessionId}`;
      }
      if (route.wikiId) return `#workspace/${route.workspaceId}/resume/archive/${route.wikiId}`;
      return route.sessionId
        ? `#workspace/${route.workspaceId}/resume/${route.sessionId}`
        : `#workspace/${route.workspaceId}/resume`;
    case 'move':
      return `#workspace/${route.workspaceId}/move/${route.sessionId}`;
    case 'conversation':
      return route.from === 'notifications'
        ? `#notifications/${route.sessionId}`
        : `#conversation/${route.sessionId}`;
    case 'notifications':
      return '#notifications';
    case 'targets':
      return '#targets';
    case 'quota':
      return '#quota';
    case 'usage':
      return '#usage';
    case 'settings':
      return `#settings${route.path.map(key => `/${encodeURIComponent(key)}`).join('')}`;
    case 'workspace':
      return route.workspaceId ? `#workspace/${route.workspaceId}` : '';
  }
}

/** Top-level screens: the ones the bottom navigation and the drawer switch between. */
export function isTopLevelRoute(route: Route): boolean {
  return (
    route.name === 'notifications' ||
    route.name === 'workspace' ||
    route.name === 'targets' ||
    route.name === 'quota' ||
    route.name === 'usage' ||
    route.name === 'settings'
  );
}

/* -------------------------------------------------------------------------- */
/* Theme                                                                      */
/* -------------------------------------------------------------------------- */

export type Theme = 'system' | 'dark' | 'light';
const THEME_KEY = 'mj-theme';

function readTheme(): Theme {
  try {
    const stored = localStorage.getItem(THEME_KEY);
    if (stored === 'dark' || stored === 'light' || stored === 'system') return stored;
  } catch {
    // Storage can be unavailable (private mode); the default applies.
  }
  return 'system';
}

/** Apply without an inline pre-paint script: the stylesheet defaults to dark and follows `prefers-color-scheme` until this runs. */
export function applyTheme(theme: Theme): void {
  const root = document.documentElement;
  if (theme === 'system') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', theme);
}

/* -------------------------------------------------------------------------- */
/* Back layers                                                                */
/* -------------------------------------------------------------------------- */

interface BackLayer {
  id: number;
  close: () => void;
}

/* -------------------------------------------------------------------------- */
/* Toasts                                                                     */
/* -------------------------------------------------------------------------- */

export type ToastKind = 'info' | 'success' | 'error';

export interface Toast {
  id: number;
  kind: ToastKind;
  text: string;
}

/* -------------------------------------------------------------------------- */
/* Store                                                                      */
/* -------------------------------------------------------------------------- */

interface UiState {
  route: Route;
  /** The hash named no screen; the shell chooses the default from session state. */
  implicitRoute: boolean;
  theme: Theme;
  selectedWorkspaceId: string | null;
  setSelectedWorkspaceId: (id: string | null) => void;
  drawerOpen: boolean;
  layers: BackLayer[];
  toasts: Toast[];

  navigate: (route: Route) => void;
  /** Go to an app URL (from a notification) by its hash. */
  navigateToUrl: (url: string) => void;
  setTheme: (theme: Theme) => void;
  setDrawerOpen: (open: boolean) => void;
  /** Register an overlay the Back button and Escape should close. Returns the unregister function. */
  registerBackLayer: (close: () => void) => () => void;
  /** Close the topmost overlay. Returns whether one was open. */
  closeTopmostBackLayer: () => boolean;
  pushToast: (kind: ToastKind, text: string) => number;
  dismissToast: (id: number) => void;
}

let pendingNavigationHash: string | null = null;
let nextLayerId = 0;
let nextToastId = 0;
/** History entries the app pushed for layers and has since removed itself; their `popstate` is ignored. */
let suppressedPops = 0;
/** Layer ids whose history entry the browser already popped. */
const consumedLayers = new Set<number>();

function initialHash(): string {
  return typeof location === 'undefined' ? '' : location.hash;
}

export function readSelectedWorkspaceId(): string | null {
  try { return localStorage.getItem('mj.web.selected_workspace'); } catch { return null; }
}

export const useUiStore = create<UiState>((set, get) => ({
  route: parseRoute(initialHash()),
  implicitRoute: isImplicitRoute(initialHash()),
  theme: readTheme(),
  selectedWorkspaceId: readSelectedWorkspaceId(),
  setSelectedWorkspaceId: id => {
    try {
      if (id === null) localStorage.removeItem('mj.web.selected_workspace');
      else localStorage.setItem('mj.web.selected_workspace', id);
    } catch { /* The choice remains available for this page. */ }
    set({ selectedWorkspaceId: id });
  },
  drawerOpen: false,
  layers: [],
  toasts: [],

  navigate: route => {
    const hash = routeHash(route);
    const changesHash = location.hash !== hash && !(hash === '' && location.hash === '');
    if (changesHash) {
      // Navigating retires overlays now, not at `hashchange`. A dialog that
      // navigates and then closes itself in the same tick would otherwise
      // still look open and `history.back()` over the new route.
      for (const layer of get().layers) consumedLayers.add(layer.id);
    }
    set({ route, implicitRoute: false, drawerOpen: false, ...(changesHash ? { layers: [] } : {}) });
    if (changesHash) {
      pendingNavigationHash = hash;
      location.hash = hash;
    }
  },

  navigateToUrl: url => {
    let target: URL;
    try {
      target = new URL(url, location.origin);
    } catch {
      return;
    }
    if (target.origin !== location.origin) return;
    location.hash = target.hash;
    syncRouteFromLocation();
  },

  setTheme: theme => {
    try {
      localStorage.setItem(THEME_KEY, theme);
    } catch {
      // Not persisted; the choice still applies for this page.
    }
    applyTheme(theme);
    set({ theme });
  },

  setDrawerOpen: open => set({ drawerOpen: open }),

  registerBackLayer: close => {
    const id = ++nextLayerId;
    set(state => ({ layers: [...state.layers, { id, close }] }));
    history.pushState({ mjLayer: id }, '');
    return () => {
      const present = get().layers.some(layer => layer.id === id);
      if (!present) return;
      set(state => ({ layers: state.layers.filter(layer => layer.id !== id) }));
      if (consumedLayers.delete(id)) return;
      // Closed by the app: drop the history entry it pushed.
      suppressedPops += 1;
      history.back();
    };
  },

  closeTopmostBackLayer: () => {
    const { layers } = get();
    const top = layers[layers.length - 1];
    if (!top) return false;
    top.close();
    return true;
  },

  pushToast: (kind, text) => {
    const id = ++nextToastId;
    set(state => ({ toasts: [...state.toasts, { id, kind, text }] }));
    return id;
  },

  dismissToast: id => set(state => ({ toasts: state.toasts.filter(toast => toast.id !== id) })),
}));

/** Make the store's route match `location.hash` (called on `hashchange`). */
export function syncRouteFromLocation(): void {
  const route = parseRoute(location.hash);
  const implicit = isImplicitRoute(location.hash);
  const current = useUiStore.getState();
  if (routeHash(current.route) === routeHash(route) && current.implicitRoute === implicit) return;
  useUiStore.setState({ route, implicitRoute: implicit, drawerOpen: false });
}

/**
 * Wire the browser to the store: `hashchange`, Back for overlays, Escape and
 * the service worker's notification-open message. Returns the teardown.
 */
export function installUiListeners(): () => void {
  applyTheme(useUiStore.getState().theme);
  let lastHash = location.hash;

  const onHashChange = () => {
    // Navigating to another screen retires overlays without touching history.
    if (location.hash !== lastHash) {
      lastHash = location.hash;
      for (const layer of useUiStore.getState().layers) consumedLayers.add(layer.id);
      useUiStore.setState({ layers: [] });
    }
    if (pendingNavigationHash === location.hash && routeHash(useUiStore.getState().route) === location.hash) {
      pendingNavigationHash = null;
      return;
    }
    pendingNavigationHash = null;
    syncRouteFromLocation();
  };

  const onPopState = () => {
    if (suppressedPops > 0) {
      suppressedPops -= 1;
      return;
    }
    if (location.hash !== lastHash) return; // a route change; `hashchange` handles it
    const { layers } = useUiStore.getState();
    const top = layers[layers.length - 1];
    if (!top) return;
    // The browser already popped this layer's history entry.
    consumedLayers.add(top.id);
    top.close();
  };

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'Escape' || event.defaultPrevented) return;
    if (useUiStore.getState().closeTopmostBackLayer()) event.preventDefault();
  };

  const onServiceWorkerMessage = (event: MessageEvent) => {
    const data = event.data as { type?: unknown; url?: unknown } | null;
    if (data?.type === 'mj-notification-open' && typeof data.url === 'string') {
      useUiStore.getState().navigateToUrl(data.url);
    }
  };

  window.addEventListener('hashchange', onHashChange);
  window.addEventListener('popstate', onPopState);
  document.addEventListener('keydown', onKeyDown);
  navigator.serviceWorker?.addEventListener('message', onServiceWorkerMessage);
  return () => {
    window.removeEventListener('hashchange', onHashChange);
    window.removeEventListener('popstate', onPopState);
    document.removeEventListener('keydown', onKeyDown);
    navigator.serviceWorker?.removeEventListener('message', onServiceWorkerMessage);
  };
}
