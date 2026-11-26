// HTTP transport for the Mjolnir viewer API.
//
// One entry point, `apiFetch`, owns the cross-cutting rules:
// - Credentials ride on the session cookie (`credentials: 'include'`).
// - `401` tells the auth owner the session ended and fails the request.
// - `503` with `x-mj-upgrade: pending` is the daemon explicitly refusing
//   admission during an upgrade handoff. The request was not accepted, so the
//   exact same request (same body, same command IDs) is sent again once the
//   daemon answers `/` normally. Any other failure is reported, never retried.
// - Error bodies are `{ error, code? }`; `ApiError` keeps both.

import type {
  ActionResult,
  AuthSessionStatus,
  BrowserTranscript,
  CompletePathRequest,
  ContainerUpgradePreview,
  ControllerAction,
  CreateBundleRequest,
  CreateBundleResponse,
  GitIdentity,
  HistoryPage,
  LaunchOptions,
  MovePreparation,
  MoveSelection,
  NativeHistoryPage,
  NewSessionPreflightRequest,
  NotificationMessage,
  NotificationMessageSelector,
  PathCompletion,
  PreflightNew,
  PreflightResume,
  PreflightResumeRequest,
  ProjectCatalogView,
  ProjectDiscovery,
  ProjectDiscoveryRequest,
  PromptHistory,
  PromptHistoryScope,
  PushDeviceStatus,
  PushKey,
  SessionDiff,
  SessionRowResponse,
  SettingsDocument,
  SettingsEdit,
  SettingsEditResponse,
  SettingsPageResponse,
  SubagentOptions,
  SuspendPreflight,
  SuspendPreflightQuery,
  ToolCallDetailResponse,
  UploadedAttachment,
  ViewerClientState,
  ViewerSnapshotWire,
  WikiBrief,
  WikiHitTranscript,
  WikiRestoreBody,
  WikiRestoreResponse,
  WikiSearchPage,
  WikiSessionInfo,
  WikiSyncStatus,
  JsonValue,
} from './types';

export class ApiError extends Error {
  readonly status: number;
  readonly code?: string;
  constructor(message: string, status: number, code?: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

/** The auth owner and the upgrade banner register here; the transport stays store-free. */
export interface ApiHooks {
  /** A request got `401`. */
  onUnauthorized: () => void;
  /** An upgrade handoff started (`true`) or ended (`false`). */
  onUpgrade: (pending: boolean) => void;
}

let hooks: ApiHooks = { onUnauthorized: () => {}, onUpgrade: () => {} };

export function configureApi(next: Partial<ApiHooks>): void {
  hooks = { ...hooks, ...next };
}

export interface ApiRequest {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE';
  /** Serialized as JSON with `content-type: application/json`. */
  json?: unknown;
  /** A raw body (attachments); the caller sets `content-type`. */
  body?: BodyInit;
  headers?: Record<string, string>;
  query?: Record<string, string | number | boolean | string[] | undefined | null>;
  signal?: AbortSignal;
}

const UPGRADE_POLL_MS = 250;

function withQuery(path: string, query: ApiRequest['query']): string {
  if (!query) return path;
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null) continue;
    if (Array.isArray(value)) for (const item of value) params.append(key, item);
    else params.set(key, String(value));
  }
  const text = params.toString();
  return text ? `${path}?${text}` : path;
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason);
      return;
    }
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      'abort',
      () => {
        clearTimeout(timer);
        reject(signal.reason);
      },
      { once: true },
    );
  });
}

/** Read a body nobody needs to its end so Chromium does not report an aborted fetch. */
async function drain(response: Response): Promise<void> {
  await response.arrayBuffer().catch(() => {});
}

async function waitForUpgrade(signal?: AbortSignal): Promise<void> {
  hooks.onUpgrade(true);
  try {
    for (;;) {
      await sleep(UPGRADE_POLL_MS, signal);
      try {
        const probe = await fetch('/', { signal, cache: 'no-store' });
        await drain(probe);
        if (probe.ok && probe.headers.get('x-mj-upgrade') !== 'pending') return;
      } catch (error) {
        if (signal?.aborted) throw error;
      }
    }
  } finally {
    hooks.onUpgrade(false);
  }
}

/** Fetch with credentials and upgrade handling; the caller reads the body. */
export async function rawFetch(path: string, request: ApiRequest = {}): Promise<Response> {
  const url = withQuery(path, request.query);
  const headers: Record<string, string> = { ...request.headers };
  let body = request.body;
  if (request.json !== undefined) {
    headers['content-type'] = 'application/json';
    body = JSON.stringify(request.json);
  }
  const init: RequestInit = {
    method: request.method ?? (body === undefined ? 'GET' : 'POST'),
    credentials: 'include',
    headers,
    body,
    signal: request.signal,
  };
  for (;;) {
    const response = await fetch(url, init);
    if (response.status === 503 && response.headers.get('x-mj-upgrade') === 'pending') {
      await drain(response);
      await waitForUpgrade(request.signal);
      continue;
    }
    if (response.status === 401) {
      await drain(response);
      hooks.onUnauthorized();
      throw new ApiError('unauthorized', 401);
    }
    return response;
  }
}

async function failure(response: Response): Promise<ApiError> {
  const text = await response.text().catch(() => '');
  let message = response.statusText || `HTTP ${response.status}`;
  let code: string | undefined;
  if (text) {
    try {
      const body = JSON.parse(text) as { error?: unknown; code?: unknown };
      if (typeof body.error === 'string' && body.error) message = body.error;
      if (typeof body.code === 'string') code = body.code;
    } catch {
      message = text.slice(0, 500);
    }
  }
  return new ApiError(message, response.status, code);
}

/** JSON request. Resolves `undefined` for `202`/`204` (empty body). */
export async function apiFetch<T>(path: string, request: ApiRequest = {}): Promise<T> {
  const response = await rawFetch(path, request);
  if (!response.ok) throw await failure(response);
  if (response.status === 202 || response.status === 204) {
    await drain(response);
    return undefined as T;
  }
  return (await response.json()) as T;
}

export async function apiFetchText(path: string, request: ApiRequest = {}): Promise<string> {
  const response = await rawFetch(path, request);
  if (!response.ok) throw await failure(response);
  return response.text();
}

const seg = encodeURIComponent;

/* -------------------------------------------------------------------------- */
/* Auth                                                                       */
/* -------------------------------------------------------------------------- */

export const getAuthSession = (signal?: AbortSignal) =>
  apiFetch<AuthSessionStatus>('/auth/session', { signal });

/** `204` on success, `401` bad code, `429` locked out. */
export async function login(code: string): Promise<void> {
  const response = await fetch('/auth/session', {
    method: 'POST',
    credentials: 'include',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ code }),
  });
  if (response.ok) {
    await drain(response);
    return;
  }
  // A wrong code is a `401` the login form shows itself, not a signed-out event.
  throw await failure(response);
}

export async function logout(): Promise<void> {
  const response = await fetch('/auth/session', { method: 'DELETE', credentials: 'include' });
  if (!response.ok) throw await failure(response);
  await drain(response);
}

/* -------------------------------------------------------------------------- */
/* Snapshot and rows                                                          */
/* -------------------------------------------------------------------------- */

export const getSnapshot = (signal?: AbortSignal) =>
  apiFetch<ViewerSnapshotWire>('/api/v1/snapshot', { signal });

export const getSessionRow = (id: string, signal?: AbortSignal) =>
  apiFetch<SessionRowResponse>(`/api/v1/sessions/${seg(id)}/row`, { signal });

/* -------------------------------------------------------------------------- */
/* Conversations                                                              */
/* -------------------------------------------------------------------------- */

export interface ConversationQuery {
  afterSeq?: number;
  presentationKey?: string;
}

export const getConversation = (id: string, query: ConversationQuery = {}, signal?: AbortSignal) =>
  apiFetch<BrowserTranscript>(`/api/v1/conversations/${seg(id)}`, {
    query: { after_seq: query.afterSeq, presentation_key: query.presentationKey },
    signal,
  });

export const markConversationRead = (id: string, through: number) =>
  apiFetch<void>(`/api/v1/conversations/${seg(id)}/read`, { method: 'POST', json: { through } });

export const markWorkspaceRead = (workspaceId: string) =>
  apiFetch<void>(`/api/v1/workspaces/${seg(workspaceId)}/read`, { method: 'POST' });

export interface HistoryCursorQuery {
  beforePosition?: number;
  beforeId?: string;
}

export const getHistory = (id: string, before?: HistoryCursorQuery, signal?: AbortSignal) =>
  apiFetch<HistoryPage>(`/api/v1/sessions/${seg(id)}/history`, {
    query: { before_position: before?.beforePosition, before_id: before?.beforeId },
    signal,
  });

export const getNativeAgentHistory = (
  id: string,
  childId: string,
  before?: HistoryCursorQuery,
  signal?: AbortSignal,
) =>
  apiFetch<NativeHistoryPage>(`/api/v1/sessions/${seg(id)}/native-agents/${seg(childId)}/history`, {
    query: { before_position: before?.beforePosition, before_id: before?.beforeId },
    signal,
  });

export const getToolDetail = (
  id: string,
  stableId: string,
  nativeChildId?: string,
  signal?: AbortSignal,
) =>
  apiFetch<ToolCallDetailResponse>(
    nativeChildId
      ? `/api/v1/sessions/${seg(id)}/native-agents/${seg(nativeChildId)}/items/${seg(stableId)}/detail`
      : `/api/v1/sessions/${seg(id)}/items/${seg(stableId)}/detail`,
    { signal },
  );

export const getSessionDiff = (
  id: string,
  options: { base?: string; paths?: string[] } = {},
  signal?: AbortSignal,
) =>
  apiFetch<SessionDiff>(`/api/v1/sessions/${seg(id)}/diff`, {
    query: { json: true, base: options.base, path: options.paths },
    signal,
  });

export const getClientState = (id: string, signal?: AbortSignal) =>
  apiFetch<ViewerClientState>(`/api/v1/sessions/${seg(id)}/client-state`, { signal });

export const putDraft = (id: string, draft: string) =>
  apiFetch<void>(`/api/v1/sessions/${seg(id)}/draft`, { method: 'PUT', json: { draft } });

export function uploadAttachment(
  id: string,
  file: Blob & { name?: string },
  kind: 'image' | 'file',
  signal?: AbortSignal,
): Promise<UploadedAttachment> {
  const headers: Record<string, string> = {
    'content-type': file.type || 'application/octet-stream',
  };
  if (kind === 'file') headers['X-Mjolnir-Filename'] = encodeURIComponent(file.name || 'attachment');
  return apiFetch<UploadedAttachment>(`/api/v1/sessions/${seg(id)}/attachments`, {
    method: 'POST',
    body: file,
    headers,
    query: kind === 'file' ? { mode: 'file' } : undefined,
    signal,
  });
}

export const getPromptHistory = (
  id: string,
  options: { q?: string; scope?: PromptHistoryScope } = {},
  signal?: AbortSignal,
) =>
  apiFetch<PromptHistory>(`/api/v1/sessions/${seg(id)}/prompt-history`, {
    query: { q: options.q, scope: options.scope },
    signal,
  });

export const getNotificationMessage = (
  id: string,
  selector: NotificationMessageSelector,
  signal?: AbortSignal,
) =>
  apiFetch<NotificationMessage>(`/api/v1/sessions/${seg(id)}/notification-message`, {
    query: selector as unknown as Record<string, string | number>,
    signal,
  });

/* -------------------------------------------------------------------------- */
/* Actions                                                                    */
/* -------------------------------------------------------------------------- */

/**
 * `POST /api/v1/actions`. Accepted actions answer `202` with an empty body;
 * `set-container-settings` may answer a `ContainerUpgradePreview` instead.
 */
export async function dispatchAction(action: ControllerAction): Promise<ActionResult> {
  const response = await rawFetch('/api/v1/actions', { method: 'POST', json: action });
  if (!response.ok) throw await failure(response);
  if (response.status === 202 || response.status === 204) {
    await drain(response);
    return { accepted: true };
  }
  if (action.action === 'set-container-settings') {
    const preview = (await response.json()) as ContainerUpgradePreview;
    return { accepted: true, preview };
  }
  await drain(response);
  return { accepted: true };
}

/** `ControllerAction::Destroy` is `#[serde(skip)]`; destroy has its own route. */
export const destroySession = (id: string) =>
  apiFetch<void>(`/api/v1/sessions/${seg(id)}/destroy`, { method: 'POST', json: {} });

/* -------------------------------------------------------------------------- */
/* Launch, projects, preflight                                                */
/* -------------------------------------------------------------------------- */

export const getLaunchOptions = (signal?: AbortSignal) =>
  apiFetch<LaunchOptions>('/api/v1/options', { signal });

export const getProjects = (signal?: AbortSignal) =>
  apiFetch<ProjectCatalogView>('/api/v1/projects', { signal });

export const refreshProjects = (retry = false) =>
  apiFetch<ProjectCatalogView>('/api/v1/projects', { method: 'POST', query: { retry } });

export const discoverProjects = (request: ProjectDiscoveryRequest, signal?: AbortSignal) =>
  apiFetch<ProjectDiscovery>('/api/v1/projects/discover', { method: 'POST', json: request, signal });

export const completePath = (request: CompletePathRequest, signal?: AbortSignal) =>
  apiFetch<PathCompletion>('/api/v1/paths/complete', { method: 'POST', json: request, signal });

export const createBundle = (request: CreateBundleRequest) =>
  apiFetch<CreateBundleResponse>('/api/v1/bundles', { method: 'POST', json: request });

export const deleteBundle = (bundleId: string) =>
  apiFetch<void>(`/api/v1/bundles/${seg(bundleId)}`, { method: 'DELETE' });

export const preflightNew = (request: NewSessionPreflightRequest, signal?: AbortSignal) =>
  apiFetch<PreflightNew>('/api/v1/preflight/new', { method: 'POST', json: request, signal });

export const preflightResume = (request: PreflightResumeRequest, signal?: AbortSignal) =>
  apiFetch<PreflightResume>('/api/v1/preflight/resume', { method: 'POST', json: request, signal });

export const getSubagentOptions = (
  profileId: string,
  query: { model?: string; unrestricted?: boolean; with_disabled?: boolean } = {},
  signal?: AbortSignal,
) =>
  apiFetch<SubagentOptions>(`/api/v1/profiles/${seg(profileId)}/subagent-options`, {
    query,
    signal,
  });

export const prepareMove = (selection: MoveSelection, signal?: AbortSignal) =>
  apiFetch<MovePreparation>('/api/v1/moves/prepare', { method: 'POST', json: selection, signal });

export const suspendPreflight = (
  id: string,
  query: SuspendPreflightQuery = {},
  signal?: AbortSignal,
) =>
  apiFetch<SuspendPreflight>(`/api/v1/sessions/${seg(id)}/lifecycle/suspend-preflight`, {
    method: 'POST',
    query: { ...query },
    signal,
  });

/* -------------------------------------------------------------------------- */
/* Wiki                                                                       */
/* -------------------------------------------------------------------------- */

export const searchWiki = (q?: string, limit?: number, signal?: AbortSignal) =>
  apiFetch<WikiSearchPage>('/api/v1/wiki/search', { query: { q, limit }, signal });

export const startWikiSync = () => apiFetch<WikiSyncStatus>('/api/v1/wiki/sync', { method: 'POST' });

export const watchWikiSync = (epoch: string | number, after: number, signal?: AbortSignal) =>
  apiFetch<WikiSyncStatus>('/api/v1/wiki/sync/watch', { query: { epoch, after }, signal });

export const getWikiSession = (wikiId: string, signal?: AbortSignal) =>
  apiFetch<WikiSessionInfo>(`/api/v1/wiki/sessions/${seg(wikiId)}`, { signal });

export const getWikiBrief = (wikiId: string, maxChars?: number, signal?: AbortSignal) =>
  apiFetch<WikiBrief>(`/api/v1/wiki/sessions/${seg(wikiId)}/brief`, {
    query: { max_chars: maxChars },
    signal,
  });

export const getWikiHits = (
  wikiId: string,
  query: { q: string; context_messages?: number; per_message_chars?: number },
  signal?: AbortSignal,
) => apiFetch<WikiHitTranscript>(`/api/v1/wiki/sessions/${seg(wikiId)}/hits`, { query, signal });

export const restoreWikiSession = (wikiId: string, body: WikiRestoreBody) =>
  apiFetch<WikiRestoreResponse>(`/api/v1/wiki/sessions/${seg(wikiId)}/restore`, {
    method: 'POST',
    json: body,
  });

/* -------------------------------------------------------------------------- */
/* Settings, git identity, push                                               */
/* -------------------------------------------------------------------------- */

export const getSettings = (signal?: AbortSignal) =>
  apiFetch<SettingsDocument>('/api/v1/settings', { signal });

export const getSettingsPage = (draft: JsonValue, path: string[], signal?: AbortSignal) =>
  apiFetch<SettingsPageResponse>('/api/v1/settings/page', {
    method: 'POST',
    json: { draft, path },
    signal,
  });

export const editSettings = (draft: JsonValue, page: string[], edit: SettingsEdit) =>
  apiFetch<SettingsEditResponse>('/api/v1/settings/edit', {
    method: 'POST',
    json: { draft, page, edit },
  });

export const saveSettings = (original: JsonValue, draft: JsonValue, page: string[] = []) =>
  apiFetch<SettingsDocument>('/api/v1/settings/save', {
    method: 'POST',
    json: { original, draft, page },
  });

export const getGitIdentity = (signal?: AbortSignal) =>
  apiFetch<GitIdentity>('/api/v1/git-identity', { signal });

export const saveGitIdentity = (name: string, email: string) =>
  apiFetch<GitIdentity>('/api/v1/git-identity', { method: 'POST', json: { name, email } });

export const getPushKey = (signal?: AbortSignal) => apiFetch<PushKey>('/api/v1/push/key', { signal });

export const getPushStatus = (endpoint: string, signal?: AbortSignal) =>
  apiFetch<PushDeviceStatus>('/api/v1/push/status', { query: { endpoint }, signal });
