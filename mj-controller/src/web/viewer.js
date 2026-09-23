import { renderMarkdown } from './markdown.js';
import { renderToolOutput } from './tool-output.js';

/// Build one element. Every piece of this application creates nodes and sets
/// `textContent`; nothing builds markup as a string, which is what makes agent
/// output structurally unable to inject an element.
function el(name, className, textContent) {
  const node = document.createElement(name);
  if (className) node.className = className;
  if (textContent !== undefined) node.textContent = textContent;
  return node;
}

/// A button carrying the data a click handler reads back off it.
function button(label, className, data) {
  const node = el('button', className, label);
  for (const [key, value] of Object.entries(data || {})) node.dataset[key] = value;
  return node;
}

const login = document.querySelector('#login'),
  app = document.querySelector('#app'),
  header = document.querySelector('#shell-header'),
  shellTitle = document.querySelector('#shell-title'),
  backButton = document.querySelector('#back'),
  menuButton = document.querySelector('#menu-button'),
  menu = document.querySelector('#menu'),
  menuVersion = document.querySelector('#menu-version'),
  announcer = document.querySelector('#announcer'),
  workspaceStrip = document.querySelector('#workspaces'),
  sessions = document.querySelector('#sessions'),
  resumable = document.querySelector('#resumable'),
  resumeListView = document.querySelector('#resume-list-view'),
  resumeDetailView = document.querySelector('#resume-detail-view'),
  resumeDetail = document.querySelector('#resume-detail'),
  resumeSearch = document.querySelector('#resume-search'),
  resumeWikiNote = document.querySelector('#resume-wiki-note'),
  resumeArchived = document.querySelector('#resume-archived'),
  resumeArchivedRows = document.querySelector('#resume-archived-rows'),
  resumeDetailBack = document.querySelector('#resume-detail-back'),
  targetsPanel = document.querySelector('#targets'),
  quotaPanel = document.querySelector('#quota'),
  logout = document.querySelector('#logout'),
  newForm = document.querySelector('#new-form'),
  newStep = document.querySelector('#new-step'),
  newProgress = document.querySelector('#new-progress'),
  newBackButton = document.querySelector('#new-back'),
  newNextButton = document.querySelector('#new-next'),
  newError = document.querySelector('#new-error'),
  moveForm = document.querySelector('#move-form'),
  moveStep = document.querySelector('#move-step'),
  moveProgress = document.querySelector('#move-progress'),
  moveBackButton = document.querySelector('#move-back'),
  moveNextButton = document.querySelector('#move-next'),
  moveError = document.querySelector('#move-error'),
  actionError = document.querySelector('#action-error'),
  feed = document.querySelector('#conversation-feed'),
  feedScroll = document.querySelector('#conversation-scroll'),
  conversationTransition = document.querySelector('#conversation-transition'),
  conversationTransitionTitle = document.querySelector('#conversation-transition-title'),
  conversationTransitionStage = document.querySelector('#conversation-transition-stage'),
  conversationTransitionNotice = document.querySelector('#conversation-transition-notice'),
  conversationTransitionError = document.querySelector('#conversation-transition-error'),
  conversationTransitionCancel = document.querySelector('#conversation-transition-cancel'),
  jumpToLatest = document.querySelector('#jump-to-latest'),
  cancelTurnButton = document.querySelector('#cancel-turn'),
  commandPalette = document.querySelector('#command-palette'),
  sendButton = document.querySelector('#send-button'),
  queue = document.querySelector('#conversation-queue'),
  shells = document.querySelector('#conversation-shells'),
  conversationSide = document.querySelector('#conversation-side'),
  conversationSummary = conversationSide?.querySelector('summary'),
  queueHeading = queue?.previousElementSibling,
  shellsHeading = shells?.previousElementSibling,
  backgroundTasks = document.querySelector('#conversation-background-tasks'),
  backgroundTasksHeading = backgroundTasks?.previousElementSibling,
  elicitations = document.querySelector('#elicitations'),
  reviewHost = document.querySelector('#turn-review'),
  promptSettings = document.querySelector('#prompt-settings'),
  subagentsButton = document.querySelector('#subagents-button'),
  promptText = document.querySelector('#prompt-text'),
  attachments = document.querySelector('#attachments'),
  attachImage = document.querySelector('#attach-image'),
  imagePicker = document.querySelector('#image-picker'),
  voiceInput = document.querySelector('#voice-input'),
  voiceControls = document.querySelector('#voice-controls'),
  voiceStatus = document.querySelector('#voice-status'),
  voiceCancel = document.querySelector('#voice-cancel');

/// Below this, silence is ordinary and saying so is noise rather than news.
/// The same threshold as `mj_core::activity::SILENCE_WORTH_REPORTING`, so a
/// turn does not read as quiet here and ordinary in `mj sessions`.
const SILENCE_WORTH_REPORTING_SECONDS = 60;

/// Every page, by the route name that shows it.
const PAGES = {
  dashboard: document.querySelector('#dashboard'),
  new: document.querySelector('#new-page'),
  resume: document.querySelector('#resume-page'),
  move: document.querySelector('#move-page'),
  targets: document.querySelector('#targets-page'),
  quota: document.querySelector('#quota-page'),
  conversation: document.querySelector('#conversation'),
};

/// Transcript nodes by entry id, so an update patches the row it belongs to
/// rather than searching the whole document for it.
const entryNodes = new Map();
let snapshot,
  route = { name: 'dashboard' },
  resumeRouteVisit = 0,
  currentSession,
  moveDraft,
  cursor = 0,
  presentationKey = null,
  acknowledged = 0,
  eventSource,
  conversationMode = null;

/// Actions the browser has asked for and not yet heard back about.
///
/// A control is disabled because it is in this set, not because a handler
/// disabled it: state decides, so a re-render cannot lose the fact and a
/// failure cannot leave a button dead.
const pendingActions = new Set();
const pendingLifecycleActions = new Map();
/// Stop failures are tied to a task identity, so a fresh snapshot can redraw
/// the row without losing the inline error that tells the person what failed.
const backgroundTaskErrors = new Map();

async function upgradeAwareFetch(url, options = {}) {
  for (;;) {
    const response = await fetch(url, options);
    if (response.status !== 503 || response.headers.get('x-mj-upgrade') !== 'pending') {
      return response;
    }
    // The daemon explicitly refused admission. Keep the exact request body,
    // including command IDs and steering targets, until its replacement is ready.
    await response.body?.cancel();
    for (;;) {
      options.signal?.throwIfAborted();
      await new Promise(resolve => setTimeout(resolve, 250));
      try {
        const probe = await fetch('/', { signal: options.signal, cache: 'no-store' });
        await probe.body?.cancel();
        if (probe.ok && probe.headers.get('x-mj-upgrade') !== 'pending') break;
      } catch (error) {
        if (options.signal?.aborted) throw error;
      }
    }
  }
}

async function request(url, options = {}) {
  const response = await upgradeAwareFetch(url, {
    ...options,
    headers: { 'content-type': 'application/json', ...(options.headers || {}) },
  });
  if (response.status === 401) {
    // Authentication expired. Every route has to reach the login swap, not
    // only the snapshot refresh, or a phone sits on a dead page issuing
    // requests that will never succeed.
    showLogin();
    throw new Error('unauthorized');
  }
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    const failure = new Error(body.error || response.statusText);
    // Callers that treat one status specially, such as SessionWiki being
    // switched off, need the code and not only the sentence.
    failure.status = response.status;
    throw failure;
  }
  if (response.status === 202 || response.status === 204) return null;
  return response.json();
}

/// Upload one image as raw bytes. JSON action requests have a deliberately
/// different content type and body limit, so keeping this path separate makes
/// it impossible to accidentally base64 an image back into the prompt.
async function uploadAttachment(sessionId, file, signal) {
  const response = await upgradeAwareFetch(
    `/api/sessions/${encodeURIComponent(sessionId)}/attachments`,
    {
      method: 'POST',
      body: file,
      signal,
      headers: { 'content-type': file.type || 'application/octet-stream' },
    },
  );
  if (response.status === 401) {
    showLogin();
    throw new Error('unauthorized');
  }
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error || 'the image upload failed');
  }
  const image = await response.json();
  if (!image.attachment || image.data_base64) {
    throw new Error('the server returned an invalid image attachment');
  }
  return image;
}

/// Say something once, for a screen reader.
function announce(message) {
  announcer.textContent = message;
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------
//
// The URL is the state. Back, Forward, reload and a shared link all work
// because nothing but the router writes `location.hash`, and every page is
// rendered from what the router parsed rather than from what a click handler
// remembered.

const ID = '[A-Za-z0-9_-]+';
const ROUTE_PATTERNS = [
  [new RegExp(`^#subagents/(${ID})/(${ID})$`), ([parentId, sessionId]) => ({ name: 'conversation', sessionId, subagentParentId: parentId })],
  [new RegExp(`^#subagents/(${ID})$`), ([parentId]) => ({ name: 'dashboard', subagentParentId: parentId })],
  [new RegExp(`^#workspace/(${ID})/new$`), ([id]) => ({ name: 'new', workspaceId: id })],
  [new RegExp(`^#workspace/(${ID})/resume/archive/(${ID})$`), ([workspaceId, wikiId]) => ({ name: 'resume', workspaceId, wikiId })],
  [new RegExp(`^#workspace/(${ID})/resume/(${ID})$`), ([workspaceId, sessionId]) => ({ name: 'resume', workspaceId, sessionId })],
  [new RegExp(`^#workspace/(${ID})/resume$`), ([id]) => ({ name: 'resume', workspaceId: id })],
  [new RegExp(`^#workspace/(${ID})/move/(${ID})$`), ([workspaceId, sessionId]) => ({ name: 'move', workspaceId, sessionId })],
  [new RegExp(`^#workspace/(${ID})$`), ([id]) => ({ name: 'dashboard', workspaceId: id })],
  [new RegExp(`^#conversation/(${ID})$`), ([id]) => ({ name: 'conversation', sessionId: id })],
  [/^#targets$/, () => ({ name: 'targets' })],
  [/^#quota$/, () => ({ name: 'quota' })],
];

function parseRoute(hash) {
  for (const [pattern, build] of ROUTE_PATTERNS) {
    const match = pattern.exec(hash);
    if (match) return build(match.slice(1));
  }
  return { name: 'dashboard' };
}

function routeHash(next) {
  if (next.subagentParentId) {
    return next.name === 'conversation'
      ? `#subagents/${next.subagentParentId}/${next.sessionId}`
      : `#subagents/${next.subagentParentId}`;
  }
  switch (next.name) {
    case 'new':
      return `#workspace/${next.workspaceId}/new`;
    case 'resume':
      if (next.wikiId) return `#workspace/${next.workspaceId}/resume/archive/${next.wikiId}`;
      return next.sessionId
        ? `#workspace/${next.workspaceId}/resume/${next.sessionId}`
        : `#workspace/${next.workspaceId}/resume`;
    case 'move':
      return `#workspace/${next.workspaceId}/move/${next.sessionId}`;
    case 'conversation':
      return `#conversation/${next.sessionId}`;
    case 'targets':
      return '#targets';
    case 'quota':
      return '#quota';
    default:
      return next.workspaceId ? `#workspace/${next.workspaceId}` : '';
  }
}

/// Go to a route. Assigning the hash it already has fires no `hashchange`, so
/// the render is called directly in that case rather than being dropped.
function navigate(next) {
  const hash = routeHash(next);
  const current = location.hash;
  if (hash === current || (!hash && !current)) {
    applyRoute();
    return;
  }
  location.hash = hash;
}

/// The workspace the route names, or the one to fall back to.
function selectedWorkspaceId() {
  const workspaces = snapshot?.workspaces || [];
  if (route.subagentParentId) {
    return snapshot?.sessions.find(session => session.id === route.subagentParentId)?.workspace_id;
  }
  if (route.workspaceId && workspaces.some(w => w.id === route.workspaceId)) {
    return route.workspaceId;
  }
  if (route.name === 'conversation') {
    const session = snapshot?.sessions.find(s => s.id === route.sessionId);
    if (session?.workspace_id) return session.workspace_id;
  }

  return workspaces[0]?.id;
}

function applyRoute() {
  cancelSessionPress();
  const previousRoute = route;
  route = parseRoute(location.hash);
  if (routeHash(previousRoute) !== routeHash(route)) {
    resumeRouteVisit += 1;
    if (previousRoute.name === 'resume' && !previousRoute.sessionId && !previousRoute.wikiId && snapshot) {
      resumeListState(previousRoute.workspaceId).scrollTop = window.scrollY;
    }
  }
  if (!snapshot) return;

  // The dashboard names its workspace in the URL, so a reload, a Back press
  // and a shared link all return to the same one. An empty hash is the state
  // a first visit is in, and canonicalising it here is what gives every later
  // navigation something to go back to.
  if (route.name === 'dashboard' && !route.workspaceId && !route.subagentParentId) {
    const workspaceId = selectedWorkspaceId();
    if (workspaceId) {
      navigate({ name: 'dashboard', workspaceId });
      return;
    }
  }

  // A conversation route only means a conversation while that session still
  // has one. Otherwise it is a stale link, and the dashboard is the answer.
  if (route.name === 'conversation') {
    const session = snapshot.sessions.find(s => s.id === route.sessionId);
    const virtualParent = route.subagentParentId
      ? snapshot.sessions.find(s => s.id === route.subagentParentId)
      : null;
    if (!session
      || (route.subagentParentId && !virtualParent?.subagent_session_ids?.includes(session.id))
      || (!session.capabilities?.open
        && !isTransitioningSession(session)
        && !isLoadingConversationSession(session))) {
      navigate(route.subagentParentId
        ? { name: 'dashboard', subagentParentId: route.subagentParentId }
        : { name: 'dashboard', workspaceId: selectedWorkspaceId() });
      return;
    }
  }

  if (route.name === 'move') {
    const session = snapshot.sessions.find(s => s.id === route.sessionId);
    if (!session?.capabilities?.move_session
      && session?.operation?.kind !== 'move'
      && !session?.move_recovery?.checkpoint_retained) {
      navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
      return;
    }
  }

  const name = PAGES[route.name] ? route.name : 'dashboard';
  for (const [key, page] of Object.entries(PAGES)) page.classList.toggle('hidden', key !== name);
  workspaceStrip.classList.toggle('hidden', name === 'conversation' && !route.subagentParentId);
  backButton.classList.toggle('hidden', name === 'dashboard');
  shellTitle.textContent =
    {
      new: 'New session',
      resume: 'Resume',
      move: 'Move session',
      targets: 'Targets',
      quota: 'Quota',
      conversation: 'Conversation',
    }[name] || 'MJ';

  if (name === 'conversation') {
    openConversation(route.sessionId);
  } else if (currentSession) {
    leaveConversation();
  }
  // Arriving at the wizard starts it over; leaving it discards what was
  // half-answered rather than keeping it to surprise the next visit.
  if (name !== 'new') {
    abortPendingNewPreflight();
    newDraft = null;
  }
  if (name !== 'move') moveDraft = null;
  renderRoute();
  // A screen reader should land at the top of the page it just moved to
  // rather than wherever it happened to be.
  PAGES[name].setAttribute('tabindex', '-1');
  PAGES[name].focus({ preventScroll: true });
  if (name === 'resume') {
    const visit = resumeRouteVisit;
    requestAnimationFrame(() => {
      if (route.name !== 'resume' || resumeRouteVisit !== visit) return;
      if (route.sessionId || route.wikiId) {
        window.scrollTo(0, 0);
      } else {
        const state = resumeListState(route.workspaceId);
        const row = [...resumable.children].find(child => child.dataset?.sessionId === state.focusSessionId);
        row?.focus({ preventScroll: true });
        window.scrollTo(0, state.scrollTop);
      }
    });
  }
  announce(shellTitle.textContent);
}

function renderRoute() {
  if (!snapshot) return;
  if (route.name !== 'dashboard') closeSessionMenu(false);
  renderWorkspaces();
  renderLaunchFailures();
  switch (route.name) {
    case 'new':
      renderNewForm();
      break;
    case 'resume':
      renderResumable();
      break;
    case 'move':
      renderMoveForm();
      break;
    case 'targets':
      renderTargets();
      break;
    case 'quota':
      renderQuota();
      break;
    case 'conversation':
      break;
    default:
      renderSessions();
  }
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

const dismissedLaunchFailures = new Set();

function renderLaunchFailures() {
  const notices = (snapshot.launch_failures || []).filter(
    failure => route.name === 'dashboard' && failure.workspace_id === selectedWorkspaceId() && !dismissedLaunchFailures.has(failure.id),
  );
  const failureCards = notices.map(failure => {
    const card = el('div', 'card');
    card.append(el('p', '', 'A session could not be started. Check the project and target, then retry. Details are in the daemon logs.'));
    const dismiss = el('button', 'secondary', 'Dismiss launch error');
    dismiss.onclick = () => {
      dismissedLaunchFailures.add(failure.id);
      renderLaunchFailures();
    };
    card.append(dismiss);
    return card;
  });
  document.querySelector('#launch-failures').replaceChildren(...failureCards);
}

function renderWorkspaces() {
  const selected = selectedWorkspaceId();
  if (route.subagentParentId) {
    const parent = snapshot.sessions.find(session => session.id === route.subagentParentId);
    const tab = el('div', 'tab virtual-workspace');
    tab.setAttribute('role', 'tab');
    tab.setAttribute('aria-selected', 'true');
    tab.setAttribute('aria-current', 'page');
    tab.append(el('span', '', parent?.title || route.subagentParentId));
    const close = el('button', 'virtual-workspace-close', '×');
    close.type = 'button';
    close.setAttribute('aria-label', `Close ${parent?.title || 'session'} sub-agent workspace`);
    close.onclick = event => {
      event.stopPropagation();
      navigate({ name: 'conversation', sessionId: route.subagentParentId });
    };
    tab.append(close);
    workspaceStrip.replaceChildren(tab);
    return;
  }
  workspaceStrip.replaceChildren(
    ...(snapshot.workspaces || []).map(workspace => {
      const tab = el('button', 'tab', workspace.name);
      tab.setAttribute('role', 'tab');
      tab.setAttribute('aria-selected', String(workspace.id === selected));
      // Selection is a word to a screen reader and a border to everyone else,
      // never colour alone.
      if (workspace.id === selected) tab.setAttribute('aria-current', 'page');
      tab.dataset.workspaceId = workspace.id;
      return tab;
    }),
  );
}

// ---------------------------------------------------------------------------
// The session list
// ---------------------------------------------------------------------------

// Dashboard order is deliberately a view concern.  A live snapshot may
// report a newer activity watermark for an existing session, but moving that
// row under a reader's finger makes the dashboard feel broken.  Each
// workspace gets one seed order per document; ranks are retained after a row
// disappears so a reconnect cannot make it jump when it returns.
const dashboardOrders = new Map();
const sessionCards = new Map();
const sessionItems = new Map();
const sessionGroups = new Map();
let openSessionMenuId = null;
let openSessionMenuTrigger = null;
let suppressedSessionClickId = null;
let activeSessionPress = null;
let snapshotReceivedAtMs = 0;
let dashboardOrderSeeded = false;

function reconcileChildren(parent, desired) {
  // Remove departed siblings before inserting arrivals, so removing an earlier
  // row never detaches and reinserts the focused row. Ordinary refreshes do
  // no structural DOM work at all.
  const desiredSet = new Set(desired);
  for (const child of [...parent.children]) {
    if (!desiredSet.has(child)) parent.removeChild(child);
  }
  for (let index = 0; index < desired.length; index += 1) {
    if (parent.children[index] !== desired[index]) {
      parent.insertBefore(desired[index], parent.children[index] || null);
    }
  }
}

function epochMs(value) {
  if (value === undefined || value === null || value === '') return null;
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function epochSecondsMs(value) {
  if (value === undefined || value === null || value === '') return null;
  if (typeof value === 'number' && Number.isFinite(value)) return value * 1000;
  return epochMs(value);
}

function sessionActivityMs(session) {
  return epochMs(session.last_activity_at_ms) ?? epochMs(session.created_at) ?? 0;
}

function projectKeyFor(session) {
  return session.project_key || session.bundle_id || session.id;
}

function orderState(workspaceId, live) {
  let state = dashboardOrders.get(workspaceId);
  if (!state) {
    state = { sessions: new Map(), groups: new Map(), nextSession: 0, nextGroup: 0 };
    dashboardOrders.set(workspaceId, state);
    const initial = [...live].sort((left, right) =>
      sessionActivityMs(right) - sessionActivityMs(left) || left.id.localeCompare(right.id),
    );
    for (const session of initial) {
      if (!state.sessions.has(session.id)) state.sessions.set(session.id, state.nextSession++);
    }
    const maxima = new Map();
    for (const session of initial) {
      const key = projectKeyFor(session);
      maxima.set(key, Math.max(maxima.get(key) ?? 0, sessionActivityMs(session)));
    }
    [...maxima.entries()]
      .sort((left, right) =>
        right[1] - left[1] ||
        left[0].localeCompare(right[0]),
      )
      .forEach(([key]) => state.groups.set(key, state.nextGroup++));
  }
  // New ids append to the remembered order.  Deliberately never delete a
  // rank: a stopped session can return after a reconnect without reordering
  // every row below it.
  for (const session of live) {
    if (!state.sessions.has(session.id)) state.sessions.set(session.id, state.nextSession++);
    const key = projectKeyFor(session);
    if (!state.groups.has(key)) state.groups.set(key, state.nextGroup++);
  }
  return state;
}

function seedDashboardOrders(data) {
  if (dashboardOrderSeeded) return;
  dashboardOrderSeeded = true;
  const workspaceIds = new Set((data.workspaces || []).map(workspace => workspace.id));
  for (const session of data.sessions || []) {
    if (session.workspace_id) workspaceIds.add(session.workspace_id);
  }
  for (const workspaceId of workspaceIds) {
    const workspaceLive = (data.sessions || []).filter(session =>
      session.workspace_id === workspaceId && isDashboardSession(session),
    );
    orderState(workspaceId, workspaceLive);
  }
  // Snapshots predating workspaces still have a single implicit workspace.
  if (!(data.workspaces || []).length) {
    orderState('', (data.sessions || []).filter(session =>
      isDashboardSession(session),
    ));
  }
}

function isTransitioningSession(session) {
  if (!session) return false;
  if (session.transitioning === true) return true;
  // A snapshot from just before the shared boolean was deployed still has
  // enough information to protect its transcript: every operation except a
  // normal checkpoint owns the conversation until completion.
  return Boolean(session.operation && session.operation.kind !== 'checkpoint');
}

/// A live session can briefly lose its conversation projection just after a
/// lifecycle operation hands it back to the running worker. This is not a
/// lifecycle transition (and therefore must not make ordinary checkpoints or
/// reconnects hide a readable conversation): it is only the completion gap of
/// the transition route already open in this tab.
function isLoadingConversationSession(session) {
  return Boolean(
    session
    && session.id === currentSession
    && !session.capabilities?.open
    && !isTransitioningSession(session)
    && session.operation?.kind !== 'checkpoint'
    && session.lifecycle === 'live'
    && !session.has_error
    && (conversationMode === 'loading' || conversationMode?.startsWith('transition:')),
  );
}

function isDashboardSession(session) {
  return ['live', 'starting', 'suspending'].includes(session.lifecycle)
    || isTransitioningSession(session);
}

function orderedSessions(live) {
  const workspaceId = selectedWorkspaceId();
  const state = orderState(workspaceId || '', live);
  return [...live].sort((left, right) =>
    state.sessions.get(left.id) - state.sessions.get(right.id) || left.id.localeCompare(right.id),
  );
}

/// Whether a session is a sub-agent rather than one the person started. Every
/// list of top-level sessions filters with this, so the lists cannot disagree.
function isSubagentSession(session) {
  return Boolean(session?.subagent_parent_id);
}

function liveSessions() {
  if (route.subagentParentId) {
    const parent = snapshot.sessions.find(session => session.id === route.subagentParentId);
    const children = new Set(parent?.subagent_session_ids || []);
    return (snapshot.sessions || []).filter(session => children.has(session.id));
  }
  const workspaceId = selectedWorkspaceId();
  return (snapshot.sessions || []).filter(
    session =>
      session.workspace_id === workspaceId &&
      !isSubagentSession(session) &&
      isDashboardSession(session),
  );
}

/// Sessions grouped by the controller's projected project identity.
///
/// The controller publishes an opaque `project_key` and the same short
/// `project_label` the TUI uses. Keep those fields separate: labels can be
/// shared by different projects, while keys must never merge them. Group and
/// session ranks are seeded from activity, then frozen for this document.
function byProject(list) {
  const groups = new Map();
  for (const session of list) {
    // `bundle_id` keeps older snapshots renderable; current snapshots always
    // provide the opaque project key. The session id is only a last-resort
    // boundary for malformed legacy data, never a project label.
    const key = projectKeyFor(session);
    if (!groups.has(key)) groups.set(key, { key, label: session.project_label || key, sessions: [] });
    groups.get(key).sessions.push(session);
  }
  const state = orderState(route.subagentParentId || selectedWorkspaceId() || '', list);
  return [...groups.values()].sort((left, right) =>
    state.groups.get(left.key) - state.groups.get(right.key) || left.key.localeCompare(right.key),
  );
}

function renderSessions() {
  const groups = byProject(orderedSessions(liveSessions()));
  if (openSessionMenuId && !groups.some(group => group.sessions.some(session => session.id === openSessionMenuId))) {
    closeSessionMenu();
  }
  const parent = snapshot.sessions.find(s => s.id === route.subagentParentId);
  const nativeGroups = renderNativeSubagents(parent);
  if (!groups.length && !nativeGroups.length) {
    sessions.replaceChildren(el(
      'p',
      'dim',
      route.subagentParentId
        ? 'No sub-agents have been started for this session.'
        : 'No live sessions or operations in this workspace.',
    ));
    return;
  }
  const renderedGroups = groups.map(group => {
    const groupId = `${selectedWorkspaceId() || ''}\u001f${group.key}`;
    let section = sessionGroups.get(groupId);
    if (!section) {
      section = el('section', 'project');
      const heading = el('h2', 'project-heading');
      const label = el('span');
      const count = el('span', 'dim');
      heading.append(label, count);
      const list = el('div', 'project-sessions');
      list.setAttribute('role', 'list');
      section.append(heading, list);
      section._headingLabel = label;
      section._headingCount = count;
      section._sessionList = list;
      sessionGroups.set(groupId, section);
    }
    section._headingLabel.textContent = group.label;
    section._headingCount.textContent = ` ${group.sessions.length}`;
    const items = group.sessions.map(session => {
      let card = sessionCards.get(session.id);
      if (!card) {
        card = sessionCard(session);
        sessionCards.set(session.id, card);
      } else {
        updateSessionCard(card, session);
      }
      let item = sessionItems.get(session.id);
      if (!item) {
        item = el('div');
        item.setAttribute('role', 'listitem');
        sessionItems.set(session.id, item);
      }
      if (item.firstChild !== card) item.replaceChildren(card);
      return item;
    });
    reconcileChildren(section._sessionList, items);
    return section;
  });
  reconcileChildren(sessions, [...renderedGroups, ...nativeGroups]);
}

function renderNativeSubagents(parent) {
  if (!parent) return [];
  const agents = parent.native_subagents || [];
  return [
    ['Working', a => a.state === 'running'],
    ['Idle and reusable', a => a.state !== 'running' && a.availability === 'available'],
    ['History and availability unknown', a => a.state !== 'running' && a.availability !== 'available'],
  ].flatMap(([label, matches]) => {
    const entries = agents.filter(matches);
    if (!entries.length) return [];
    const section = el('section', 'project'); section.append(el('h2', '', label));
    for (const agent of entries) {
      const row = el('div', 'session-card');
      const availability = agent.availability === 'available' ? 'reusable' : agent.availability === 'unavailable' ? 'unavailable' : 'availability unknown';
      row.append(el('strong', '', agent.name), el('p', '', `${agent.state} · ${availability}`));
      if (agent.availability_reason) row.append(el('p', 'dim', agent.availability_reason));
      const history = button('View history');
      history.onclick = () => openNativeHistory(parent.id, agent);
      row.append(history); section.append(row);
    }
    return [section];
  });
}

async function openNativeHistory(owner, agent) {
  const modal = el('dialog', 'native-agent-history');
  const close = button('Close'); close.onclick = () => modal.close();
  const earlier = button('Load earlier');
  const content = el('div');
  const error = el('p', 'error');
  modal.append(el('h2', '', agent.name), close, earlier, error, content);
  document.body.append(modal); modal.addEventListener('close', () => modal.remove()); modal.showModal();
  let before = null;
  let generation = null;
  async function load() {
    earlier.disabled = true;
    try {
      const query = before ? `?before_position=${before.position}&before_id=${encodeURIComponent(before.stable_id)}` : '';
      const page = await request(`/api/v1/sessions/${encodeURIComponent(owner)}/native-agents/${encodeURIComponent(agent.session_id)}/history${query}`);
      if (!modal.open) return;
      if (generation !== null && generation !== page.generation) {
        content.replaceChildren(); before = null; generation = null;
        error.textContent = 'History changed during reload. Load it again.';
        return;
      }
      generation = page.generation;
      const rows = page.items.map(item => {
        const row = el('section'); row.append(el('h3', '', item.role), renderMarkdown(item.text)); return row;
      });
      content.prepend(...rows);
      before = page.items[0] || before;
      earlier.hidden = !page.has_more;
      if (!before) content.textContent = 'No recorded transcript.';
    } catch (failure) { if (modal.open) error.textContent = failure.message; }
    finally { earlier.disabled = false; }
  }
  earlier.onclick = load;
  await load();
}

/// One session row.
///
/// Every control here appears because a capability the daemon published says
/// it may. Nothing on this page infers what is legal from a status string.
function sessionCard(session) {
  const card = el('article', 'card session');
  card.dataset.sessionId = session.id;
  const titleRow = el('div', 'session-title-row');
  const heading = el('h3');
  const attention = el('span', 'session-attention');
  const menuTrigger = button('⋯', 'session-menu-trigger', { sessionMenu: session.id });
  menuTrigger.type = 'button';
  menuTrigger.setAttribute('aria-haspopup', 'menu');
  menuTrigger.setAttribute('aria-expanded', 'false');
  const menu = el('div', 'session-menu hidden');
  menu.setAttribute('role', 'menu');
  menu.dataset.sessionId = session.id;
  titleRow.append(heading, attention, menuTrigger, menu);

  const meta = el('div', 'session-meta');
  const location = el('span', 'session-location');
  const profile = el('span', 'session-profile');
  meta.append(location, profile);
  const activity = el('p', 'session-activity');
  card.append(titleRow, meta, activity);
  card._heading = heading;
  card._attention = attention;
  card._menuTrigger = menuTrigger;
  card._menu = menu;
  card._location = location;
  card._profile = profile;
  card._activity = activity;
  card._sessionMenuSignature = '';
  updateSessionCard(card, session);
  return card;
}

function attentionParts(session) {
  const parts = [];
  if (session.operation?.kind === 'move') parts.push(['→', 'Moving']);
  if (session.has_error) parts.push(['!', 'Error']);
  if (session.pending_elicitations?.length) parts.push(['?', 'Input needed']);
  const queued = (session.queued_prompts || []).length;
  if (queued) parts.push([String(queued), `${queued} queued prompt${queued === 1 ? '' : 's'}`]);
  return parts;
}

function sessionMenuActions(session) {
  const can = session.capabilities || {};
  const actions = [];
  if (session.configuration_issue) actions.push(['Repair configuration…', 'secondary', 'repair-config']);
  if (can.rename) actions.push(['Rename', 'secondary', 'rename']);
  if (can.cancel_operation) actions.push(['Cancel operation', 'danger', 'cancel']);
  if (can.suspend) actions.push(['Suspend session…', 'secondary', 'suspend']);
  if (can.destroy) actions.push(['Destroy session…', 'danger', 'destroy']);
  if (can.resume) actions.push(['Resume', '', 'resume']);
  if (can.move_session) actions.push(['Move…', '', 'move']);
  return actions;
}

function updateSessionCard(card, session) {
  card._session = session;
  const can = session.capabilities || {};
  const openable = can.open === true || isTransitioningSession(session);
  card.dataset.openable = String(openable);
  if (openable) {
    card.setAttribute('role', 'link');
    card.setAttribute('tabindex', '0');
  } else {
    card.removeAttribute('role');
    card.removeAttribute('tabindex');
  }
  const attention = attentionParts(session);
  const attentionText = attention.map(([, label]) => label.toLowerCase()).join(', ');
  card.setAttribute(
    'aria-label',
    `${can.open === true ? 'Open session' : openable ? 'View session status' : 'Session'} ${session.title || session.id}${attentionText ? `; needs attention: ${attentionText}` : ''}`,
  );
  renderSessionTitle(card._heading, session);
  card._attention.replaceChildren(
    ...attention.map(([glyph, label]) => {
      const node = el('span', `session-attention-item ${label === 'Error' || label === 'Input needed' ? 'alert' : ''}`, glyph);
      node.setAttribute('aria-label', label);
      node.setAttribute('role', 'img');
      node.title = label;
      return node;
    }),
  );
  card._location.textContent = session.display_location || session.target_id || '';
  card._location.title = card._location.textContent;
  card._profile.textContent = session.profile_id || '';
  card._profile.title = card._profile.textContent;
  updateSessionActivity(card, session);
  updateSessionMenu(card, session);
}

function updateSessionMenu(card, session) {
  const actions = sessionMenuActions(session);
  const signature = actions.map(action => action[2]).join('|');
  const menuChanged = card._sessionMenuSignature !== signature;
  if (menuChanged) {
    const activeAction = card._menu?.ownerDocument?.activeElement?.dataset?.action;
    card._menu.replaceChildren(
      ...actions.map(([label, className, actionName]) => {
        const control = action(label, className, {
          action: actionName,
          id: session.id,
          profile: session.profile_id,
          target: session.target_id,
        });
        control.setAttribute('role', 'menuitem');
        return control;
      }),
    );
    card._sessionMenuSignature = signature;
    if (activeAction && card._menu.classList && !card._menu.classList.contains('hidden')) {
      const next = card._menu.querySelector(`button[data-action="${activeAction}"]:not(:disabled)`)
        || card._menu.querySelector('button:not(:disabled)');
      next?.focus({ preventScroll: true });
    }
  } else {
    for (const control of card._menu.querySelectorAll?.('button[data-action]') || []) {
      control.dataset.profile = session.profile_id || '';
      control.dataset.target = session.target_id || '';
      control.disabled = pendingActions.has(`${control.dataset.action}:${session.id}`);
    }
  }
  card._menuTrigger.disabled = actions.length === 0;
  card._menuTrigger.setAttribute('aria-label', `Actions for ${session.title || session.id}`);
  card._menuTrigger.setAttribute('aria-expanded', String(openSessionMenuId === session.id));
  if (openSessionMenuId === session.id && !actions.length) closeSessionMenu(false);
}

function serverClockMs() {
  const server = epochMs(snapshot?.server_time_ms);
  if (server == null || !snapshotReceivedAtMs) return Date.now();
  return server + (Date.now() - snapshotReceivedAtMs);
}

function formatClock(milliseconds) {
  const seconds = Math.max(0, Math.floor(Number(milliseconds || 0) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  if (minutes < 60) return `${minutes}m${String(remainingSeconds).padStart(2, '0')}s`;
  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  if (hours < 24) return `${hours}h${String(remainingMinutes).padStart(2, '0')}m`;
  const days = Math.floor(hours / 24);
  const remainingHours = hours % 24;
  return `${days}d${String(remainingHours).padStart(2, '0')}h`;
}

function localClock(milliseconds) {
  const date = new Date(milliseconds);
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
}

function idleSinceLabel(startedAt, now) {
  const date = new Date(startedAt);
  const today = new Date(now);
  const startDay = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const todayDay = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const days = Math.round((todayDay - startDay) / 86400000);
  if (days === 0) return `Idle since ${localClock(startedAt)}`;
  if (days === 1) return `Idle since yesterday ${localClock(startedAt)}`;
  const dateLabel = date.toLocaleDateString([], {
    month: 'short',
    day: 'numeric',
    ...(date.getFullYear() === today.getFullYear() ? {} : { year: 'numeric' }),
  });
  return `Idle since ${dateLabel} ${localClock(startedAt)}`;
}

function operationLabel(operation, now) {
  if (!operation) return '';
  const stages = [...(operation.stages || [])]
    .filter(stage => stage && stage.label)
    .sort((left, right) => (left.started_at_epoch_seconds || 0) - (right.started_at_epoch_seconds || 0));
  const oldestStage = stages[0] && epochSecondsMs(stages[0].started_at_epoch_seconds);
  const started = oldestStage ?? epochSecondsMs(operation.started_at_epoch_seconds);
  const clock = started == null ? '' : ` ${formatClock(now - started)}`;
  const labels = stages.map(stage => stage.label).join(' · ');
  const kind = {
    create: 'Starting',
    resume: 'Resuming',
    move: 'Moving',
    suspend: 'Suspending',
    destroy: 'Destroying',
    cleanup: 'Cleaning up',
    checkpoint: 'Checkpointing',
  }[operation.kind] || String(operation.kind || 'Operation').replace(/-/g, ' ');
  return `${labels || kind}${clock}`;
}

function sessionActivityLabel(session, now = serverClockMs()) {
  if (session.operation) return operationLabel(session.operation, now);
  if (pendingLifecycleActions.has(`destroy:${session.id}`)) return 'Destroying…';
  if (pendingLifecycleActions.has(`suspend:${session.id}`)) return 'Suspending…';
  if (session.launch_error) return session.launch_error;
  if (session.configuration_issue) return 'Needs configuration repair';
  if (session.has_error && isTransitioningSession(session)) return 'Needs recovery';
  if (['starting', 'suspending', 'failed'].includes(session.lifecycle)) {
    return sessionLifecycleLabel(session);
  }
  if (session.quota_recovery) {
    const deadline = session.quota_recovery.retry_at_ms;
    return deadline == null ? 'Quota limit · reset time unknown' : `Quota limit · resumes ${new Date(deadline).toLocaleString()}`;
  }
  if (session.capacity_retry) {
    const seconds = Math.max(0, Math.ceil((session.capacity_retry.retry_at_ms - now) / 1000));
    return `Provider unavailable · retrying in ${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, '0')}s`;
  }
  if (session.retry_assessment_pending) return 'Checking response';
  const details = session.activity_details || {};
  const kind = details.kind;
  const turnStarted = epochMs(details.turn_started_at_ms);
  const stepStarted = epochMs(details.step_started_at_ms);
  const backgroundStarted = epochMs(details.background_started_at_ms);
  const idleStarted = epochMs(details.idle_since_ms);
  // How long the harness has been quiet, while something is running. Shown as
  // a fact beside the other clocks: Mjolnir never ends a turn for silence, so
  // the reader decides whether a quiet turn is worth cancelling.
  const lastActivity = epochMs(details.last_activity_at_ms);
  const quietFor = lastActivity == null ? null : Math.max(0, now - lastActivity);
  const quiet = quietFor != null && quietFor >= SILENCE_WORTH_REPORTING_SECONDS
    ? ` · Quiet ${formatClock(quietFor)}`
    : '';
  if (kind === 'step') {
    return `Step${stepStarted == null ? '' : ` ${formatClock(now - stepStarted)}`}${quiet}`;
  }
  if (kind === 'turn') {
    const turn = turnStarted == null ? null : formatClock(now - turnStarted);
    const stepElapsed = stepStarted == null ? null : Math.max(0, now - stepStarted);
    const stepBaseElapsed = stepStarted == null && turnStarted != null
      ? Math.max(0, now - turnStarted)
      : stepElapsed;
    const clampedStep = stepBaseElapsed == null || turnStarted == null
      ? stepBaseElapsed
      : Math.min(stepBaseElapsed, Math.max(0, now - turnStarted));
    const step = clampedStep == null ? null : formatClock(clampedStep);
    return `Turn${turn ? ` ${turn}` : ''} · Step${step ? ` ${step}` : ''}${quiet}`;
  }
  if (kind === 'background') {
    return `${details.label || 'Background'}${backgroundStarted == null ? '' : ` ${formatClock(now - backgroundStarted)}`}`;
  }
  if (kind === 'idle') return idleStarted == null ? 'Idle' : idleSinceLabel(idleStarted, now);
  if (kind === 'lifecycle') return details.label || sessionLifecycleLabel(session);
  if (kind) return details.label || kind;
  if (session.activity) return session.activity;
  if (session.is_idle && idleStarted != null) return idleSinceLabel(idleStarted, now);
  return sessionLifecycleLabel(session);
}

function updateSessionActivity(card, session) {
  card._activity.textContent = sessionActivityLabel(session);
  card._activity.title = card._activity.textContent;
}

function updateSessionClocks() {
  for (const card of sessionCards.values()) {
    if (card.isConnected === false || !card._session) continue;
    // This is intentionally the only per-tick mutation: card identity and
    // all controls stay put while a clock advances.
    card._activity.textContent = sessionActivityLabel(card._session);
  }
  const selected = snapshot?.sessions.find(session => session.id === currentSession);
  if (isTransitioningSession(selected)) {
    conversationTransitionStage.textContent = sessionActivityLabel(selected);
  }
}

function closeSessionMenu(restoreFocus = true) {
  if (!openSessionMenuId) return;
  const card = sessionCards.get(openSessionMenuId);
  const trigger = openSessionMenuTrigger || card?._menuTrigger;
  if (card?._menu) {
    card._menu.classList?.add('hidden');
    card._menuTrigger?.setAttribute('aria-expanded', 'false');
  }
  openSessionMenuId = null;
  openSessionMenuTrigger = null;
  if (restoreFocus && trigger?.isConnected !== false) trigger.focus?.({ preventScroll: true });
}

function openSessionMenu(sessionId, trigger, toggle = false) {
  const card = sessionCards.get(sessionId) || trigger?.closest?.('.session');
  if (!card || !card._menu || !card._menu.children.length) return false;
  if (openSessionMenuId === sessionId) {
    if (toggle) {
      closeSessionMenu();
      return false;
    }
    return true;
  }
  closeSessionMenu(false);
  card._menu.classList.remove('hidden');
  card._menuTrigger.setAttribute('aria-expanded', 'true');
  openSessionMenuId = sessionId;
  openSessionMenuTrigger = trigger || card._menuTrigger;
  card._menu.querySelector('button:not(:disabled)')?.focus({ preventScroll: true });
  return true;
}

function sessionCardFromTarget(target) {
  return target?.closest?.('.session[data-session-id]');
}

function cancelSessionPress() {
  if (!activeSessionPress) return;
  clearTimeout(activeSessionPress.timer);
  activeSessionPress = null;
}

function beginSessionPress(event) {
  // A completed long press may not produce the synthetic click on every
  // touch browser. A new pointer gesture is unambiguously a fresh action.
  suppressedSessionClickId = null;
  if (event.isPrimary === false) {
    cancelSessionPress();
    return;
  }
  if (event.button !== undefined && event.button !== 0) return;
  if (event.target?.closest?.('button, a, input, select, textarea')) return;
  const card = sessionCardFromTarget(event.target);
  if (!card || !card._session || !sessionMenuActions(card._session).length) return;
  if (activeSessionPress && activeSessionPress.pointerId !== event.pointerId) {
    cancelSessionPress();
    return;
  }
  cancelSessionPress();
  activeSessionPress = {
    id: card.dataset.sessionId,
    pointerId: event.pointerId,
    x: event.clientX,
    y: event.clientY,
    timer: setTimeout(() => {
      const current = sessionCards.get(card.dataset.sessionId);
      if (
        !activeSessionPress ||
        activeSessionPress.id !== card.dataset.sessionId ||
        current !== card ||
        card.isConnected === false ||
        !snapshot?.sessions.some(session => session.id === card.dataset.sessionId && session.workspace_id === selectedWorkspaceId()) ||
        !sessionMenuActions(card._session).length
      ) {
        cancelSessionPress();
        return;
      }
      suppressedSessionClickId = card.dataset.sessionId;
      activeSessionPress = null;
      openSessionMenu(card.dataset.sessionId, card._menuTrigger);
    }, 500),
  };
}

function moveSessionPress(event) {
  if (!activeSessionPress) return;
  if (activeSessionPress.pointerId !== event.pointerId) {
    cancelSessionPress();
    return;
  }
  const dx = event.clientX - activeSessionPress.x;
  const dy = event.clientY - activeSessionPress.y;
  if (Math.hypot(dx, dy) > 10) cancelSessionPress();
}

// Durable "running" means the session is alive, not that a turn or background
// command is running. Leave activity to the separate turn/BG/idle indicator.
function sessionLifecycleLabel(session) {
  const labels = {
    live: 'Live',
    starting: 'Starting',
    suspending: 'Suspending',
    suspended: 'Suspended',
    failed: 'Failed',
  };
  if (session.lifecycle && labels[session.lifecycle]) return labels[session.lifecycle];
  return session.state === 'running' ? 'Live' : session.state || 'Unknown';
}

function action(label, className, data) {
  const node = button(label, className, data);
  node.disabled = pendingActions.has(`${data.action}:${data.id}`);
  return node;
}

/// Find a session card for an event without treating one of its controls as
/// a request to open the conversation. Card summaries remain ordinary text:
/// selecting text is not a separate interaction the card needs to preserve.
function sessionCardFromEvent(event) {
  const target = event.target;
  if (!target || target.closest('button')) return null;
  const card = target.closest('.session[data-session-id]');
  return card?.dataset.openable === 'true' ? card : null;
}

function openSessionCard(event) {
  const card = sessionCardFromEvent(event);
  if (!card) return false;
  if (event.type && event.type !== 'click') suppressedSessionClickId = null;
  if (suppressedSessionClickId === card.dataset.sessionId) {
    suppressedSessionClickId = null;
    return false;
  }
  closeSessionMenu(false);
  navigate({
    name: 'conversation',
    sessionId: card.dataset.sessionId,
    subagentParentId: typeof route === 'undefined' ? undefined : route.subagentParentId,
  });
  return true;
}

function handleSessionCardKeydown(event) {
  const trigger = event.target?.closest?.('button[data-session-menu]');
  if (trigger && ((event.key === 'F10' && event.shiftKey) || event.key === 'ContextMenu')) {
    event.preventDefault();
    openSessionMenu(trigger.dataset.sessionMenu, trigger);
    return;
  }
  if ((event.key === 'F10' && event.shiftKey) || event.key === 'ContextMenu') {
    const card = sessionCardFromTarget(event.target);
    if (!card || !card._session || !sessionMenuActions(card._session).length) return;
    event.preventDefault();
    openSessionMenu(card.dataset.sessionId, card._menuTrigger);
    return;
  }
  if (event.key !== 'Enter' && event.key !== ' ') return;
  if (!openSessionCard(event)) return;
  event.preventDefault();
}

function handleSessionMenuKeydown(event) {
  const menu = event.target?.closest?.('.session-menu');
  if (!menu) return;
  const controls = [...menu.querySelectorAll('button:not(:disabled)')];
  if (event.key === 'Escape') {
    event.preventDefault();
    closeSessionMenu();
    return;
  }
  if (event.key === 'Tab') {
    // Let the browser's normal tab order continue from the menu item. The
    // trigger is restored only for Escape and pointer dismissal.
    closeSessionMenu(false);
    return;
  }
  if (!controls.length) return;
  const index = controls.indexOf(event.target);
  let next = null;
  if (event.key === 'ArrowDown') next = controls[(index + 1) % controls.length];
  if (event.key === 'ArrowUp') next = controls[(index - 1 + controls.length) % controls.length];
  if (event.key === 'Home') next = controls[0];
  if (event.key === 'End') next = controls.at(-1);
  if (next) {
    event.preventDefault();
    next.focus({ preventScroll: true });
  }
}

// ---------------------------------------------------------------------------
// The other pages
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The New wizard
// ---------------------------------------------------------------------------
//
// One decision per screen, in the order the terminal asks them, ending in a
// review that names every choice before anything is committed. A phone keyboard
// covering a modal is how the previous single flat form became unusable, so
// this is a route rather than a dialog.

/// The steps, in order. Every isolated creation shows the resolved network
/// clone plan on the review step; raw local targets still validate their
/// project directory before that review.
const NEW_STEPS = [
  { key: 'profile', title: 'Profile', applies: () => true },
  { key: 'target', title: 'Target', applies: () => true },
  { key: 'project', title: 'Project', applies: () => true },
  { key: 'review', title: 'Review', applies: () => true },
];

let newDraft = null;
let pendingNewPreflight = null;
let pendingNewPreflightController = null;
let renderedNewDraft = null;
let renderedNewSignature = null;

function abortPendingNewPreflight() {
  if (newDraft) newDraft.preflighted = false;
  pendingNewPreflightController?.abort();
  pendingNewPreflightController = null;
  pendingNewPreflight = null;
}

function freshDraft() {
  return {
    workspaceId: selectedWorkspaceId(),
    step: 0,
    profileId: snapshot?.profiles[0]?.id || '',
    targetId: snapshot?.targets[0]?.id || '',
    bundleId: snapshot?.bundles[0]?.id || '',
    projectDirectory: '',
    title: '',
    remoteRepositories: [],
    localChangesExcluded: false,
    preflightError: '',
    preflighted: false,
    worktreeOptions: null,
    createManagedWorktree: false,
    mjolnirSubagents: snapshot?.subagents_enabled === true,
    worktreeSelection: null,
    bundleSource: '',
    creatingBundle: false,
    showBundleSource: false,
    projectDirectories: {},
  };
}

/// Only Claude and Codex receive Mjolnir's delegation tools, so only they are
/// offered the choice. Every other harness sends no opinion and keeps
/// following the global `[subagents] enabled` setting.
function subagentChoiceApplies() {
  const kind = snapshot?.profiles.find(profile => profile.id === newDraft?.profileId)?.harness_kind;
  return kind === 'claude' || kind === 'codex';
}

function targetIsBare(targetId) {
  return (
    snapshot?.targets.find(target => target.id === targetId)?.requires_project_directory === true
  );
}

function visibleSteps() {
  return NEW_STEPS.filter(step => step.applies(newDraft));
}

/// The title the daemon would derive, shown on review so the person sees the
/// name before committing rather than discovering it afterwards.
function derivedTitle() {
  const project = targetIsBare(newDraft.targetId)
    ? newDraft.projectDirectory.replace(/\/+$/, '').split('/').pop() || newDraft.projectDirectory
    : newDraft.bundleId;
  return `${project} via ${newDraft.profileId}`;
}

function renderNewForm() {
  if (!newDraft || newDraft.workspaceId !== selectedWorkspaceId()) {
    if (newDraft && newDraft.workspaceId !== selectedWorkspaceId()) {
      abortPendingNewPreflight();
    }
    newDraft = freshDraft();
    newError.textContent = '';
  }
  const steps = visibleSteps();
  newDraft.step = Math.min(newDraft.step, steps.length - 1);
  const step = steps[newDraft.step];
  // A snapshot often only changes another session. Keep the actual controls
  // mounted so it cannot interrupt a touch gesture or dismiss a native picker.
  const signature = JSON.stringify({
    step: step.key,
    profiles: step.key === 'profile' ? snapshot.profiles.map(p => [p.id, p.harness_kind]) : null,
    targets: step.key === 'target' ? snapshot.targets.map(t => [t.id, t.kind]) : null,
    project: step.key === 'project' ? [newDraft.targetId, snapshot.bundles, snapshot.targets.find(t => t.id === newDraft.targetId)?.recent_project_directories, newDraft.showBundleSource] : null,
    remote: step.key === 'review' ? [newDraft.preflighted, newDraft.remoteRepositories, newDraft.localChangesExcluded, newDraft.preflightError, newDraft.worktreeOptions, newDraft.createManagedWorktree, newDraft.mjolnirSubagents, subagentChoiceApplies()] : null,
    checking: pendingNewPreflight === newDraft,
    committing: Boolean(newDraft.committing),
    creating: newDraft.creatingBundle,
  });
  if (renderedNewDraft === newDraft && renderedNewSignature === signature) return;
  const focused = newStep.contains(document.activeElement) ? document.activeElement : null;
  const caret = focused?.id && focused.type === 'text'
    ? { id: focused.id, start: focused.selectionStart, end: focused.selectionEnd }
    : null;
  renderedNewDraft = newDraft;
  renderedNewSignature = signature;
  newProgress.textContent = `Step ${newDraft.step + 1} of ${steps.length} · ${step.title}`;
  newBackButton.disabled = newDraft.step === 0;
  newNextButton.textContent = step.key === 'review' ? (newDraft.preflightError ? 'Retry' : 'Start') : 'Next';

  const body = document.createDocumentFragment();
  switch (step.key) {
    case 'profile': {
      body.append(
        pickerField('Profile', 'new-profile', snapshot.profiles, newDraft.profileId, value => {
          newDraft.profileId = value;
        }),
      );
      break;
    }
    case 'target': {
      body.append(
        pickerField('Target', 'new-target', snapshot.targets, newDraft.targetId, value => {
          newDraft.projectDirectories[newDraft.targetId] = newDraft.projectDirectory;
          newDraft.targetId = value;
          newDraft.projectDirectory = newDraft.projectDirectories[value] ?? snapshot.targets.find(t => t.id === value)?.recent_project_directories?.[0] ?? '';
          // Changing the target changes which project question is asked, and
          // invalidates anything the previous project answer was checked for.
          newDraft.preflighted = false;
          newDraft.remoteRepositories = [];
          newDraft.localChangesExcluded = false;
          newDraft.preflightError = '';
        }),
      );
      break;
    }
    case 'project': {
      if (targetIsBare(newDraft.targetId)) {
        body.append(el('p', 'dim', 'Choose an existing directory. Review whether to create a managed worktree before launching.'));
        const recents = snapshot.targets.find(t => t.id === newDraft.targetId)?.recent_project_directories || [];
        if (!newDraft.projectDirectory && !Object.hasOwn(newDraft.projectDirectories, newDraft.targetId)) {
          newDraft.projectDirectory = recents[0] || '';
        }
        if (recents.length) {
          const recentList = el('div', 'recent-projects');
          recentList.append(el('p', 'dim', 'Recent projects on this host'));
          for (const directory of recents) {
            const pick = el('button', 'secondary recent-project', directory);
            pick.type = 'button';
            pick.onclick = () => {
              newDraft.projectDirectory = directory;
              newDraft.projectDirectories[newDraft.targetId] = directory;
              newDraft.preflighted = false;
              document.querySelector('#new-project-directory').value = directory;
            };
            recentList.append(pick);
          }
          body.append(recentList);
        }
        body.append(
          pathField(
            'Project directory',
            'new-project-directory',
            newDraft.projectDirectory,
            value => {
              newDraft.projectDirectory = value;
              newDraft.projectDirectories[newDraft.targetId] = value;
              newDraft.preflighted = false;
            },
            {
              host: () => newDraft.targetId,
              kind: 'directories',
              applies: () => true,
            },
          ),
        );
      } else {
        body.append(
          pickerField('Bundle', 'new-bundle', snapshot.bundles, newDraft.bundleId, value => {
            newDraft.bundleId = value;
            newDraft.preflighted = false;
            newDraft.remoteRepositories = [];
            newDraft.localChangesExcluded = false;
            newDraft.preflightError = '';
          }),
        );
        const create = el('button', 'secondary', 'Create bundle');
        create.type = 'button';
        create.onclick = () => {
          newDraft.showBundleSource = !newDraft.showBundleSource;
          renderNewForm();
          document.querySelector('#new-bundle-source')?.focus();
        };
        body.append(create);
        if (newDraft.showBundleSource || !snapshot.bundles.length) {
          body.append(
            pathField(
              'Repository source',
              'new-bundle-source',
              newDraft.bundleSource,
              value => {
                newDraft.bundleSource = value;
              },
              // A bundle source may be a URL or an `owner/repo` shorthand, and
              // a path here belongs to the controller's own machine.
              { host: () => null, kind: 'directories', applies: looksLikePath },
            ),
          );
          body.append(el('p', 'dim', 'Use a GitHub owner/repository or URL, or an existing repository path with a network remote. The isolated session starts from the remote default branch and excludes local unpublished changes.'));
          const save = el('button', '', newDraft.creatingBundle ? 'Creating bundle…' : 'Save bundle');
          save.type = 'button';
          save.onclick = createNewBundle;
          body.append(save);
        }
      }
      body.append(
        textField('Title (optional)', 'new-title', newDraft.title, value => {
          newDraft.title = value;
        }),
      );
      break;
    }
    default: {
      const review = el('dl', 'review');
      const rows = [
        ['Profile', newDraft.profileId],
        ['Target', newDraft.targetId],
        targetIsBare(newDraft.targetId)
          ? ['Project directory', newDraft.projectDirectory]
          : ['Bundle', newDraft.bundleId],
        ['Name', newDraft.title.trim() || derivedTitle()],
      ];
      if (newDraft.localChangesExcluded) {
        rows.push(['Local changes', 'Excluded (unpublished commits and dirty files)']);
        for (const repository of newDraft.remoteRepositories) {
          rows.push([
            `${repository.id} network source`,
            `fetch ${repository.fetch_url} · ${repository.default_branch} · push ${repository.push_urls?.join(', ') || 'none'}`,
          ]);
        }
      }
      if (pendingNewPreflight === newDraft) rows.push(['Project preflight', 'Checking project…']);
      if (newDraft.preflightError) rows.push(['Project preflight', newDraft.preflightError]);
      for (const [term, value] of rows) {
        review.append(el('dt', '', term), el('dd', '', value));
      }
      body.append(review);
      const worktree = el('label', 'field-inline');
      const checkbox = el('input');
      checkbox.type = 'checkbox';
      checkbox.id = 'new-managed-worktree';
      const available = targetIsBare(newDraft.targetId) && newDraft.worktreeOptions?.available === true;
      checkbox.checked = available && newDraft.createManagedWorktree;
      checkbox.disabled = !available;
      checkbox.onchange = () => {
        newDraft.createManagedWorktree = checkbox.checked;
        renderNewForm();
      };
      worktree.append(checkbox, document.createTextNode('Create managed worktree'));
      body.append(worktree, el('p', 'dim', !targetIsBare(newDraft.targetId)
        ? 'The target provides its own isolated workspace.'
        : checkbox.checked ? 'Create a separate session-owned checkout from the selected checkout’s HEAD.'
          : 'Use the selected directory directly.'));
      if (subagentChoiceApplies()) {
        const subagents = el('label', 'field-inline');
        const subagentBox = el('input');
        subagentBox.type = 'checkbox';
        subagentBox.id = 'new-mjolnir-subagents';
        subagentBox.checked = newDraft.mjolnirSubagents === true;
        subagentBox.onchange = () => {
          newDraft.mjolnirSubagents = subagentBox.checked;
          renderNewForm();
        };
        subagents.append(subagentBox, document.createTextNode('Use Mjolnir sub-agents'));
        body.append(subagents, el('p', 'dim', subagentBox.checked
          ? 'Delegation goes to Mjolnir sub-agents that share this session’s files.'
          : 'Unchecked keeps the harness’s own Agent or spawn_agent tools.'));
      }

    }
  }
  newStep.replaceChildren(body);
  const checking = pendingNewPreflight === newDraft;
  if (checking) {
    newNextButton.textContent = 'Checking…';
  }
  const busy = newDraft.committing === true || newDraft.creatingBundle;
  newNextButton.disabled = busy || checking || (step.key === 'review' && !newDraft.preflighted && !newDraft.preflightError);
  newBackButton.disabled ||= busy;
  for (const input of newStep.querySelectorAll('input, select, button')) input.disabled = input.disabled || busy;
  if (focused?.type === 'checkbox' && !busy) document.getElementById(focused.id)?.focus({ preventScroll: true });
  if (caret && !busy) {
    const input = document.getElementById(caret.id);
    input?.focus({ preventScroll: true });
    input?.setSelectionRange(caret.start, caret.end);
  }
}

function pickerField(label, id, items, value, onChange) {
  const field = choiceControl({
    label,
    options: items.map(item => ({ value: item.id, title: item.label ?? item.id, description: item.kind || item.harness_kind })),
    values: [value],
    onChange: () => onChange(field.querySelector('input:checked')?.value || ''),
  });
  field.id = id;
  if (!items.length) field.append(el('p', 'dim', `No ${label.toLowerCase()}s configured.`));
  return field;
}

async function createNewBundle() {
  const draft = newDraft;
  if (!draft || draft.creatingBundle) return;
  const source = draft.bundleSource.trim();
  if (!source) {
    newError.textContent = 'Enter a repository source for the bundle.';
    return;
  }
  draft.creatingBundle = true;
  newError.textContent = '';
  renderNewForm();
  try {
    const result = await request('/api/bundles', { method: 'POST', body: JSON.stringify({ source }) });
    if (newDraft !== draft) return;
    draft.bundleId = result.bundle_id;
    draft.showBundleSource = false;
    draft.bundleSource = '';
    draft.preflighted = false;
    draft.remoteRepositories = [];
    draft.localChangesExcluded = false;
    draft.preflightError = '';
    await refresh();
  } catch (error) {
    if (newDraft === draft) newError.textContent = error.message;
  } finally {
    draft.creatingBundle = false;
    if (newDraft === draft) renderNewForm();
  }
}

function textField(label, id, value, onInput) {
  const field = el('label', 'field');
  field.append(el('span', '', label));
  const input = el('input');
  input.id = id;
  input.value = value;
  input.oninput = () => onInput(input.value);
  field.append(input);
  return field;
}

/// How long a path field waits after the last keystroke before it asks.
/// Short enough to feel live, long enough that typing a path costs one
/// request per pause rather than one per character.
const PATH_SUGGESTION_DELAY_MS = 250;

/// Whether text asks to be read as a filesystem path rather than a URL or an
/// `owner/repo` shorthand. The controller applies the same predicate.
const looksLikePath = text => /^[\/~.]/.test(text) || /^[A-Za-z]:[\\/]/.test(text);

/// Live suggestions beneath one path field.
///
/// The list only ever suggests. It never rewrites text that is being typed,
/// which is why the controller's shared-prefix `insert` is ignored here. A
/// reply is shown only if it still answers what the field holds: a request
/// that was superseded, a value that has changed, or a field that has lost
/// focus all drop the answer instead of pushing it under the person.
function attachPathSuggestions(input, complete) {
  const list = el('div', 'field-suggestions hidden');
  list.setAttribute('role', 'listbox');
  input.after(list);
  const state = { timer: null, controller: null, matches: [], selected: 0, rows: [], list };

  const hide = () => {
    state.matches = [];
    state.rows = [];
    state.selected = 0;
    list.replaceChildren();
    list.classList.add('hidden');
  };

  const accept = index => {
    const candidate = state.matches[index];
    if (candidate === undefined) return false;
    hide();
    input.value = candidate;
    // A directory candidate ends in a separator, so re-dispatching the edit
    // both updates the draft and asks for that directory's children.
    input.dispatchEvent(new Event('input', { bubbles: true }));
    return true;
  };

  const updateSelection = () => {
    state.rows.forEach((row, index) =>
      row.setAttribute('aria-selected', String(index === state.selected)),
    );
  };

  const render = truncated => {
    state.rows = state.matches.map((candidate, index) => {
      const row = el('button', 'palette-row');
      row.type = 'button';
      row.setAttribute('role', 'option');
      row.setAttribute('aria-selected', String(index === state.selected));
      row.dataset.insert = candidate;
      row.append(el('span', 'palette-name', candidate));
      // Pressing a row must not blur the field first, or the list would be
      // gone before the click arrived.
      row.addEventListener('mousedown', event => event.preventDefault());
      row.addEventListener('click', () => accept(index));
      return row;
    });
    const rows = [...state.rows];
    if (truncated) rows.push(el('div', 'palette-row dim', 'More matches \u2014 keep typing'));
    list.replaceChildren(...rows);
    list.classList.remove('hidden');
  };

  const fetchSuggestions = async () => {
    const prefix = input.value;
    const controller = new AbortController();
    state.controller = controller;
    let answer;
    try {
      answer = await request('/api/paths/complete', {
        method: 'POST',
        signal: controller.signal,
        body: JSON.stringify({ target_id: complete.host(), prefix, kind: complete.kind }),
      });
    } catch {
      // An abort, a lost connection, and a host that cannot list the
      // directory all mean the same thing to someone typing: no suggestions.
      if (state.controller === controller) hide();
      return;
    }
    if (controller.signal.aborted || input.value !== prefix || document.activeElement !== input) return;
    state.matches = (answer && answer.candidates) || [];
    state.selected = 0;
    if (!state.matches.length) {
      hide();
      return;
    }
    render(Boolean(answer.truncated));
  };

  input.addEventListener('input', () => {
    clearTimeout(state.timer);
    if (state.controller) state.controller.abort();
    state.controller = null;
    hide();
    const value = input.value;
    if (!value || !complete.applies(value)) return;
    state.timer = setTimeout(fetchSuggestions, PATH_SUGGESTION_DELAY_MS);
  });

  input.addEventListener('keydown', event => {
    if (!state.matches.length) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      state.selected = (state.selected + delta + state.matches.length) % state.matches.length;
      updateSelection();
    } else if (event.key === 'Enter') {
      event.preventDefault();
      accept(state.selected);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      hide();
    }
  });

  input.addEventListener('blur', hide);
}
function pathField(label, id, value, onInput, complete = null) {
  const field = textField(label, id, value, onInput);
  const input = field.querySelector('input');
  input.spellcheck = false;
  input.autocapitalize = 'none';
  input.setAttribute('aria-description', '~ expands on apply using the home directory on the selected machine.');
  if (complete) attachPathSuggestions(input, complete);
  return field;
}

/// Ask the daemon whether this combination would launch, and what to warn
/// about, before the person commits to it.
async function preflightNew() {
  const draft = newDraft;
  if (!draft || pendingNewPreflight === draft) return false;
  draft.preflighted = false;
  draft.preflightError = '';
  draft.remoteRepositories = [];
  draft.localChangesExcluded = false;
  draft.worktreeOptions = null;
  const bare = targetIsBare(draft.targetId);
  const controller = new AbortController();
  pendingNewPreflight = draft;
  pendingNewPreflightController = controller;
  renderNewForm();
  try {
    let remoteRepairs = [];
    let answer;
    while (true) {
      answer = await request('/api/preflight/new', {
        method: 'POST',
        signal: controller.signal,
        body: JSON.stringify({
          workspace_id: selectedWorkspaceId(),
          profile_id: draft.profileId,
          bundle_id: draft.bundleId,
          target_id: draft.targetId,
          project_directory: bare ? draft.projectDirectory : null,
          remote_repairs: remoteRepairs,
        }),
      });
      if (controller.signal.aborted || newDraft !== draft || pendingNewPreflightController !== controller) return false;
      remoteRepairs = answer.remote_repairs || [];
      if (!remoteRepairs.length) break;
      const details = remoteRepairs.map(repair =>
        `${repair.path}: branch ${repair.branch} tracks missing remote ${repair.missing_remote}.\nSet its tracking remote to ${repair.replacement_remote}.\nFetch: ${repair.fetch_url}\nPush: ${repair.push_urls.join(', ')}`
      ).join('\n\n');
      if (!confirm(`Repair Git tracking and continue?\n\n${details}`)) {
        draft.preflightError = 'Git tracking repair was declined. Retry to review the repair, or go Back to choose another project.';
        return false;
      }
      if (controller.signal.aborted || newDraft !== draft || pendingNewPreflightController !== controller) return false;
    }
    if (bare && answer.project_directory) {
      draft.projectDirectory = answer.project_directory;
      draft.projectDirectories[draft.targetId] = answer.project_directory;
    }
    const selection = JSON.stringify([draft.targetId, draft.projectDirectory]);
    draft.worktreeOptions = answer.managed_worktree || { available: false, default_create: false };
    if (draft.worktreeSelection !== selection) {
      draft.createManagedWorktree = draft.worktreeOptions.default_create;
      draft.worktreeSelection = selection;
    }
    if (!bare || !draft.worktreeOptions.available) draft.createManagedWorktree = false;
    draft.remoteRepositories = answer.remote_repositories || [];
    draft.localChangesExcluded = answer.local_changes_excluded === true;
    draft.preflightError = '';
    draft.preflighted = true;
    return true;
  } catch (error) {
    if (controller.signal.aborted || error?.name === 'AbortError') return false;
    if (newDraft !== draft || pendingNewPreflightController !== controller) return false;
    draft.preflightError = error.message || 'Could not check this project. Retry or go Back to change it.';
    return false;
  } finally {
    if (pendingNewPreflightController === controller) {
      pendingNewPreflight = null;
      pendingNewPreflightController = null;
    }
    if (newDraft === draft) renderNewForm();
  }
}

async function advanceNew() {
  if (!newDraft || newDraft.creatingBundle || newDraft.committing || pendingNewPreflight === newDraft) return;
  const steps = visibleSteps();
  const step = steps[newDraft.step];
  newError.textContent = '';
  if (step.key === 'profile' && !snapshot.profiles.some(p => p.id === newDraft.profileId)) {
    newError.textContent = 'Choose an available profile before continuing.';
    return;
  }
  if (step.key === 'target' && !snapshot.targets.some(t => t.id === newDraft.targetId)) {
    newError.textContent = 'Choose an available target before continuing.';
    return;
  }

  if (step.key === 'project') {
    if (!targetIsBare(newDraft.targetId) && !snapshot.bundles.some(b => b.id === newDraft.bundleId)) {
      newError.textContent = 'Choose or create a bundle before continuing.';
      return;
    }
    if (targetIsBare(newDraft.targetId) && !newDraft.projectDirectory.trim()) {
      newError.textContent = 'Name the project directory to open.';
      return;
    }
    newDraft.preflighted = false;
    newDraft.step = Math.min(newDraft.step + 1, visibleSteps().length - 1);
    renderNewForm();
    await preflightNew();
    return;
  }
  if (step.key !== 'review') {
    newDraft.step += 1;
    renderNewForm();
    return;
  }
  if (!newDraft.preflighted) {
    await preflightNew();
    return;
  }
  await commitNew();
}

async function commitNew() {
  const draft = newDraft;
  if (!draft || draft.committing || !draft.preflighted || pendingNewPreflight === draft || draft.preflightError) return;
  const bare = targetIsBare(newDraft.targetId);
  const body = {
    action: 'new',
    workspace_id: draft.workspaceId,
    profile_id: newDraft.profileId,
    bundle_id: newDraft.bundleId,
    target_id: newDraft.targetId,
    project_directory: bare ? newDraft.projectDirectory : null,
    create_managed_worktree: bare && draft.worktreeOptions?.available === true && draft.createManagedWorktree,
    mjolnir_subagents: subagentChoiceApplies() ? draft.mjolnirSubagents === true : null,
  };
  if (newDraft.title.trim()) body.title = newDraft.title.trim();
  draft.committing = true;
  renderNewForm();
  try {
    await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    if (newDraft !== draft) return;
    await refresh();
    if (newDraft !== draft) return;
    navigate({ name: 'dashboard', workspaceId: draft.workspaceId });
  } catch (err) {
    if (newDraft === draft) newError.textContent = err.message;
  } finally {
    draft.committing = false;
    if (newDraft === draft) renderNewForm();
  }
}

/// Resume is a workspace-scoped list. Retained move recoveries remain
/// discoverable even when the normal resume capability is temporarily false.
function isResumeSession(session) {
  const recovery = session?.move_recovery;
  const retainedMove = recovery?.checkpoint_retained
    && ['failed', 'cancelled'].includes(recovery.phase);
  return Boolean(session?.capabilities?.resume || retainedMove);
}

function resumeActivityMs(session) {
  return epochMs(session.updated_at)
    ?? epochMs(session.last_activity_at_ms)
    ?? epochMs(session.created_at)
    ?? 0;
}

function resumeSessions(workspaceId = selectedWorkspaceId()) {
  return (snapshot?.sessions || [])
    .filter(session =>
      session.workspace_id === workspaceId
      && !isSubagentSession(session)
      && isResumeSession(session))
    .sort((left, right) =>
      resumeActivityMs(right) - resumeActivityMs(left) || left.id.localeCompare(right.id));
}

const resumeDrafts = new Map();
const resumeListStates = new Map();
const resumeRows = new Map();
const resumeCards = new Map();

function resumeListState(workspaceId) {
  let state = resumeListStates.get(workspaceId);
  if (!state) {
    state = { query: '', scrollTop: 0, focusSessionId: null };
    resumeListStates.set(workspaceId, state);
  }
  return state;
}

function resumeDraft(session) {
  const key = `${session.workspace_id}\u001f${session.id}`;
  let draft = resumeDrafts.get(key);
  if (!draft) {
    const recovery = session.move_recovery;
    draft = {
      profileId: recovery?.source_profile_id || session.profile_id || '',
      targetId: recovery?.source_target_template_id || session.target_id || '',
      queue: 'start',
      initialized: false,
      error: '',
    };
    resumeDrafts.set(key, draft);
  }
  return draft;
}

function resumeRecencyLabel(session) {
  return recencyLabel(resumeActivityMs(session));
}

function recencyLabel(millis) {
  if (!millis) return 'Unknown';
  const age = Math.max(0, serverClockMs() - millis);
  if (age < 60_000) return 'Just now';
  if (age < 3_600_000) return `${Math.floor(age / 60_000)}m ago`;
  if (age < 86_400_000) return `${Math.floor(age / 3_600_000)}h ago`;
  if (age < 604_800_000) return `${Math.floor(age / 86_400_000)}d ago`;
  return new Date(millis).toLocaleDateString([], {
    month: 'short', day: 'numeric',
    ...(new Date(millis).getFullYear() !== new Date(serverClockMs()).getFullYear() ? { year: 'numeric' } : {}),
  });
}

function resumeRow(session) {
  const row = button('', 'resume-session-row session', { resumeSession: session.id });
  row.type = 'button';
  row.dataset.sessionId = session.id;
  row.append(
    el('span', 'resume-session-title', session.title || session.id),
    el('span', 'resume-session-meta', [
      session.display_location || session.project_label || session.target_id || '',
      session.profile_id || '',
    ].filter(Boolean).join(' · ')),
    el('span', 'resume-session-recent', resumeRecencyLabel(session)),
    el('span', 'resume-session-snippet hidden'),
  );
  row.setAttribute('aria-label', `Resume ${session.title || session.id}`);
  row.onclick = () => {
    const state = resumeListState(session.workspace_id);
    state.focusSessionId = session.id;
    navigate({ name: 'resume', workspaceId: session.workspace_id, sessionId: session.id });
  };
  return row;
}

function renderResumable() {
  const workspaceId = route.workspaceId || selectedWorkspaceId();
  const state = resumeListState(workspaceId);
  const listMode = !route.sessionId && !route.wikiId;
  resumeListView?.classList.toggle('hidden', !listMode);
  resumeDetailView?.classList.toggle('hidden', listMode);
  if (!listMode) {
    renderResumeDetail();
    return;
  }
  // The index answers on its own schedule; the list is drawn from what it has
  // already said and redrawn when a later answer arrives.
  startWikiSearchIfIdle(state.query);
  if (resumeSearch && document.activeElement !== resumeSearch) resumeSearch.value = state.query;
  renderWikiSearchBox();
  const query = state.query.trim();
  // One search path: with a query, the list is the sessions the index
  // returned, in the order it ranked them. Nothing is matched here.
  const ranks = wikiSessionRanks();
  const list = query
    ? resumeSessions(workspaceId)
      .filter(session => ranks.has(session.id))
      .sort((left, right) => ranks.get(left.id) - ranks.get(right.id))
    : resumeSessions(workspaceId);
  for (const id of resumeRows.keys()) {
    if (!list.some(session => session.id === id)) resumeRows.delete(id);
  }
  if (!list.length) {
    resumable.replaceChildren(el('p', 'dim', query ? 'No matching sessions.' : 'No sessions to resume.'));
  } else {
    const rows = list.map(session => {
      let row = resumeRows.get(session.id);
      if (!row) {
        row = resumeRow(session);
        resumeRows.set(session.id, row);
      }
      row.querySelector('.resume-session-title').textContent = session.title || session.id;
      row.setAttribute('aria-label', `Resume ${session.title || session.id}`);
      row.querySelector('.resume-session-meta').textContent = [
        session.display_location || session.project_label || session.target_id || '',
        session.profile_id || '',
      ].filter(Boolean).join(' · ');
      row.querySelector('.resume-session-recent').textContent = resumeRecencyLabel(session);
      // A wiki hit for a session Mjolnir still has belongs on that session's
      // own row rather than in the archived list.
      const snippet = row.querySelector('.resume-session-snippet');
      const matched = wikiSnippetFor(session.id);
      snippet.classList.toggle('hidden', !matched);
      snippet.replaceChildren(...(matched ? wikiSnippetNodes(matched) : []));
      return row;
    });
    reconcileChildren(resumable, rows);
  }
  renderWikiSections();
}

function resumeChoiceField(label, id, items, value, onChange) {
  const field = el('label', 'field resume-choice');
  field.id = id;
  field.append(el('span', '', label));
  if (items.length === 1 && items[0].id === value) {
    field.append(el('span', 'field-value', items[0].label ?? items[0].id));
    return field;
  }
  if (!items.length) {
    field.append(el('span', 'field-value dim', `No ${label.toLowerCase()}s configured.`));
    return field;
  }
  const select = document.createElement('select');
  select.setAttribute('aria-label', label);
  const empty = el('option', '', `Choose ${label.toLowerCase()}`);
  empty.value = '';
  empty.disabled = true;
  empty.selected = !value || !items.some(item => String(item.id) === String(value));
  select.append(empty);
  for (const item of items) {
    const option = el('option', '', item.label ?? item.id);
    option.value = item.id;
    option.selected = String(item.id) === String(value);
    select.append(option);
  }
  select.onchange = () => onChange(select.value);
  field.append(select);
  return field;
}

// ---------------------------------------------------------------------------
// Archived sessions, from the SessionWiki index
// ---------------------------------------------------------------------------
//
// Mjolnir's own copy of an old session eventually goes away; SessionWiki keeps
// the transcript. Those rows are searched over HTTP, never on the render path:
// typing schedules a request, the answer updates state, and the state is drawn
// the next time the page renders. A request whose answer arrives after a later
// one started is dropped, so the list always shows the newest query's result.

const WIKI_SEARCH_DEBOUNCE_MS = 250;
/// How long the page waits before asking again while the first build runs.
const WIKI_INDEXING_POLL_MS = 5000;
/// How long it waits before repeating the query while a sync is running.
const WIKI_TOP_UP_POLL_MS = 2000;
/// How many times one query is repeated for a running sync.
const WIKI_TOP_UP_LIMIT = 10;
const WIKI_SEARCH_LIMIT = 50;
const WIKI_BRIEF_CHARS = 24000;

const wikiState = {
  /// The text the next scheduled request will carry.
  query: '',
  /// Incremented per request; only the current one may write `rows`.
  requestId: 0,
  timer: null,
  started: false,
  searching: false,
  rows: [],
  error: '',
  /// What the last answer said about the index: `ready`, `indexing` while the
  /// first build runs, or `version_mismatch`. Nothing has answered yet, so the
  /// page starts closed rather than offering a search it cannot run. A daemon
  /// that answers without a status is treated as ready.
  status: { state: 'indexing', topping_up: false },
  /// How many times the current query has been repeated for a running sync.
  topUps: 0,
  /// True once the daemon has said there is nothing to ask: this daemon has no
  /// wiki routes at all. Asking again in this page load would only repeat the
  /// same answer.
  unavailable: false,
  /// The one line the page shows about that.
  notice: '',
  /// Briefings and restore choices, kept per archived session id.
  briefs: new Map(),
  drafts: new Map(),
};

const wikiRowNodes = new Map();

/// Ask for the current query after a pause. Each keystroke replaces the
/// pending timer, so one request follows a burst of typing. The repeats a
/// still-building or still-syncing index asks for use the same timer with
/// their own longer waits.
function scheduleWikiSearch(query, delayMs = WIKI_SEARCH_DEBOUNCE_MS) {
  // A new query gets its own repeat budget; the old one's is spent.
  if (query !== wikiState.query) wikiState.topUps = 0;
  wikiState.query = query;
  wikiState.started = true;
  if (wikiState.unavailable) return;
  if (wikiState.timer !== null) clearTimeout(wikiState.timer);
  wikiState.timer = setTimeout(() => {
    wikiState.timer = null;
    // The timer's return value is nothing to the browser; handing the request
    // back is what lets a test wait for the round trip it started.
    return runWikiSearch();
  }, delayMs);
}

/// Schedule the first search of a visit. Rendering calls this; it only ever
/// sets a timer, so no fetch happens while the page is being drawn.
function startWikiSearchIfIdle(query) {
  if (wikiState.started || wikiState.unavailable) return;
  // This first ask has no typing to wait out, and its answer is what opens the
  // search box, so it goes as soon as the page yields.
  scheduleWikiSearch(query || '', 0);
}

/// Ask again when the last answer was not the final one.
///
/// While the first build runs the whole answer will change, so the page polls
/// every five seconds until it is ready, which is also what re-enables the
/// search box. While a sync is topping the index up the same query may gain
/// rows, so it is repeated every two seconds, at most ten times.
function scheduleWikiRefresh() {
  if (wikiState.unavailable) return;
  const { state, topping_up: toppingUp } = wikiState.status;
  if (state === 'version_mismatch') return;
  if (state === 'indexing') {
    scheduleWikiSearch(wikiState.query, WIKI_INDEXING_POLL_MS);
    return;
  }
  if (toppingUp && wikiState.topUps < WIKI_TOP_UP_LIMIT) {
    wikiState.topUps += 1;
    scheduleWikiSearch(wikiState.query, WIKI_TOP_UP_POLL_MS);
  }
}

async function runWikiSearch() {
  if (wikiState.unavailable) return;
  wikiState.requestId += 1;
  const requestId = wikiState.requestId;
  const query = wikiState.query.trim();
  wikiState.searching = true;
  try {
    const body = await request(
      `/api/v1/wiki/search?q=${encodeURIComponent(query)}&limit=${WIKI_SEARCH_LIMIT}`);
    if (requestId !== wikiState.requestId) return;
    wikiState.rows = body?.rows || [];
    wikiState.status = body?.status || { state: 'ready', topping_up: false };
    wikiState.error = '';
    scheduleWikiRefresh();
  } catch (err) {
    if (requestId !== wikiState.requestId) return;
    if (err.status === 404) {
      // A daemon without these routes. There is nothing to search and nothing
      // to report; the page simply has no archived sessions and no search.
      wikiState.unavailable = true;
      wikiState.rows = [];
      wikiState.error = '';
      wikiState.notice = '';
    } else {
      wikiState.error = err.message;
    }
  } finally {
    if (requestId === wikiState.requestId) {
      wikiState.searching = false;
      if (route.name === 'resume') renderResumable();
    }
  }
}

/// What stands in the search box while it cannot be typed into, or null when
/// the index can answer.
function wikiSearchPlaceholder() {
  if (wikiState.unavailable) return 'Search is unavailable';
  if (wikiState.status.state === 'indexing') return 'Indexing…';
  if (wikiState.status.state === 'version_mismatch') {
    return 'SessionWiki index is at a different version';
  }
  return null;
}

const WIKI_SEARCH_PLACEHOLDER = 'Title, project, or anything said';

/// Search is the index's answer, so the box is closed until the index can
/// answer. The list and the rows keep working meanwhile.
function renderWikiSearchBox() {
  if (!resumeSearch) return;
  const placeholder = wikiSearchPlaceholder();
  resumeSearch.disabled = Boolean(placeholder);
  resumeSearch.placeholder = placeholder || WIKI_SEARCH_PLACEHOLDER;
}

/// The rows the Archived section lists: the tool's own copy is gone and this
/// daemon has no live session for them.
function wikiArchivedRows() {
  return wikiState.rows.filter(row => row.archived === true && !row.hel_session_id);
}

/// The matching text for a session Mjolnir still holds, if the search found
/// one. Those hits belong on the live row, not in the archived list.
function wikiSnippetFor(sessionId) {
  const hit = wikiState.rows.find(row => row.hel_session_id === sessionId && row.snippet);
  return hit ? hit.snippet : '';
}

/// Where each live session the index returned sat in its answer, so a query
/// can list them in the order the index ranked them.
function wikiSessionRanks() {
  const ranks = new Map();
  wikiState.rows.forEach((row, rank) => {
    if (row.hel_session_id && !ranks.has(row.hel_session_id)) ranks.set(row.hel_session_id, rank);
  });
  return ranks;
}

/// SessionWiki marks the matched words with U+0002 and U+0003.
function wikiSnippetNodes(text) {
  const nodes = [];
  let rest = String(text);
  while (rest.length) {
    const open = rest.indexOf('');
    if (open === -1) {
      nodes.push(el('span', '', rest));
      break;
    }
    if (open > 0) nodes.push(el('span', '', rest.slice(0, open)));
    const close = rest.indexOf('', open + 1);
    if (close === -1) {
      nodes.push(el('mark', '', rest.slice(open + 1)));
      break;
    }
    nodes.push(el('mark', '', rest.slice(open + 1, close)));
    rest = rest.slice(close + 1);
  }
  return nodes;
}

function wikiRowNode(row) {
  let node = wikiRowNodes.get(row.id);
  if (!node) {
    node = button('', 'resume-session-row session archived-session-row', { wikiId: row.id });
    node.type = 'button';
    node.append(
      el('span', 'resume-session-title'),
      el('span', 'resume-session-meta'),
      el('span', 'resume-session-recent'),
      el('span', 'resume-session-snippet hidden'),
    );
    node.onclick = () => navigate({
      name: 'resume',
      workspaceId: route.workspaceId || selectedWorkspaceId(),
      wikiId: row.id,
    });
    wikiRowNodes.set(row.id, node);
  }
  const title = row.title || row.id;
  node.querySelector('.resume-session-title').textContent = title;
  node.setAttribute('aria-label', `Restore ${title}`);
  // The profile and target come from the index's own tags, so only a Mjolnir
  // row has them. They say where the session actually ran, which the tool name
  // alone cannot.
  node.querySelector('.resume-session-meta').textContent = [
    row.profile,
    row.target,
    row.tool,
    row.project,
    `${row.msgs} message${row.msgs === 1 ? '' : 's'}`,
  ].filter(Boolean).join(' · ');
  node.querySelector('.resume-session-recent').textContent = recencyLabel(epochMs(row.started));
  const snippet = node.querySelector('.resume-session-snippet');
  const text = row.snippet || row.preview || '';
  snippet.classList.toggle('hidden', !text);
  snippet.replaceChildren(...(text ? wikiSnippetNodes(text) : []));
  return node;
}

function renderWikiSections() {
  if (!resumeArchived || !resumeArchivedRows || !resumeWikiNote) return;
  const note = wikiState.unavailable ? wikiState.notice : wikiState.error;
  resumeWikiNote.textContent = note;
  resumeWikiNote.classList.toggle('hidden', !note);
  const rows = wikiState.unavailable ? [] : wikiArchivedRows();
  resumeArchived.classList.toggle('hidden', rows.length === 0);
  for (const id of [...wikiRowNodes.keys()]) {
    if (!rows.some(row => row.id === id)) wikiRowNodes.delete(id);
  }
  reconcileChildren(resumeArchivedRows, rows.map(wikiRowNode));
}

/// The restore form's state for one archived session.
///
/// It starts on the profile and target the index says the session ran under,
/// when the configuration still has them, so a restore repeats where it was
/// rather than asking from scratch. Failing that it falls back to the only
/// choice there is, or to nothing.
function wikiDraft(wikiId) {
  let draft = wikiState.drafts.get(wikiId);
  if (!draft) {
    const row = wikiState.rows.find(item => item.id === wikiId);
    const held = (list, id) => (list || []).some(item => item.id === id);
    draft = {
      profileId: held(snapshot?.profiles, row?.profile)
        ? row.profile
        : (snapshot?.profiles?.length === 1 ? snapshot.profiles[0].id : ''),
      targetId: held(snapshot?.targets, row?.target)
        ? row.target
        : (snapshot?.targets?.length === 1 ? snapshot.targets[0].id : ''),
      error: '',
    };
    wikiState.drafts.set(wikiId, draft);
  }
  return draft;
}

/// Fetch a briefing once per session id and redraw when it lands. Called from
/// the render, but the request itself runs in the background.
function ensureWikiBrief(wikiId) {
  let brief = wikiState.briefs.get(wikiId);
  if (brief) return brief;
  brief = { status: 'loading', markdown: '', error: '' };
  wikiState.briefs.set(wikiId, brief);
  request(`/api/v1/wiki/sessions/${encodeURIComponent(wikiId)}/brief?max_chars=${WIKI_BRIEF_CHARS}`)
    .then(body => {
      brief.status = 'ready';
      brief.markdown = body?.markdown || '';
    })
    .catch(err => {
      brief.status = 'error';
      brief.error = err.message;
    })
    .finally(() => {
      if (route.name === 'resume' && route.wikiId === wikiId) renderResumeDetail();
    });
  return brief;
}

function renderWikiDetail(wikiId) {
  const row = wikiState.rows.find(item => item.id === wikiId);
  const draft = wikiDraft(wikiId);
  const brief = ensureWikiBrief(wikiId);
  const focused = document.activeElement;
  const previousFocus = resumeDetail?.contains(focused)
    ? focused.closest?.('[data-role]')?.dataset?.role
    : null;
  const card = el('article', 'card resume-card wiki-card');
  card.dataset.wikiId = wikiId;
  card.append(el('h3', '', row?.title || wikiId));
  card.append(el('p', 'dim', [
    row?.profile,
    row?.target,
    row?.tool,
    row?.project,
    row ? `${row.msgs} message${row.msgs === 1 ? '' : 's'}` : '',
    row?.started ? recencyLabel(epochMs(row.started)) : '',
  ].filter(Boolean).join(' · ')));
  card.append(el('p', '', 'This session is archived. Restoring starts a new session that opens with a summary of the old one.'));
  const briefBox = el('div', 'wiki-brief');
  if (brief.status === 'loading') briefBox.append(el('p', 'dim', 'Loading the transcript…'));
  else if (brief.status === 'error') briefBox.append(el('p', 'resume-status error', brief.error));
  else briefBox.append(renderMarkdown(brief.markdown));
  card.append(briefBox);
  const profileItems = (snapshot?.profiles || []).map(profile => ({ id: profile.id, label: profile.id }));
  const targetItems = (snapshot?.targets || []).map(target => ({
    id: target.id,
    label: target.label || target.name || target.id,
  }));
  if (!profileItems.some(item => item.id === draft.profileId)) draft.profileId = profileItems.length === 1 ? profileItems[0].id : '';
  if (!targetItems.some(item => item.id === draft.targetId)) draft.targetId = targetItems.length === 1 ? targetItems[0].id : '';
  const profileField = resumeChoiceField('Profile', `wiki-profile-${wikiId}`, profileItems, draft.profileId, value => {
    draft.profileId = value;
    renderResumeDetail();
  });
  profileField.dataset.role = 'wiki-profile';
  const targetField = resumeChoiceField('Target', `wiki-target-${wikiId}`, targetItems, draft.targetId, value => {
    draft.targetId = value;
    renderResumeDetail();
  });
  targetField.dataset.role = 'wiki-target';
  card.append(profileField, targetField);
  const submit = action('Restore', '', { action: 'wiki-restore', id: wikiId });
  submit.disabled = submit.disabled || !draft.profileId || !draft.targetId;
  const row2 = el('div', 'row');
  row2.append(submit);
  card.append(row2);
  const pending = el('p', 'dim', pendingActions.has(`wiki-restore:${wikiId}`) ? 'Restoring…' : '');
  pending.setAttribute('role', 'status');
  card.append(pending);
  const errorNode = el('p', 'error', draft.error);
  errorNode.setAttribute('role', 'alert');
  card.append(errorNode);
  resumeDetail?.replaceChildren(card);
  if (previousFocus) resumeDetail?.querySelector(`[data-role="${previousFocus}"] select`)?.focus({ preventScroll: true });
}

/// Start a new session from an archived one and follow it into its
/// conversation, the same way a resumed session is followed.
async function restoreWikiSession(wikiId) {
  const key = `wiki-restore:${wikiId}`;
  if (pendingActions.has(key)) return;
  const draft = wikiDraft(wikiId);
  if (!draft.profileId || !draft.targetId) return;
  draft.error = '';
  pendingActions.add(key);
  renderRoute();
  try {
    const body = await request(`/api/v1/wiki/sessions/${encodeURIComponent(wikiId)}/restore`, {
      method: 'POST',
      body: JSON.stringify({
        workspace_id: route.workspaceId || selectedWorkspaceId(),
        profile_id: draft.profileId,
        target_id: draft.targetId,
      }),
    });
    await refresh();
    if (body?.session_id) navigate({ name: 'conversation', sessionId: body.session_id });
  } catch (err) {
    draft.error = err.message;
  } finally {
    pendingActions.delete(key);
    renderRoute();
  }
}

// ---------------------------------------------------------------------------
// Moving a local checkout into an isolated workspace
// ---------------------------------------------------------------------------
//
// Both the move dialog and the resume card show the same preview of what a
// conversion does, so the wording lives in one place here. The controller
// renders the same sentences for the terminal.

function conversionBytes(bytes) {
  const kb = 1024;
  const mb = kb * 1024;
  const value = Number(bytes) || 0;
  return value < mb ? `${(value / kb).toFixed(1)} KB` : `${(value / mb).toFixed(1)} MB`;
}

function conversionSummary(preview) {
  const push = (preview.push_urls || []).length ? preview.push_urls.join(', ') : preview.fetch_url;
  return `Clone ${preview.fetch_url} (default branch ${preview.default_branch}) into ${preview.destination} on branch ${preview.branch || 'a detached head'}; push to ${push}.`;
}

function conversionWarnings(preview) {
  const lines = [];
  const staged = preview.staged_files || 0;
  const unstaged = preview.unstaged_files || 0;
  const untracked = preview.untracked_files || 0;
  const dirty = staged + unstaged + untracked;
  if (dirty > 0) {
    lines.push(`${staged} staged, ${unstaged} unstaged, and ${untracked} untracked ${dirty === 1 ? 'file' : 'files'} (${conversionBytes(preview.untracked_bytes)}) will be copied into the container. Ignored files such as build output, .env, and node_modules will not.`);
  }
  const commits = preview.unpushed_commits || 0;
  if (commits > 0) {
    let line = `${commits} ${commits === 1 ? 'commit' : 'commits'} not on ${preview.fetch_url} ${commits === 1 ? 'travels' : 'travel'} in the checkpoint.`;
    if (commits > 200) line += ' That is a large history; consider pushing first.';
    lines.push(line);
  }
  if (preview.host_checkout_retained) {
    lines.push(`${preview.checkout} stays on this machine and will no longer track this session. Edits made in the container do not come back automatically; push the branch or move the session back.`);
  }
  return lines;
}

/// Whether resuming this session on `targetId` moves its checkout off this
/// machine: the session runs in a directory (its current target needs one)
/// and the destination supplies its own isolated workspace. The controller
/// decides for real; this only decides whether to ask.
function conversionApplies(session, targetId) {
  if (!targetId) return false;
  const target = id => (snapshot.targets || []).find(item => item.id === id);
  return target(session.target_id)?.requires_project_directory === true
    && target(targetId)?.requires_project_directory === false;
}

function ensureResumeConversionPreflight(session) {
  const draft = resumeDraft(session);
  const targetId = draft.targetId;
  if (!conversionApplies(session, targetId)) {
    if (draft.conversion) draft.conversion = null;
    return;
  }
  if (draft.conversion && draft.conversion.targetId === targetId) return;
  draft.conversion = { targetId, status: 'checking', preview: null, detail: '', acknowledged: false };
  request('/api/preflight/resume', {
    method: 'POST',
    body: JSON.stringify({ session_id: session.id, target_id: targetId }),
  }).then(answer => {
    const current = resumeDraft(session).conversion;
    if (!current || current.targetId !== targetId) return;
    if (answer.kind === 'converting-raw-checkout') {
      current.status = 'converting';
      current.preview = answer.preview;
    } else if (answer.kind === 'unavailable') {
      current.status = 'error';
      current.detail = answer.detail;
    } else {
      current.status = 'ready';
    }
    renderResumeDetail();
  }).catch(error => {
    const current = resumeDraft(session).conversion;
    if (!current || current.targetId !== targetId) return;
    current.status = 'error';
    current.detail = error.message;
    renderResumeDetail();
  });
}

/// A conversion the person has not agreed to yet blocks Resume, and so does a
/// check that has not finished or could not be made.
function conversionBlocksResume(draft) {
  const conversion = draft.conversion;
  if (!conversion) return false;
  if (conversion.status === 'checking' || conversion.status === 'error') return true;
  return conversion.status === 'converting' && !conversion.acknowledged;
}

function resumeCardSignature(session) {
  const draft = resumeDraft(session);
  return JSON.stringify([
    session.compatible_resume_targets || [],
    draft.targetId,
    draft.conversion?.status || '',
    draft.conversion?.detail || '',
    draft.conversion?.preview || null,
    (snapshot.profiles || []).map(profile => [profile.id, profile.harness_kind]),
    session.move_recovery,
    session.profile_id,
    session.target_id,
    session.capabilities?.resume,
    session.capabilities?.destroy,
    session.launch_error,
    session.capabilities?.open,
    session.lifecycle,
    session.operation?.kind,
    session.state,
    session.has_error,
    session.configuration_issue,
    session.title,
    session.display_location,
    (session.queued_prompts || []).length,
  ]);
}

function resumeTargetItems(session) {
  return (session.compatible_resume_targets || []).map(id => {
    const target = (snapshot.targets || []).find(item => item.id === id);
    return { id, label: target?.label || target?.name || id };
  });
}

function resumeCard(session) {
  const card = el('article', 'card resume-card');
  card.dataset.sessionId = session.id;
  card._signature = '';
  card._session = session;
  updateResumeCard(card, session, true);
  return card;
}

function updateResumeCard(card, session, rebuild = false) {
  ensureResumeConversionPreflight(session);
  const focused = document.activeElement;
  const previousFocus = card.contains(focused) ? focused.closest?.('[data-role]')?.dataset?.role : null;
  const destroyButton = card.querySelector('button[data-action="destroy"]');
  if (destroyButton) destroyButton.disabled = pendingActions.has(`destroy:${session.id}`);
  const signature = resumeCardSignature(session);
  if (!rebuild && card._signature === signature) {
    card._session = session;
    const draft = resumeDraft(session);
    card._errorNode.textContent = draft.error;
    card._pendingNode.textContent = pendingActions.has(`destroy:${session.id}`) ? 'Destroying…' : pendingActions.has(`resume:${session.id}`) ? 'Requesting resume…' : '';
    card._invalid = !draft.profileId || !draft.targetId || conversionBlocksResume(draft);
    const submit = card.querySelector('button[data-action="resume"]');
    if (submit) {
      submit.dataset.profile = draft.profileId;
      submit.dataset.target = draft.targetId;
      submit.disabled = pendingActions.has(`resume:${session.id}`) || card._invalid === true;
      submit.setAttribute('aria-busy', String(pendingActions.has(`resume:${session.id}`)));
    }
    return;
  }
  card._signature = signature;
  card._session = session;
  const recovery = session.move_recovery;
  const draft = resumeDraft(session);
  const targetItems = resumeTargetItems(session);
  const profileItems = (snapshot.profiles || []).map(profile => ({ id: profile.id, label: profile.id }));
  if (!draft.initialized) {
    if (!profileItems.some(item => item.id === draft.profileId)) {
      draft.profileId = profileItems.length === 1 ? profileItems[0].id : '';
    }
    if (!targetItems.some(item => item.id === draft.targetId)) {
      draft.targetId = targetItems.length === 1 ? targetItems[0].id : '';
    }
    draft.initialized = true;
  } else {
    if (!profileItems.some(item => item.id === draft.profileId)) draft.profileId = '';
    if (!targetItems.some(item => item.id === draft.targetId)) draft.targetId = '';
  }
  const body = el('div');
  const heading = el('h3', '', session.title || session.id);
  body.append(heading, el('p', 'dim', `${sessionLifecycleLabel(session)} · ${session.display_location || session.target_id || 'Unknown target'}`));
  if (session.state === 'lost' && !recovery?.checkpoint_retained) body.append(el('p', 'resume-status error', 'The session is lost. No verified recovery checkpoint is available.'));
  else if (session.state === 'destroyed-with-data-loss') body.append(el('p', 'resume-status error', 'The session was destroyed with data loss. No session data remains to resume.'));
  else if (session.configuration_issue) body.append(el('p', 'resume-status error', session.configuration_issue));
  else if (session.has_error) body.append(el('p', 'resume-status error', 'The previous operation reported an error. Review the available choices before trying again.'));
  if (recovery) {
    const phase = recovery.phase === 'cancelled' ? 'cancelled' : recovery.phase === 'failed' ? 'failed' : 'interrupted';
    body.append(el('p', '', `Move was ${phase}.`));
    if (recovery.checkpoint_retained) body.append(el('p', 'dim', 'A verified recovery checkpoint is retained.'));
  }
  const queuePinned = recovery?.queue_admission_started && !recovery.queue_admission_finished;
  const moveRow = el('div', 'row');
  if (recovery?.checkpoint_retained && ['failed', 'cancelled'].includes(recovery.phase)) {
    const retry = button('Retry move', 'secondary', { action: 'move', id: session.id });
    moveRow.append(retry);
  }
  if (moveRow.children.length) {
    body.append(el('p', 'dim', queuePinned
      ? 'Queued work already began on the destination. Retry the move using its retained destination and queue choice.'
      : 'Retry the move with the retained destination, or resume with the source settings.'));
    body.append(moveRow);
  }
  const noRecovery = !recovery?.checkpoint_retained && ['lost', 'destroyed-with-data-loss'].includes(session.state);
  const canResume = session.capabilities?.resume === true && !queuePinned && !noRecovery;
  const stale = session.capabilities?.open === true || ['live', 'starting', 'suspending'].includes(session.lifecycle);
  card._invalid = false;
  if (stale) {
    body.append(el('p', 'dim', 'This session is active now and cannot be resumed.'));
    const open = button(session.capabilities?.open ? 'Open session' : 'View session status', 'secondary');
    open.onclick = () => navigate(session.capabilities?.open || isTransitioningSession(session)
      ? { name: 'conversation', sessionId: session.id }
      : { name: 'dashboard', workspaceId: session.workspace_id });
    body.append(open);
  } else if (!canResume) {
    if (queuePinned) body.append(el('p', 'dim', 'Resume is unavailable while queued work is pinned to the retained destination.'));
    else if (!recovery && !noRecovery) body.append(el('p', 'dim', 'This session cannot be resumed from the web viewer.'));
  } else if (!targetItems.length) {
    body.append(el('p', '', 'This session cannot resume on any target configured here. Finish recovery in the terminal.'));
  } else {
    const profileField = resumeChoiceField('Profile', `resume-profile-${session.id}`, profileItems, draft.profileId, value => {
      draft.profileId = value;
      renderResumeDetail();
    });
    profileField.dataset.role = 'resume-profile';
    const targetField = resumeChoiceField('Target', `resume-target-${session.id}`, targetItems, draft.targetId, value => {
      draft.targetId = value;
      renderResumeDetail();
    });
    targetField.dataset.role = 'resume-target';
    body.append(profileField, targetField);
    const queued = (session.queued_prompts || []).length;
    if (queued) {
      const queueField = resumeChoiceField(
        `${queued} queued prompt${queued === 1 ? '' : 's'}`,
        `resume-queue-${session.id}`,
        [{ id: 'start', label: 'Run them after resuming' }, { id: 'discard', label: 'Discard them' }],
        draft.queue,
        value => { draft.queue = value; },
      );
      queueField.dataset.role = 'resume-queue';
      body.append(queueField);
    }
    const conversion = draft.conversion;
    if (conversion?.status === 'checking') {
      body.append(el('p', 'dim', 'Checking checkout…'));
    } else if (conversion?.status === 'error') {
      body.append(el('p', 'resume-status error', conversion.detail || 'This checkout cannot move into that target.'));
    } else if (conversion?.status === 'converting' && conversion.preview) {
      body.append(el('p', '', conversionSummary(conversion.preview)));
      for (const warning of conversionWarnings(conversion.preview)) {
        body.append(el('p', 'move-warning', warning));
      }
      const acknowledge = el('label', 'move-warning');
      acknowledge.dataset.role = 'resume-conversion';
      const check = document.createElement('input');
      check.type = 'checkbox';
      check.checked = conversion.acknowledged === true;
      check.onchange = () => {
        conversion.acknowledged = check.checked;
        renderResumeDetail();
      };
      acknowledge.append(check, el('span', '', 'I understand; move this checkout into the container'));
      body.append(acknowledge);
    }
    const row = el('div', 'row');
    const submit = action('Resume', '', { action: 'resume', id: session.id, profile: draft.profileId, target: draft.targetId });
    submit.disabled = submit.disabled || !draft.profileId || !draft.targetId || conversionBlocksResume(draft);
    row.append(submit);
    body.append(row);
    card._invalid = !draft.profileId || !draft.targetId || conversionBlocksResume(draft);
  }
  const pendingNode = el('p', 'dim', pendingActions.has(`destroy:${session.id}`) ? 'Destroying…' : pendingActions.has(`resume:${session.id}`) ? 'Requesting resume…' : '');
  pendingNode.setAttribute('role', 'status');
  card._pendingNode = pendingNode;
  body.append(pendingNode);
  if (session.capabilities?.destroy) {
    const destroy = action('Destroy session…', 'danger', { action: 'destroy', id: session.id });
    body.append(destroy);
  }
  if (session.launch_error) body.append(el('p', 'error', session.launch_error));
  const errorNode = el('p', 'error', draft.error);
  errorNode.dataset.resumeError = 'true';
  errorNode.setAttribute('role', 'alert');
  card._errorNode = errorNode;
  body.append(errorNode);
  card.replaceChildren(body);
  if (previousFocus) card.querySelector(`[data-role="${previousFocus}"] select`)?.focus({ preventScroll: true });
}

function renderResumeDetail() {
  if (route.wikiId) {
    renderWikiDetail(route.wikiId);
    return;
  }
  const session = snapshot.sessions.find(item =>
    item.id === route.sessionId && item.workspace_id === route.workspaceId);
  if (!session) {
    resumeDetail?.replaceChildren(el('p', 'dim', 'This session is no longer available. Return to the session list.'));
    return;
  }
  let cached = resumeCards.get(session.id);
  if (!cached) {
    cached = resumeCard(session);
    resumeCards.set(session.id, cached);
  } else {
    updateResumeCard(cached, session);
  }
  if (resumeDetail?.firstChild !== cached) resumeDetail?.replaceChildren(cached);
}

// ---------------------------------------------------------------------------
// Move confirmation
// ---------------------------------------------------------------------------
//
// Moving is deliberately a two-step route. The first request is read-only and
// returns a fingerprinted preparation from the daemon; only the second request
// can interrupt the source. Keeping the preparation in this route also makes a
// browser reconnect harmless: it cannot accidentally submit a changed target
// under an old confirmation.

function freshMoveDraft(session) {
  const compatible = session.compatible_resume_targets || [];
  const recovery = session.move_recovery;
  const recoveryTarget = recovery?.destination_target_template_id;
  return {
    workspaceId: session.workspace_id || selectedWorkspaceId(),
    sessionId: session.id,
    profileId: recovery?.destination_profile_id || session.profile_id || snapshot.profiles[0]?.id || '',
    targetId: compatible.includes(recoveryTarget)
      ? recoveryTarget
      : compatible.includes(session.target_id) ? session.target_id : compatible[0] || '',
    clearResourceAllocation: recovery?.clear_resource_allocation === true,
    destinationAdditionalMounts: recovery ? (recovery.destination_additional_mounts || []) : null,
    destinationResourceAllocation: recovery ? (recovery.destination_resource_allocation ?? null) : null,
    queueLocked: recovery?.queue_admission_started === true && recovery?.queue_admission_finished !== true,
    preparation: null,
    preparing: false,
    committing: false,
    acknowledge: false,
    queue: recovery?.queue || 'discard',
  };
}

function moveQueueItemText(item) {
  if (item?.kind && typeof item.kind === 'object') {
    const [kind, details] = Object.entries(item.kind)[0] || [];
    if (kind === 'set_config') return `/${details?.key || 'config'} ${details?.value || ''}`.trim();
  }
  const content = Array.isArray(item?.content) ? item.content : [];
  const text = content.find(block => block?.type === 'text')?.text;
  const image = content.find(block => block?.type === 'image');
  if (typeof text === 'string' && text) {
    if (image) return `${text} [Image attachment: ${image.mimeType || image.mime_type || 'image'}]`;
    return text;
  }
  if (image) return `[Image attachment: ${image.mimeType || image.mime_type || 'image'}]`;
  try {
    return JSON.stringify(item?.content || item).slice(0, 400);
  } catch (_) {
    return 'Queued command';
  }
}

function renderMoveForm() {
  if (!moveStep || route.name !== 'move') return;
  const session = snapshot.sessions.find(item => item.id === route.sessionId);
  if (!session) return;
  const recoveryTarget = session.move_recovery?.destination_target_template_id;
  if (!moveDraft || moveDraft.sessionId !== session.id) moveDraft = freshMoveDraft(session);
  const draft = moveDraft;
  const preparation = draft.preparation;
  moveProgress.textContent = preparation ? 'Review the destination and confirm the interruption.' : 'Choose a compatible destination. The source is not changed during preparation.';
  moveStep.replaceChildren();

  if (!preparation) {
    moveStep.append(el('p', '', `Move “${session.title || session.id}” while keeping its session identity, transcript, and recoverable workspace state.`));
    const profilePicker = pickerField('Profile', `move-profile-${session.id}`, snapshot.profiles, draft.profileId, value => {
      if (draft.queueLocked) return;
      draft.profileId = value;
      draft.preparation = null;
    });
    moveStep.append(profilePicker);
    const targetIds = [...new Set([
      ...(session.compatible_resume_targets || []),
      recoveryTarget,
    ].filter(Boolean))];
    const targets = targetIds.map(id => snapshot.targets.find(target => target.id === id) || { id });
    const targetPicker = pickerField('Target', `move-target-${session.id}`, targets, draft.targetId, value => {
      if (draft.queueLocked) return;
      draft.targetId = value;
      draft.preparation = null;
    });
    if (draft.queueLocked) {
      for (const input of [...profilePicker.querySelectorAll('input'), ...targetPicker.querySelectorAll('input')]) {
        input.disabled = true;
      }
    }
    moveStep.append(targetPicker);
    moveStep.append(el('p', 'dim', 'Move keeps the existing environment when the target, attached directories, and resource sizing stay the same, and rebuilds a fresh environment otherwise. Existing resource sizing and attached directories are retained. When the environment is rebuilt, installed packages and files outside the declared workspace are not migrated.'));
    const clearResources = el('label', 'field-inline');
    const clearResourcesInput = document.createElement('input');
    clearResourcesInput.type = 'checkbox';
    clearResourcesInput.checked = draft.clearResourceAllocation;
    clearResourcesInput.disabled = draft.queueLocked;
    clearResourcesInput.onchange = () => {
      draft.clearResourceAllocation = clearResourcesInput.checked;
      draft.preparation = null;
    };
    clearResources.append(clearResourcesInput, el('span', '', 'Clear inherited resource sizing and use destination defaults'));
    moveStep.append(clearResources);
    if (draft.queueLocked) {
      moveStep.append(el('p', 'dim', 'The destination and resource settings are locked to the existing destination because queue admission already began.'));
    }
    moveStep.append(el('p', 'dim', 'Use this only when intentionally removing the current container or host sizing. Attached directories stay fixed to this workspace in the web viewer.'));
    moveNextButton.textContent = 'Prepare move';
  } else {
    const target = snapshot.targets.find(item => item.id === (preparation.selection.target_template_id || session.target_id));
    const profile = snapshot.profiles.find(item => item.id === (preparation.selection.profile_id || session.profile_id));
    moveStep.append(el('p', '', `From ${preparation.source_profile_id} / ${preparation.source_target_template_id} to ${profile?.id || preparation.selection.profile_id || session.profile_id} / ${target?.id || preparation.selection.target_template_id || session.target_id}.`));
    moveStep.append(el('p', 'dim', preparation.selection.clear_resource_allocation
      ? 'Resource sizing: use destination defaults. Attached directories remain fixed to this workspace.'
      : 'Resource sizing and attached directories: retain the source workspace settings.'));
    moveStep.append(el('p', 'dim', preparation.cross_harness ? 'This is a cross-harness handoff. Harness-private state is rebuilt from the canonical transcript.' : 'The same harness session state will be restored when supported.'));
    moveStep.append(el('p', 'dim', preparation.in_place
      ? 'Only the harness and profile are replaced; the environment and workspace are kept.'
      : 'The session is restored into a fresh environment.'));
    if (preparation.source_unavailable) {
      moveStep.append(el('p', 'move-warning', 'Source is unavailable; Move will recover its saved data without starting its old harness.'));
    }
    if (preparation.conversion) {
      moveStep.append(el('p', '', conversionSummary(preparation.conversion)));
      for (const warning of conversionWarnings(preparation.conversion)) {
        moveStep.append(el('p', 'move-warning', warning));
      }
    }
    if (preparation.active) {
      const warning = el('label', 'move-warning');
      const check = document.createElement('input');
      check.type = 'checkbox';
      check.checked = draft.acknowledge;
      check.onchange = () => {
        draft.acknowledge = check.checked;
        renderMoveForm();
      };
      warning.append(check, el('span', '', preparation.in_place
        ? 'Interrupt the active turn and checkpoint the session before replacing the harness.'
        : 'Interrupt the active turn and checkpoint the session before rebuilding it.'));
      moveStep.append(warning);
    }
    const queued = preparation.queued_commands || [];
    if (queued.length) {
      moveStep.append(el('h3', '', `${queued.length} queued command${queued.length === 1 ? '' : 's'}`));
      const list = el('div', 'move-queue');
      list.append(...queued.map((item, index) => el('div', 'queue-item', `${index + 1}. ${moveQueueItemText(item)}`)));
      moveStep.append(list);
      const queuePicker = choiceControl({
        label: 'After the destination is ready',
        options: draft.queueLocked
          ? [{
            value: draft.queue,
            title: draft.queue === 'start' ? 'Continue queued work on this destination' : 'Keep queued work discarded',
            description: 'The previous destination already admitted this choice; retrying with another choice could replay work.',
          }]
          : [
            { value: 'discard', title: 'Discard queued work', description: 'Start idle on the destination (default).' },
            { value: 'start', title: 'Run queued work', description: 'Accept the existing commands in order after readiness.' },
          ],
        values: [draft.queue],
        onChange: () => {
          draft.queue = queuePicker.querySelector('input:checked')?.value || 'discard';
        },
      });
      moveStep.append(queuePicker);
    } else {
      moveStep.append(el('p', 'dim', draft.queueLocked
        ? `Queue admission already began with “${draft.queue}”; retrying must use the same choice on this destination.`
        : 'No queued work is waiting; the destination will open idle.'));
    }
    moveNextButton.textContent = 'Confirm move';
  }
  const busy = draft.preparing || draft.committing;
  moveNextButton.disabled = busy || (preparation?.active === true && !draft.acknowledge);
  moveBackButton.disabled = busy;
  for (const input of moveStep.querySelectorAll('input, select, button')) input.disabled = busy;
}

async function prepareMove() {
  const draft = moveDraft;
  if (!draft || draft.preparing) return;
  if (!draft.profileId && !draft.targetId) {
    moveError.textContent = 'Choose a profile, a target, or both.';
    return;
  }
  draft.preparing = true;
  moveError.textContent = '';
  renderMoveForm();
  try {
    draft.preparation = await request('/api/moves/prepare', {
      method: 'POST',
      body: JSON.stringify({
        session_id: draft.sessionId,
        profile_id: draft.profileId || null,
        target_template_id: draft.targetId || null,
        clear_resource_allocation: draft.clearResourceAllocation,
        additional_mounts: draft.destinationAdditionalMounts,
        resource_allocation: draft.destinationResourceAllocation,
      }),
    });
  } catch (error) {
    moveError.textContent = error.message;
  } finally {
    draft.preparing = false;
    if (moveDraft === draft) renderMoveForm();
  }
}

async function commitMove() {
  const draft = moveDraft;
  if (!draft || !draft.preparation || draft.committing) return;
  draft.committing = true;
  moveError.textContent = '';
  renderMoveForm();
  try {
    await request('/api/actions', {
      method: 'POST',
      body: JSON.stringify({
        action: 'move',
        request: {
          preparation: draft.preparation,
          queue: (draft.preparation.queued_commands || []).length || draft.queueLocked ? draft.queue : null,
          acknowledge_interruption: draft.acknowledge,
        },
      }),
    });
    await refresh();
    announce(`Moving ${draft.sessionId}`);
    navigate({ name: 'dashboard', workspaceId: draft.workspaceId });
  } catch (error) {
    moveError.textContent = error.message;
  } finally {
    draft.committing = false;
    if (moveDraft === draft) renderMoveForm();
  }
}

async function advanceMove() {
  if (!moveDraft?.preparation) return prepareMove();
  return commitMove();
}

/// Bytes as a person reads them.
function formatBytes(bytes) {
  if (bytes === undefined || bytes === null) return null;
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = Number(bytes);
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/// The band a percentage falls in, matching the terminal's thresholds.
///
/// The terminal colours quota by headroom remaining and target load by the
/// inverse, so a busy machine and an exhausted limit both read red.
function band(percentRemaining) {
  if (percentRemaining === null || percentRemaining === undefined) return '';
  if (percentRemaining <= 20) return 'reading-low';
  if (percentRemaining <= 50) return 'reading-mid';
  return 'reading-high';
}

/// The freshness of one reading, as a word.
///
/// Four states, and each is said rather than implied: there has never been a
/// reading, one is being taken now, the last one is older than it should be,
/// or the last probe failed and the previous reading is what is on screen.
function freshness(reading) {
  if (reading.has_error) return { word: 'probe failed', className: 'reading-low' };
  if (reading.refreshing && reading.sampled_at_epoch_seconds === undefined) {
    return { word: 'loading', className: '' };
  }
  if (reading.refreshing) return { word: 'refreshing', className: '' };
  if (reading.stale) return { word: 'stale', className: 'reading-mid' };
  return null;
}

function renderTargets() {
  const readings = snapshot.capacity || [];
  if (!readings.length) {
    targetsPanel.replaceChildren(el('p', 'dim', 'No hosts or fleets are configured to be probed.'));
    return;
  }
  targetsPanel.replaceChildren(
    ...readings.map(reading => {
      const card = el('article', 'card');
      const heading = el('h3');
      heading.append(el('span', '', reading.label));
      const state = freshness(reading);
      if (state) heading.append(el('span', `pill ${state.className}`, state.word));
      card.append(heading);
      card.append(el('p', 'dim', reading.target_ids.join(', ')));

      const rows = [];
      if (reading.cpu_percent !== undefined) {
        // CPU is load, so its band is the inverse of the headroom bands.
        rows.push(['CPU', `${reading.cpu_percent}%`, band(100 - reading.cpu_percent)]);
      }
      if (reading.memory_total_bytes) {
        const used = reading.memory_used_bytes ?? 0;
        const percent = Math.min(100, Math.round((used / reading.memory_total_bytes) * 100));
        rows.push([
          'Memory',
          `${percent}% of ${formatBytes(reading.memory_total_bytes)}`,
          band(100 - percent),
        ]);
      }
      if (reading.logical_cores) rows.push(['Cores', String(reading.logical_cores), '']);
      if (reading.disk_total_bytes) {
        rows.push(['Disk', formatBytes(reading.disk_total_bytes), '']);
      }
      if (reading.virtual_machines !== undefined) {
        rows.push([
          'Machines',
          `${reading.virtual_machines} VM${reading.virtual_machines === 1 ? '' : 's'}`,
          '',
        ]);
      }
      if (!rows.length) {
        card.append(el('p', 'dim', 'No reading yet.'));
      } else {
        const list = el('dl', 'readings');
        for (const [term, value, className] of rows) {
          list.append(el('dt', '', term), el('dd', className, value));
        }
        card.append(list);
      }
      card.append(refreshRow('refresh-capacity', { target_id: reading.id }));
      return card;
    }),
  );
}

function renderQuota() {
  const focused = document.activeElement;
  const focusedProfile = focused?.closest('.quota-profile')?.dataset.profileId;
  const focusedControl = focused?.matches('summary') ? 'summary' : focused?.matches('button[data-refresh]') ? 'button[data-refresh]' : null;
  const expanded = new Set(
    [...quotaPanel.querySelectorAll('details[open]')].map(row => row.dataset.profileId),
  );
  const profiles = snapshot.profiles || [];
  if (!profiles.length) {
    quotaPanel.replaceChildren(el('p', 'dim', 'No profiles configured.'));
    return;
  }
  // Keep the provider's actual period labels. Missing windows are not zero,
  // and an unfamiliar provider must not disappear from the overview.
  const labels = [...new Set(profiles.flatMap(profile =>
    (profile.quota?.windows || []).map(window => window.label),
  ))].sort((a, b) => {
    // Match the TUI: weekly quota first, then the five-hour window.
    const rank = label => label === 'Week' ? 0 : label === '5H' ? 1 : 2;
    return rank(a) - rank(b) || a.localeCompare(b);
  });
  quotaPanel.style.setProperty('--quota-columns', Math.max(1, labels.length));
  const heading = el('div', 'quota-overview-heading');
  heading.append(el('span', '', '% left'));
  for (const label of labels) heading.append(el('span', '', label));
  const hint = el('p', 'dim quota-hint', 'Tap a profile for resets and details.');
  quotaPanel.replaceChildren(
    hint,
    heading,
    ...profiles.map(profile => {
      const disclosure = el('details', 'quota-profile');
      disclosure.dataset.profileId = profile.id;
      disclosure.open = expanded.has(profile.id);
      const summary = el('summary', 'quota-overview-row');
      const quota = profile.quota;
      const name = el('span', 'quota-profile-name', profile.id);
      if (quota?.has_error) name.append(el('small', 'reading-low', 'probe failed'));
      else if (quota?.stale) name.append(el('small', 'reading-mid', 'stale'));
      summary.append(name);
      const spoken = [profile.id, quota?.has_error ? 'probe failed; last reading' : quota?.stale ? 'stale reading' : ''];
      if (quota?.windows?.length) {
        for (const label of labels) {
          const window = quota.windows.find(window => window.label === label);
          const used = window?.percent_used;
          const remaining = used == null ? null : 100 - used;
          const warning = window?.projects_exhaustion_before_reset;
          const value = window ? remaining === null ? '?' : `${remaining}%` : '—';
          const cell = el('span', `quota-value ${band(remaining)}`, value + (warning ? ' !' : ''));
          const description = `${label}: ${window ? remaining === null ? 'unknown' : `${remaining}% left` : 'not reported'}${warning ? ', projected to run out before reset' : ''}`;
          cell.title = description;
          summary.append(cell);
          spoken.push(description);
        }
      } else {
        const state = el('span', 'quota-no-windows dim', quota?.has_error ? 'Unavailable' : quota?.summary || 'No reading yet');
        summary.append(state);
        spoken.push(state.textContent);
      }
      summary.append(el('span', 'quota-chevron', '›'));
      summary.lastChild.setAttribute('aria-hidden', 'true');
      summary.setAttribute('aria-label', spoken.filter(Boolean).join('. '));
      disclosure.append(summary);
      const card = el('div', 'quota-details');
      disclosure.append(card);
      card.append(el('p', 'dim', profile.harness_kind));
      const error = el('p', 'quota-error');
      error.setAttribute('role', 'alert');
      card.append(error);

      if (!quota) {
        card.append(el('p', 'dim', 'No reading yet.'));
        card.append(refreshRow('refresh-quota', { profile_id: profile.id }));
        return disclosure;
      }
      const windows = quota.windows || [];
      if (!windows.length) {
        card.append(el('p', 'dim', quota.summary || 'No windows reported.'));
      }
      for (const window of windows) {
        const row = el('div', 'quota-window');
        const label = el('div', 'quota-label');
        label.append(el('span', '', window.label));
        const used = window.percent_used;
        label.append(
          el(
            'span',
            band(used === undefined ? undefined : 100 - used),
            used === undefined ? 'unknown' : `${used}% used`,
          ),
        );
        row.append(label);
        if (used !== undefined) {
          // A bar and a number say the same thing, so a reader who cannot see
          // the bar has not lost anything.
          const meter = el('div', 'meter');
          meter.setAttribute('role', 'img');
          meter.setAttribute('aria-label', `${window.label}: ${used}% used`);
          const fill = el('div', `meter-fill ${band(100 - used)}`);
          fill.style.setProperty('--fill', `${used}%`);
          meter.append(fill);
          row.append(meter);
        }
        const notes = [];
        if (window.resets_at) notes.push(`resets ${window.resets_at}`);
        if (window.projects_exhaustion_before_reset) notes.push('on course to run out first');
        if (notes.length) row.append(el('p', 'dim', notes.join(' · ')));
        card.append(row);
      }
      if (quota.refreshed_at_epoch_seconds) {
        card.append(
          el(
            'p',
            'dim',
            `Last refreshed ${new Date(quota.refreshed_at_epoch_seconds * 1000).toLocaleTimeString()}`,
          ),
        );
      }
      card.append(refreshRow('refresh-quota', { profile_id: profile.id }));
      return disclosure;
    }),
  );
  if (focusedProfile && focusedControl) {
    [...quotaPanel.querySelectorAll('.quota-profile')]
      .find(row => row.dataset.profileId === focusedProfile)
      ?.querySelector(focusedControl)?.focus({ preventScroll: true });
  }
}

/// The refresh control both pages carry.
function refreshRow(actionName, payload) {
  const row = el('div', 'row');
  const control = button('Refresh', 'secondary', { refresh: actionName });
  control.dataset.payload = JSON.stringify(payload);
  row.append(control);
  return row;
}

async function runRefresh(target, errorNode) {
  const body = { action: target.dataset.refresh, ...JSON.parse(target.dataset.payload) };
  if (errorNode) errorNode.textContent = '';
  target.disabled = true;
  try {
    await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    await refresh();
  } catch (err) {
    if (errorNode) errorNode.textContent = err.message;
  } finally {
    target.disabled = false;
  }
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

function startEvents() {
  if (eventSource) eventSource.close();
  eventSource = new EventSource('/api/events');
  eventSource.addEventListener('open', () => setConnection('online'));
  eventSource.addEventListener('revision', () => {
    setConnection('online');
    refresh().then(ok => {
      if (!ok || !currentSession) return;
      const session = snapshot?.sessions.find(item => item.id === currentSession);
      if (session?.capabilities?.open && !isTransitioningSession(session)) {
        loadConversation(true);
      }
    });
  });
  // The browser reconnects a stream on its own; saying so is what stops the
  // page looking current while it is not.
  eventSource.addEventListener('error', () => {
    if (navigator.onLine) setConnection('reconnecting');
    else setConnection('offline');
  });
}

function showLogin() {
  cancelVoiceInput();
  snapshot = undefined;
  currentSession = null;
  if (eventSource) {
    eventSource.close();
    eventSource = undefined;
  }
  // Nothing from the previous viewer may survive a sign-out in this tab.
  pendingActions.clear();
  pendingLifecycleActions.clear();
  backgroundTaskErrors.clear();
  resumeRows.clear();
  resumeCards.clear();
  resumeDrafts.clear();
  resumeListStates.clear();
  pendingReviewSessions.clear();
  entryNodes.clear();
  elicitationCards.clear();
  sentElicitations.clear();
  clearPromptImages();
  login.classList.remove('hidden');
  app.classList.add('hidden');
  menuButton.classList.add('hidden');
  backButton.classList.add('hidden');
  closeMenu();
}

// Acceptance hands ownership to the daemon. Keep pending feedback until a
// snapshot observes the operation or its final result, including reconnects.
function reconcileLifecycleActions() {
  for (const [key, pending] of pendingLifecycleActions) {
    const session = snapshot.sessions.find(item => item.id === pending.id);
    const running = session?.operation || session?.lifecycle === 'suspending';
    const finished = !session || (pending.action === 'suspend' && session.lifecycle === 'suspended')
      || (session.launch_error && session.launch_error !== pending.previousError)
      || (pending.observed && !running);
    if (finished) {
      pendingLifecycleActions.delete(key);
      pendingActions.delete(key);
    } else if (running) pending.observed = true;
  }
}

function confirmSessionDestruction(session) {
  if (!session) return Promise.resolve(null);
  return new Promise(resolve => {
    const dialog = el('dialog', 'session-destroy-dialog');
    dialog.setAttribute('aria-label', 'Destroy session');
    dialog.append(el('h2', '', 'Destroy session?'));
    dialog.append(el('p', '', session.title || session.id));
    dialog.append(el('p', '', 'Permanently remove this session, its environment, and its recovery archive. Work held only in the environment will be lost. This also destroys its sub-agents.'));
    const label = el('label');
    const branch = el('input');
    branch.type = 'checkbox';
    branch.checked = false;
    label.append(branch, document.createTextNode(' Also delete the managed branch'));
    dialog.append(label, el('p', 'dim', 'Keeping the managed branch does not preserve work held only inside the environment.'));
    const controls = el('div', 'row');
    const cancel = button('Cancel', 'secondary');
    const destroy = button('Destroy session', 'danger');
    const finish = choice => { dialog.close(); dialog.remove(); resolve(choice); };
    cancel.onclick = () => finish(null);
    destroy.onclick = () => finish(branch.checked);
    dialog.oncancel = event => { event.preventDefault(); finish(null); };
    controls.append(cancel, destroy);
    dialog.append(controls);
    document.body.append(dialog);
    dialog.showModal();
    cancel.focus();
  });
}

async function refresh() {
  try {
    snapshot = await request('/api/snapshot');
    snapshotReceivedAtMs = Date.now();
    reconcileLifecycleActions();
    seedDashboardOrders(snapshot);
    renderMenuVersion();
    login.classList.add('hidden');
    app.classList.remove('hidden');
    menuButton.classList.remove('hidden');
    if (currentSession) {
      const session = snapshot.sessions.find(x => x.id === currentSession);
      if (!session
        || (!session.capabilities?.open
          && !isTransitioningSession(session)
          && !isLoadingConversationSession(session))) {
        navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
        return true;
      }
      syncConversationMode(session);
      renderQueue(session);
      renderElicitations(session);
      renderTurnReview(session);
      renderAttachments();
      renderConversationHeader(session);
    }
    renderRoute();
    if (!eventSource) startEvents();
    return true;
  } catch (e) {
    if (e.message === 'unauthorized') showLogin();
    return false;
  }
}

/// Load the snapshot first, then honour the URL.
///
/// A protected route must stay a login page while the snapshot request is
/// unauthorized: rendering it first would dereference a snapshot that is not
/// there.
async function restoreRoute() {
  if (!(await refresh())) return;
  applyRoute();
}

function renderQueue(session) {
  if (typeof reconcileSubmissions === 'function') {
    reconcileSubmissions(session.id, [...(session.queued_prompts || []), ...(session.active_user_shells || [])].map(item => item.id));
    renderPendingSubmissions();
  }
  const backgroundTaskContainer =
    typeof backgroundTasks === 'undefined' ? null : backgroundTasks;
  const pending = typeof pendingActions === 'undefined' ? new Set() : pendingActions;
  const taskErrors =
    typeof backgroundTaskErrors === 'undefined' ? new Map() : backgroundTaskErrors;
  const prompts = session.queued_prompts || [];
  queue.replaceChildren(
    ...prompts.map((prompt, index) => {
      const row = el('div', 'queue-item');
      row.append(el('span', '', `${index + 1}. ${prompt.text}`));
      const controls = el('div', 'row');
      // The newest queued prompt can be taken back into the composer, the
      // way the terminal's edit-latest does, because the last thing you
      // queued is the one you most often want to change.
      if (index === prompts.length - 1) {
        controls.append(button('Edit', 'secondary', { editQueueId: prompt.id }));
      }
      controls.append(button('Remove', 'danger', { queueId: prompt.id }));
      row.append(controls);
      return row;
    }),
  );
  queue.hidden = prompts.length === 0;
  if (queueHeading) queueHeading.hidden = prompts.length === 0;

  const running = session.active_user_shells || [];
  shells.replaceChildren(
    ...running.map(shell => {
      const row = el('div', 'queue-item');
      row.append(el('span', '', `$ ${shell.command}`));
      row.append(button('Cancel', 'danger', { shellId: shell.id }));
      return row;
    }),
  );
  shells.hidden = running.length === 0;
  if (shellsHeading) shellsHeading.hidden = running.length === 0;
  const tasks = session.background_tasks || [];
  const taskIds = new Set(tasks.map(task => task.id));
  const taskPrefix = `stop-background:${session.id}:`;
  for (const key of [...pending]) {
    if (key.startsWith(taskPrefix) && !taskIds.has(key.slice(taskPrefix.length))) {
      pending.delete(key);
    }
  }
  for (const key of [...taskErrors.keys()]) {
    if (key.startsWith(taskPrefix) && !taskIds.has(key.slice(taskPrefix.length))) {
      taskErrors.delete(key);
    }
  }
  const now = tasks.length ? serverClockMs() : 0;
  const taskRows = tasks.map(task => {
      const key = backgroundTaskKey(session.id, task.id);
      const row = el('div', 'queue-item background-task');
      const details = el('div', 'background-task-details');
      details.append(el('span', 'background-task-command', `$ ${task.command}`));
      const started = epochMs(task.started_at_ms);
      details.append(
        el('span', 'dim', started == null ? 'Running' : formatClock(now - started)),
      );
      row.append(details);
      if (task.can_stop) {
        const stop = button(
          pending.has(key) ? 'Stopping…' : 'Stop',
          'danger',
          { backgroundTaskId: task.id },
        );
        stop.disabled = pending.has(key);
        row.append(stop);
      } else {
        row.append(el('span', 'dim', 'Stop unavailable'));
      }
      const error = taskErrors.get(key);
      if (error) {
        const message = el('p', 'background-task-error', error);
        message.setAttribute('role', 'alert');
        row.append(message);
      }
      return row;
    });
  if (backgroundTaskContainer) {
    backgroundTaskContainer.replaceChildren(...taskRows);
    backgroundTaskContainer.hidden = tasks.length === 0;
  }
  if (typeof backgroundTasksHeading !== 'undefined' && backgroundTasksHeading) {
    backgroundTasksHeading.hidden = tasks.length === 0;
  }
  if (conversationSummary) {
    const labels = [];
    if (prompts.length) labels.push('queued prompts');
    if (running.length) labels.push('shell commands');
    if (tasks.length) labels.push('background tasks');
    conversationSummary.textContent = labels.length
      ? labels.map(label => label[0].toUpperCase() + label.slice(1)).join(', ')
      : 'Shell commands';
  }
  conversationSide.hidden = prompts.length === 0 && running.length === 0 && tasks.length === 0;
}

function backgroundTaskKey(sessionId, taskId) {
  return `stop-background:${sessionId}:${taskId}`;
}

/// Stop one projected task. Acceptance is not completion: the button remains
/// pending until a later snapshot removes the task, so a slow provider cannot
/// be stopped twice by a rerender or a fast double tap.
async function stopBackgroundTask(taskId) {
  const session = activeSession();
  const task = session?.background_tasks?.find(item => item.id === taskId);
  if (!session || !task?.can_stop) return false;
  const key = backgroundTaskKey(session.id, task.id);
  if (pendingActions.has(key)) return false;
  pendingActions.add(key);
  backgroundTaskErrors.delete(key);
  renderQueue(session);
  try {
    await request(
      `/api/sessions/${encodeURIComponent(session.id)}/background-tasks/stop`,
      {
        method: 'POST',
        body: JSON.stringify({ background_task_id: task.id }),
      },
    );
    await refresh();
    return true;
  } catch (error) {
    pendingActions.delete(key);
    const latest = snapshot?.sessions.find(item => item.id === session.id);
    if (latest?.background_tasks?.some(item => item.id === task.id)) {
      backgroundTaskErrors.set(key, error.message);
      if (currentSession === session.id) renderQueue(latest);
    }
    return false;
  }
}
// Every snapshot revision re-renders the conversation. Rebuilding a card the
// user is answering would wipe the half-filled form and steal focus, so each
// pending request keeps its live DOM until the request itself changes or
// leaves the snapshot.
/// The review last drawn, so the card is rebuilt only when it changes.
let reviewSignature = null;
const pendingReviewSessions = new Set();
const elicitationCards = new Map(),
  sentElicitations = new Set();
function elicitationKey(sessionId, id) {
  return `${sessionId}\u001f${id}`;
}

let choiceControlSequence = 0;

/// A native radio/checkbox group whose complete rows are touch targets.
///
/// The inputs remain ordinary browser controls, so arrow keys, Tab, and
/// assistive technology keep their platform semantics. The surrounding label
/// makes the title and description part of the same target as the control.
function choiceControl({
  label,
  options,
  multiple = false,
  values = [],
  required = false,
  onChange = () => {},
}) {
  const fieldset = el('fieldset', 'choice-control');
  fieldset.append(el('legend', '', label));
  const selected = new Set(
    (Array.isArray(values) ? values : [values])
      .filter(value => value != null)
      .map(value => String(value)),
  );
  // A name is needed for native radio keyboard behaviour. It must not be
  // shared by two independently-rendered groups on the same page.
  const name = `choice-${++choiceControlSequence}`;
  for (const option of options || []) {
    const row = el('label', 'choice-option');
    const input = el('input');
    input.type = multiple ? 'checkbox' : 'radio';
    input.name = name;
    input.value = String(option.value ?? '');
    input.checked = selected.has(input.value);
    // `required` on every checkbox would require every option. Multi-select
    // required/min/max rules are applied by the form's validator instead.
    input.required = Boolean(required && !multiple);

    const text = el('span', 'choice-option-text');
    text.append(el('span', 'choice-option-title', String(option.title ?? option.value ?? '')));
    if (option.description) {
      text.append(el('span', 'choice-option-description dim', String(option.description)));
    }
    row.append(input, text);
    fieldset.append(row);
    input.addEventListener('change', event => onChange(event));
  }
  return fieldset;
}

function elicitationOptionLabel(option) {
  return option.title ?? String(option.value ?? '');
}
function elicitationControl(field) {
  const input = document.createElement('input');
  input.type =
    field.kind === 'boolean'
      ? 'checkbox'
      : field.kind === 'integer' || field.kind === 'number'
        ? 'number'
        : field.secret
          ? 'password'
          : 'text';
  if (field.kind === 'integer') input.step = '1';
  if (field.kind === 'number') input.step = 'any';
  if (field.minimum != null) input.min = field.minimum;
  if (field.maximum != null) input.max = field.maximum;
  if (field.min_length != null) input.minLength = field.min_length;
  if (field.max_length != null) input.maxLength = field.max_length;
  if (field.pattern) input.pattern = field.pattern;
  if (field.kind === 'boolean') input.checked = field.default === true;
  else if (field.default != null) input.value = String(field.default);
  return input;
}
function elicitationFieldValue(field, control) {
  if (field.kind === 'multi_select') {
    const values = [...control.querySelectorAll('input:checked')].map(input => input.value);
    return values.length || field.required ? values : undefined;
  }
  if (field.kind === 'single_select') {
    const value = control.querySelector('input:checked')?.value || '';
    return value === '' && !field.required ? undefined : value;
  }
  if (field.kind === 'boolean') return control.checked;
  if (control.value === '')
    return field.required && (field.kind === 'text' || field.kind === 'single_select')
      ? ''
      : undefined;
  if (field.kind === 'integer') return Number.parseInt(control.value, 10);
  if (field.kind === 'number') return Number(control.value);
  return control.value;
}
// Builds the controls and returns collect(), which reads them back as ACP
// content. A custom answer replaces the choice group it belongs to unless the
// request pairs it with one specific option, which is how Mjolnir's chat form
// submits the same request.
function buildElicitationForm(form, request, register) {
  const entries = [],
    customByOwner = new Map();
  for (const field of request.fields || []) {
    const isChoice = field.kind === 'single_select' || field.kind === 'multi_select';
    // A fieldset contains its own option labels. Keeping its outer wrapper a
    // div avoids invalid nested labels and preserves one target per option.
    const wrapper = document.createElement(isChoice ? 'div' : 'label');
    wrapper.className = 'elicitation-field';
    let control,
      validateChoices = () => {};
    if (isChoice) {
      control = choiceControl({
        label: `${field.title}${field.required ? ' *' : ''}`,
        options: (field.kind === 'single_select' && !field.required
          ? [{ value: '', title: 'No answer' }, ...(field.options || [])]
          : field.options || []).map(option => ({
          value: option.value,
          title: elicitationOptionLabel(option),
          description: option.description,
        })),
        multiple: field.kind === 'multi_select',
        values:
          field.kind === 'multi_select'
            ? field.default || []
            : field.default == null
              ? field.required
                ? [field.options?.[0]?.value]
                : []
              : [field.default],
        required: Boolean(field.required),
        onChange: () => validateChoices(),
      });
      const validateTarget = control.querySelector('input');
      validateChoices = () => {
        if (field.kind !== 'multi_select' || !validateTarget) return;
        const custom = customByOwner.get(field.id);
        // An unpaired free-text answer replaces this choice field. Its value
        // must be able to satisfy a required group without a phantom native
        // selection, while a cleared value puts the constraints back.
        if (custom && custom.control.value.trim() !== '' && custom.field.custom_answer_option == null) {
          validateTarget.setCustomValidity('');
          return;
        }
        const count = control.querySelectorAll('input:checked').length;
        const minimum = field.required ? Math.max(1, field.min_items ?? 1) : field.min_items;
        const few = minimum != null && (field.required || count > 0) && count < minimum;
        const many = field.max_items != null && count > field.max_items;
        validateTarget.setCustomValidity(
          few
            ? `Select at least ${minimum} option(s).`
            : many
              ? `Select at most ${field.max_items} option(s).`
              : '',
        );
      };
      for (const input of control.querySelectorAll('input')) register(input);
      register(control);
      validateChoices();
      wrapper.append(control);
    } else {
      const label = document.createElement('span');
      label.textContent = `${field.title}${field.required ? ' *' : ''}`;
      control = elicitationControl(field);
      control.required = Boolean(field.required) && field.kind !== 'boolean';
      register(control);
      wrapper.append(label, control);
    }
    if (field.description) {
      const description = document.createElement('span');
      description.className = 'dim';
      description.textContent = field.description;
      wrapper.append(description);
    }
    form.append(wrapper);
    entries.push({ field, control, validateChoices });
  }
  for (const entry of entries) {
    const owner = entry.field.custom_answer_for;
    if (!owner || entry.field.kind !== 'text' || customByOwner.has(owner)) continue;
    const target = entries.find(candidate => candidate.field.id === owner);
    if (!target || !Array.isArray(target.field.options)) continue;
    customByOwner.set(owner, entry);
  }
  // Custom text changes can make the owner group valid or invalid before the
  // user submits it. Keep browser validity and the visible choices in sync.
  for (const entry of entries) {
    if (entry.field.kind === 'multi_select') entry.validateChoices();
    const owner = entry.field.custom_answer_for;
    if (!owner || entry.field.kind !== 'text') continue;
    const target = entries.find(candidate => candidate.field.id === owner);
    if (target?.field.kind === 'multi_select') {
      entry.control.addEventListener('input', target.validateChoices);
      entry.control.addEventListener('change', target.validateChoices);
    }
  }
  return () => {
    for (const entry of entries)
      if (entry.field.kind === 'text') entry.control.value = entry.control.value.trim();
    for (const entry of entries)
      if (entry.field.kind === 'multi_select') entry.validateChoices();
    const active = new Map();
    for (const [owner, entry] of customByOwner)
      if (entry.control.value !== '') active.set(owner, entry);
    if (!form.reportValidity()) return null;
    const content = {};
    for (const entry of entries) {
      const { field, control } = entry;
      if (customByOwner.get(field.custom_answer_for) === entry) {
        if (active.has(field.custom_answer_for)) content[field.id] = control.value;
        continue;
      }
      const custom = active.get(field.id);
      if (custom && custom.field.custom_answer_option == null) continue;
      const value = elicitationFieldValue(field, control);
      if (value !== undefined) content[field.id] = value;
    }
    return content;
  };
}
function buildElicitationCard(session, request) {
  const card = document.createElement('section');
  card.className = 'card elicitation';
  const heading = document.createElement('strong');
  heading.textContent = request.title || 'Input needed';
  const message = document.createElement('pre');
  message.className = 'elicitation-message';
  message.textContent = request.message;
  const form = document.createElement('form');
  const status = document.createElement('p');
  status.className = 'dim';
  const gated = [],
    register = control => {
      gated.push(control);
      return control;
    };
  const collect = buildElicitationForm(form, request, register);
  const actions = document.createElement('div');
  actions.className = 'row';
  const send = document.createElement('button');
  send.type = 'submit';
  send.textContent = 'Send answer';
  register(send);
  const decline = document.createElement('button');
  decline.type = 'button';
  decline.className = 'secondary';
  decline.textContent = 'Decline';
  register(decline);
  const cancel = document.createElement('button');
  cancel.type = 'button';
  cancel.className = 'danger';
  cancel.textContent = 'Cancel';
  register(cancel);
  decline.addEventListener('click', () => {
    submitElicitation(session.id, request.id, { action: 'decline' });
  });
  cancel.addEventListener('click', () => {
    submitElicitation(session.id, request.id, { action: 'cancel' });
  });
  actions.append(send, decline, cancel);
  form.append(actions);
  form.addEventListener('submit', event => {
    event.preventDefault();
    const content = collect();
    if (content) submitElicitation(session.id, request.id, { action: 'accept', content });
  });
  const nodes = [heading];
  if (request.description) {
    const description = document.createElement('p');
    description.className = 'dim';
    description.textContent = request.description;
    nodes.push(description);
  }
  nodes.push(message, form, status);
  card.append(...nodes);
  return {
    card,
    setSent(sent) {
      for (const control of gated) control.disabled = sent;
      status.textContent = sent ? 'Answer sent \u2014 waiting for the session to apply it.' : '';
    },
  };
}
// ---------------------------------------------------------------------------
// Turn review
// ---------------------------------------------------------------------------
//
// The review runs in the daemon; this renders what it published and sends the
// resolution back. Both surfaces show the same review, and either can end it,
// which is what keeps a review from ever locking a phone out of its session.

/// Draws the review card, or takes it down when no review is open.
///
/// Rebuilt only when the published review actually changed, so a thumb resting
/// on a button does not lose it every two seconds.
function renderTurnReview(session) {
  const review = session?.turn_review || null;
  // The session belongs in the identity too. Two sessions can publish an
  // identical review, but their controls must still close over different ids.
  const signature = JSON.stringify([session?.id || null, review]);
  if (reviewSignature === signature) return;
  reviewSignature = signature;
  if (!review) {
    reviewHost.replaceChildren();
    return;
  }
  const card = el('section', 'card turn-review');
  card.append(el('strong', '', `Reviewing this turn (${review.tier})`));
  if (review.roles.length) {
    const strip = el('p', 'dim turn-review-roles');
    strip.textContent = review.roles
      .map(role => `${role.label}: ${role.state}`)
      .join('  ·  ');
    card.append(strip);
  }
  const verdict = review.verdict || null;
  if (verdict && verdict.text) {
    const findings = el('pre', 'turn-review-findings');
    findings.textContent = verdict.text;
    card.append(findings);
  }
  card.append(el('p', 'dim', review.status));
  const actions = el('div', 'row');
  for (const [resolution, label, className] of [
    ['forward', 'Forward findings', ''],
    ['dismiss', 'Dismiss', 'secondary'],
    ['cancel', 'Cancel', 'danger'],
  ]) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    if (className) button.className = className;
    // Cancel always works; the rest wait for the verdict the daemon
    // published, and the daemon refuses anything else anyway.
    button.disabled =
      pendingReviewSessions.has(session.id) ||
      (resolution !== 'cancel' && !(verdict?.allowed || []).includes(resolution));
    button.addEventListener('click', async () => {
      if (pendingReviewSessions.has(session.id)) return;
      pendingReviewSessions.add(session.id);
      // One resolution owns the whole card while it is in flight. Otherwise a
      // second tap can race a different answer into the same review.
      for (const control of actions.children) control.disabled = true;
      try {
        await sendAction({
          action: 'resolve-review',
          session_id: session.id,
          resolution,
        });
      } finally {
        pendingReviewSessions.delete(session.id);
        // A failed request leaves the review open. Rebuild the still-current
        // card from its published gates so every valid action becomes usable
        // again; never revive controls from a conversation already left behind.
        if (currentSession === session.id) {
          reviewSignature = null;
          renderTurnReview(activeSession());
        }
      }
    });
    actions.append(button);
  }
  card.append(actions);
  reviewHost.replaceChildren(card);
}

function renderElicitations(session) {
  const pending = (session && session.pending_elicitations) || [];
  if (session)
    for (const key of [...sentElicitations])
      if (
        key.startsWith(`${session.id}\u001f`) &&
        !pending.some(request => elicitationKey(session.id, request.id) === key)
      )
        sentElicitations.delete(key);
  const live = new Set(),
    cards = [];
  for (const request of pending) {
    const key = elicitationKey(session.id, request.id),
      signature = JSON.stringify(request);
    live.add(key);
    let entry = elicitationCards.get(key);
    if (!entry || entry.signature !== signature) {
      entry = buildElicitationCard(session, request);
      entry.signature = signature;
      elicitationCards.set(key, entry);
    }
    entry.setSent(sentElicitations.has(key));
    cards.push(entry.card);
  }
  for (const key of [...elicitationCards.keys()]) if (!live.has(key)) elicitationCards.delete(key);
  const mounted = [...elicitations.children];
  if (mounted.length !== cards.length || cards.some((card, index) => mounted[index] !== card))
    elicitations.replaceChildren(...cards);
}
async function submitElicitation(sessionId, elicitationId, response) {
  const key = elicitationKey(sessionId, elicitationId);
  if (sentElicitations.has(key)) return;
  sentElicitations.add(key);
  const rerender = () => {
    const session = snapshot?.sessions.find(x => x.id === sessionId);
    if (session && sessionId === currentSession) renderElicitations(session);
  };
  rerender();
  try {
    await request('/api/actions', {
      method: 'POST',
      body: JSON.stringify({
        action: 'respond-elicitation',
        session_id: sessionId,
        elicitation_id: elicitationId,
        response,
      }),
    });
    document.querySelector('#conversation-error').textContent = '';
    await refresh();
  } catch (err) {
    sentElicitations.delete(key);
    document.querySelector('#conversation-error').textContent = err.message;
    rerender();
  }
}
// The composer is a contenteditable rather than a textarea so a pasted or
// dropped image can be intercepted where it lands, and so the box grows with
// its content without a layout read on every keystroke. Rich content is
// refused at beforeinput, which keeps the box plain text however it arrives.
const MAX_PROMPT_REQUEST_BYTES = 32 * 1024 * 1024;
const MAX_PROMPT_IMAGES = 10;
const MAX_IMAGE_UPLOAD_BYTES = 64 * 1024 * 1024;
const MAX_IMAGE_UPLOADS_IN_FLIGHT = 2;
const SUPPORTED_IMAGE_TYPES = new Set(['image/jpeg', 'image/png', 'image/webp']);
let composerRevision = 0,
  composerPreserveEmptyBreak = false,
  promptImages = [],
  imageUploadQueue = [],
  imageUploadsInFlight = 0,
  nextPromptImageId = 1;
function composerText() {
  let text = '';
  const blocks = new Set(['DIV', 'P']);
  const append = node => {
    if (node.nodeType === Node.TEXT_NODE) {
      text += node.nodeValue || '';
      return;
    }
    if (node.nodeName === 'BR') {
      if (!node.dataset.composerFiller) text += '\n';
      return;
    }
    const block = node !== promptText && blocks.has(node.nodeName);
    if (block && text && !text.endsWith('\n')) text += '\n';
    node.childNodes.forEach(append);
    if (block && node.nextSibling && !text.endsWith('\n')) text += '\n';
  };
  append(promptText);
  return text.replace(/\r\n?/g, '\n');
}
let composerGeneration = 0;
function setComposerText(text) {
  composerGeneration += 1;
  promptText.textContent = text;
}
function placeComposerCaretAtEnd() {
  const selection = window.getSelection();
  if (!selection) return;
  const range = document.createRange();
  range.selectNodeContents(promptText);
  range.collapse(false);
  selection.removeAllRanges();
  selection.addRange(range);
}
function placeComposerCaretAtPoint(x, y) {
  let range = document.caretRangeFromPoint?.(x, y) || null;
  if (!range && document.caretPositionFromPoint) {
    const position = document.caretPositionFromPoint(x, y);
    if (position) {
      range = document.createRange();
      range.setStart(position.offsetNode, position.offset);
      range.collapse(true);
    }
  }
  if (!range || !promptText.contains(range.startContainer)) return;
  const selection = window.getSelection();
  if (!selection) return;
  selection.removeAllRanges();
  selection.addRange(range);
}
function insertComposerFallback(node, filler = null) {
  const selection = window.getSelection();
  const range = selection && selection.rangeCount ? selection.getRangeAt(0) : null;
  if (!range || !promptText.contains(range.commonAncestorContainer)) {
    promptText.append(node);
    if (filler) promptText.append(filler);
    placeComposerCaretAtEnd();
    return;
  }
  range.deleteContents();
  range.insertNode(node);
  if (filler) node.after(filler);
  range.setStartAfter(node);
  range.collapse(true);
  selection.removeAllRanges();
  selection.addRange(range);
}
// execCommand keeps the browser's own undo stack, so it is tried first; the
// fallback covers engines that refuse it, and the revision check covers those
// that run it without emitting the input event that keeps state in step.
function runComposerEdit(command, value, fallback) {
  promptText.focus();
  const revision = composerRevision;
  if (document.execCommand(command, false, value)) {
    if (composerRevision === revision) composerInputChanged();
    return;
  }
  fallback();
  composerInputChanged();
}
function insertComposerText(text) {
  const normalized = text.replace(/\r\n?/g, '\n');
  runComposerEdit('insertText', normalized, () => {
    insertComposerFallback(document.createTextNode(normalized));
  });
}
function insertComposerLineBreak() {
  composerPreserveEmptyBreak = true;
  try {
    runComposerEdit('insertLineBreak', null, () => {
      const filler = document.createElement('br');
      filler.dataset.composerFiller = 'true';
      insertComposerFallback(document.createElement('br'), filler);
    });
    let last = promptText;
    while (last.lastChild) last = last.lastChild;
    if (last.nodeName === 'BR' && last.previousSibling?.nodeName === 'BR') {
      last.dataset.composerFiller = 'true';
    }
  } finally {
    composerPreserveEmptyBreak = false;
  }
}
// A cleared box can keep a stray break behind it, which leaves the placeholder
// hidden and the box looking occupied when it holds nothing.
function composerInputChanged() {
  composerRevision += 1;
  if (!composerPreserveEmptyBreak && !promptText.textContent && promptText.childNodes.length)
    promptText.replaceChildren();
}

// ---------------------------------------------------------------------------
// Voice input
// ---------------------------------------------------------------------------
//
// Microphone permission, audio capture and transcription are one browser
// operation. The generation token makes every asynchronous boundary safe when
// a person cancels, changes session, signs out, or leaves the page.
const VOICE_MAX_RECORDING_MS = 10 * 60 * 1000;
const VOICE_TRANSCRIPTION_TIMEOUT_MS = 120 * 1000;
const VOICE_STATES = new Set(['permission', 'recording', 'transcribing']);
let voiceState = 'idle';
let voiceOperation = null;
let voiceGeneration = 0;

function voiceIsActive() {
  return VOICE_STATES.has(voiceState);
}

function voiceOperationIsCurrent(operation) {
  return (
    voiceOperation === operation
    && operation.generation === voiceGeneration
    && currentSession === operation.sessionId
  );
}

function syncSendButtonDisabled() {
  const session = activeSession();
  const canPrompt = Boolean(
    session
    && session.capabilities?.prompt !== false
    && !session.turn_review
    && !isTransitioningSession(session)
    && !isLoadingConversationSession(session),
  );
  sendButton.disabled =
    !canPrompt
    || voiceIsActive()
    || promptInFlight
    || promptImages.some(image => image.state !== 'ready');
}

function renderVoiceState() {
  const active = voiceIsActive();
  const session = activeSession();
  const canPrompt = Boolean(
    session
    && session.capabilities?.prompt !== false
    && !session.turn_review
    && !isTransitioningSession(session)
    && !isLoadingConversationSession(session),
  );
  voiceControls.classList.toggle('hidden', !active);
  voiceInput.dataset.state = voiceState;
  voiceInput.textContent = voiceState === 'recording' ? 'Stop & transcribe' : 'Voice';
  voiceInput.setAttribute(
    'aria-label',
    voiceState === 'recording' ? 'Stop recording and transcribe' : 'Start voice input',
  );
  voiceInput.disabled =
    !canPrompt
    || voiceState === 'permission'
    || voiceState === 'transcribing';
  voiceCancel.disabled = !active;
  if (voiceState === 'permission') voiceStatus.textContent = 'Requesting microphone permission…';
  else if (voiceState === 'recording') voiceStatus.textContent = 'Recording. Tap stop when finished.';
  else if (voiceState === 'transcribing') voiceStatus.textContent = 'Transcribing…';
  else voiceStatus.textContent = '';
  syncSendButtonDisabled();
}

function stopVoiceTracks(stream) {
  for (const track of stream?.getTracks?.() || []) track.stop();
}

function closeVoiceAudio(operation) {
  clearTimeout(operation.prepareTimer);
  operation.prepareTimer = null;
  stopVoiceTracks(operation.stream);
  operation.stream = null;
  for (const node of [operation.source, operation.workletNode, operation.sink]) {
    try { node?.disconnect(); } catch { /* already disconnected */ }
  }
  try { operation.workletNode?.port?.close?.(); } catch { /* already closed */ }
  operation.source = null;
  operation.workletNode = null;
  operation.sink = null;
  if (operation.audioContext && operation.audioContext.state !== 'closed') {
    Promise.resolve(operation.audioContext.close()).catch(() => {});
  }
  operation.audioContext = null;
}

function cancelVoiceInput({ clearError = false } = {}) {
  const operation = voiceOperation;
  voiceGeneration += 1;
  voiceOperation = null;
  voiceState = 'idle';
  if (operation) {
    operation.controller.abort();
    clearTimeout(operation.limitTimer);
    closeVoiceAudio(operation);
    try { operation.worker?.terminate(); } catch { /* already terminated */ }
    operation.worker = null;
  }
  if (clearError) document.querySelector('#conversation-error').textContent = '';
  renderVoiceState();
}

function failVoiceOperation(operation, message) {
  if (!voiceOperationIsCurrent(operation)) return;
  voiceGeneration += 1;
  voiceOperation = null;
  voiceState = 'idle';
  operation.controller.abort();
  clearTimeout(operation.limitTimer);
  closeVoiceAudio(operation);
  try { operation.worker?.terminate(); } catch { /* already terminated */ }
  operation.worker = null;
  document.querySelector('#conversation-error').textContent = message;
  renderVoiceState();
}

function voiceBrowserSupportError() {
  if (window.isSecureContext === false)
    return 'Voice input requires a secure HTTPS connection.';
  if (!navigator.mediaDevices?.getUserMedia)
    return 'This browser does not support microphone input.';
  if (!(window.AudioContext || window.webkitAudioContext))
    return 'This browser does not support the audio capture required for voice input.';
  if (!window.AudioWorkletNode || !window.Worker)
    return 'This browser does not support the audio capture required for voice input.';
  return null;
}

function voicePermissionError(error) {
  if (error?.name === 'NotAllowedError' || error?.name === 'PermissionDeniedError')
    return 'Microphone permission was denied. Allow microphone access and try again.';
  if (error?.name === 'NotFoundError' || error?.name === 'DevicesNotFoundError')
    return 'No microphone was found. Connect a microphone and try again.';
  if (error?.name === 'SecurityError')
    return 'Microphone access was blocked by the browser security policy.';
  if (error?.name === 'NotReadableError' || error?.name === 'TrackStartError')
    return 'The microphone is already in use or could not be read.';
  return error?.message || 'The browser could not start microphone input.';
}

async function transcribeVoice(operation, wav) {
  operation.transcriptionTimedOut = false;
  const timeout = setTimeout(() => {
    operation.transcriptionTimedOut = true;
    operation.controller.abort();
  }, VOICE_TRANSCRIPTION_TIMEOUT_MS);
  try {
    const response = await upgradeAwareFetch(
      `/api/sessions/${encodeURIComponent(operation.sessionId)}/dictation`,
      {
        method: 'POST',
        body: wav,
        signal: operation.controller.signal,
        headers: { 'content-type': 'audio/wav' },
      },
    );
    if (response.status === 401) {
      showLogin();
      throw new Error('unauthorized');
    }
    if (!response.ok) {
      const body = await response.json().catch(() => ({}));
      throw new Error(body.error || response.statusText || 'Voice transcription failed.');
    }
    const result = await response.json();
    if (typeof result.text !== 'string') throw new Error('The transcription response was invalid.');
    return result.text;
  } catch (error) {
    if (error.name === 'AbortError' && operation.transcriptionTimedOut)
      throw new Error('Voice transcription timed out after 120 seconds. Try a shorter recording.');
    throw error;
  } finally {
    clearTimeout(timeout);
  }
}

function appendVoiceText(text) {
  const transcription = text.trim();
  if (!transcription) {
    announce('No speech was detected.');
    return;
  }
  const existing = composerText();
  const separator = existing && !/\s$/.test(existing) ? ' ' : '';
  // Write at the end so an edit made while transcription was in flight is
  // retained. Images are separate browser state and are intentionally left
  // untouched here.
  setComposerText(`${existing}${separator}${transcription}`);
  composerInputChanged();
  updateCommandPalette();
  scheduleDraftSave();
  announce('Voice transcription added to the draft.');
}

async function completeVoiceTranscription(operation, wav) {
  if (!voiceOperationIsCurrent(operation)) return;
  closeVoiceAudio(operation);
  try { operation.worker?.terminate(); } catch { /* already terminated */ }
  operation.worker = null;
  try {
    const text = await transcribeVoice(operation, wav);
    if (!voiceOperationIsCurrent(operation)) return;
    appendVoiceText(text);
    clearTimeout(operation.limitTimer);
    operation.limitTimer = null;
    voiceOperation = null;
    voiceState = 'idle';
    renderVoiceState();
  } catch (error) {
    if (!voiceOperationIsCurrent(operation)) return;
    clearTimeout(operation.limitTimer);
    operation.limitTimer = null;
    voiceOperation = null;
    voiceState = 'idle';
    document.querySelector('#conversation-error').textContent =
      error.message === 'unauthorized'
        ? ''
        : error.message || 'Voice transcription failed.';
    renderVoiceState();
  }
}

function handleVoiceWorkerMessage(operation, message) {
  if (!voiceOperationIsCurrent(operation)) return;
  if (message.type === 'error') {
    failVoiceOperation(operation, message.message);
  } else if (message.type === 'limit') {
    announce('Ten minute recording limit reached. Transcribing now.');
    stopVoiceRecording();
  } else if (message.type === 'wav') {
    completeVoiceTranscription(operation, message.buffer);
  }
}

function finishVoiceRecording(operation) {
  if (!voiceOperationIsCurrent(operation) || operation.finishRequested) return;
  operation.finishRequested = true;
  operation.prepareTimer = setTimeout(() => {
    failVoiceOperation(operation, 'The browser could not finish the recording. Please try again.');
  }, 10_000);
  if (operation.workletNode?.port) {
    // The worklet posts `flushed` after its final PCM message. Posting WAV
    // finish only then preserves every queued audio chunk.
    operation.workletNode.port.postMessage({ type: 'flush' });
  } else {
    operation.worker?.postMessage({ type: 'finish' });
  }
}

function stopVoiceRecording() {
  const operation = voiceOperation;
  if (!operation || voiceState !== 'recording') return;
  voiceState = 'transcribing';
  clearTimeout(operation.limitTimer);
  operation.limitTimer = null;
  renderVoiceState();
  stopVoiceTracks(operation.stream);
  // Keep the worklet graph alive until it acknowledges the flush, otherwise
  // its final render quantum could be dropped when the node is disconnected.
  finishVoiceRecording(operation);
}

async function startVoiceInput() {
  if (voiceIsActive()) {
    if (voiceState === 'recording') stopVoiceRecording();
    return;
  }
  const sessionId = currentSession;
  if (!sessionId) return;
  const supportError = voiceBrowserSupportError();
  if (supportError) {
    document.querySelector('#conversation-error').textContent = supportError;
    return;
  }
  const operation = {
    sessionId,
    generation: ++voiceGeneration,
    controller: new AbortController(),
    stream: null,
    worker: null,
    audioContext: null,
    source: null,
    workletNode: null,
    sink: null,
    limitTimer: null,
    finishRequested: false,
    transcriptionTimedOut: false,
  };
  voiceOperation = operation;
  voiceState = 'permission';
  document.querySelector('#conversation-error').textContent = '';
  renderVoiceState();
  try {
    // Construct and resume the context in the click task. Mobile Safari can
    // reject an AudioContext created only after the availability request.
    const AudioContextConstructor = window.AudioContext || window.webkitAudioContext;
    operation.audioContext = new AudioContextConstructor();
    const resume = Promise.resolve(operation.audioContext.resume?.()).catch(error => {
      failVoiceOperation(operation, voicePermissionError(error));
    });
    const available = await request(
      `/api/sessions/${encodeURIComponent(sessionId)}/dictation`,
      { signal: operation.controller.signal },
    );
    if (!voiceOperationIsCurrent(operation)) return;
    if (available?.available !== true)
      throw new Error(available?.reason || 'Voice input is unavailable for this session.');

    const stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        channelCount: { ideal: 1 },
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
    });
    if (!voiceOperationIsCurrent(operation)) {
      stopVoiceTracks(stream);
      return;
    }
    operation.stream = stream;
    for (const track of stream.getTracks?.() || []) {
      track.addEventListener?.('ended', () => {
        if (voiceOperationIsCurrent(operation) && voiceState !== 'transcribing')
          failVoiceOperation(operation, 'The microphone stopped unexpectedly. Try again.');
      }, { once: true });
    }
    const WorkerConstructor = window.Worker;
    operation.worker = new WorkerConstructor('/voice-worker.js');
    operation.worker.onmessage = event => handleVoiceWorkerMessage(operation, event.data || {});
    operation.worker.onerror = () => handleVoiceWorkerMessage(operation, {
      type: 'error',
      message: 'The browser audio worker failed.',
    });
    operation.worker.postMessage({ type: 'start' });

    if (!operation.audioContext.audioWorklet?.addModule)
      throw new Error('This browser does not support the audio capture required for voice input.');
    await operation.audioContext.audioWorklet.addModule('/voice-worklet.js');
    if (!voiceOperationIsCurrent(operation)) return;
    const source = operation.audioContext.createMediaStreamSource(stream);
    const worklet = new window.AudioWorkletNode(
      operation.audioContext,
      'voice-capture-processor',
    );
    const sink = operation.audioContext.createGain();
    sink.gain.value = 0;
    operation.source = source;
    operation.workletNode = worklet;
    operation.sink = sink;
    operation.audioContext.addEventListener?.('statechange', () => {
      if (
        voiceOperationIsCurrent(operation)
        && voiceState === 'recording'
        && operation.audioContext?.state !== 'running'
      ) {
        failVoiceOperation(operation, 'Audio capture was interrupted. Try again.');
      }
    });
    worklet.addEventListener?.('processorerror', () => {
      failVoiceOperation(operation, 'The browser audio processor failed. Try again.');
    });
    worklet.port.onmessage = event => {
      if (!voiceOperationIsCurrent(operation)) return;
      if (event.data?.type === 'pcm') {
        try {
          operation.worker?.postMessage(
            { type: 'pcm', samples: event.data.samples },
            [event.data.samples.buffer],
          );
        } catch (error) {
          handleVoiceWorkerMessage(operation, {
            type: 'error',
            message: error.message || 'The browser could not transfer microphone audio.',
          });
        }
      } else if (event.data?.type === 'flushed') {
        operation.worker?.postMessage({ type: 'finish' });
      }
    };
    worklet.port.start?.();
    source.connect(worklet);
    worklet.connect(sink);
    sink.connect(operation.audioContext.destination);
    await resume;
    if (!voiceOperationIsCurrent(operation)) return;
    voiceState = 'recording';
    operation.limitTimer = setTimeout(stopVoiceRecording, VOICE_MAX_RECORDING_MS);
    renderVoiceState();
  } catch (error) {
    if (!voiceOperationIsCurrent(operation)) return;
    voiceOperation = null;
    voiceState = 'idle';
    closeVoiceAudio(operation);
    try { operation.worker?.terminate(); } catch { /* already terminated */ }
    operation.worker = null;
    document.querySelector('#conversation-error').textContent = voicePermissionError(error);
    renderVoiceState();
  }
}
function imageDimensions(file) {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const image = new Image();
    image.addEventListener(
      'load',
      () => {
        const size = { width: image.naturalWidth, height: image.naturalHeight };
        URL.revokeObjectURL(url);
        resolve(size);
      },
      { once: true },
    );
    image.addEventListener(
      'error',
      () => {
        URL.revokeObjectURL(url);
        reject(new Error('the browser could not decode this image'));
      },
      { once: true },
    );
    image.src = url;
  });
}

function imageFileError(file) {
  const name = file.name || 'That file';
  if (!file.type.startsWith('image/')) return `${name} is not an image`;
  if (!SUPPORTED_IMAGE_TYPES.has(file.type))
    return `${name} is not a supported image; use JPEG, PNG, or WebP`;
  if (file.size > MAX_IMAGE_UPLOAD_BYTES)
    return `${name} is too large; images must be 64 MiB or smaller`;
  return null;
}

function revokePromptImage(image) {
  if (image.controller) image.controller.abort();
  if (image.preview_url) {
    URL.revokeObjectURL(image.preview_url);
    image.preview_url = null;
  }
  image.cancelled = true;
}

function clearPromptImages() {
  imageUploadQueue = [];
  for (const image of promptImages) revokePromptImage(image);
  promptImages = [];
  renderAttachments();
}

function imageIsCurrent(image, sessionId) {
  return currentSession === sessionId && !image.cancelled && promptImages.includes(image);
}

async function processImageUpload(image, sessionId) {
  try {
    const size = await imageDimensions(image.file);
    if (!size.width || !size.height) throw new Error('the image has no usable dimensions');
    if (!imageIsCurrent(image, sessionId)) return;
    const uploaded = await uploadAttachment(sessionId, image.file, image.controller.signal);
    if (!imageIsCurrent(image, sessionId)) return;
    image.state = 'ready';
    image.error = '';
    image.attachment = uploaded.attachment;
    image.mime_type = uploaded.mime_type;
    image.width = uploaded.width || size.width;
    image.height = uploaded.height || size.height;
    image.file = null;
  } catch (error) {
    if (!imageIsCurrent(image, sessionId)) return;
    if (error.name === 'AbortError') return;
    image.state = 'failed';
    image.error = error.message || 'the image could not be uploaded';
    image.file = null;
  } finally {
    if (imageIsCurrent(image, sessionId)) renderAttachments();
  }
}

function pumpImageUploads() {
  while (imageUploadsInFlight < MAX_IMAGE_UPLOADS_IN_FLIGHT && imageUploadQueue.length) {
    const job = imageUploadQueue.shift();
    if (!imageIsCurrent(job.image, job.sessionId)) continue;
    imageUploadsInFlight += 1;
    processImageUpload(job.image, job.sessionId).finally(() => {
      imageUploadsInFlight -= 1;
      pumpImageUploads();
    });
  }
}

async function attachImageFiles(files) {
  const session = snapshot?.sessions.find(x => x.id === currentSession);
  if (!currentSession || !session?.prompt_images_supported || !files.length) return;
  const sessionId = currentSession;
  const remaining = MAX_PROMPT_IMAGES - promptImages.length;
  const selected = files.slice(0, Math.max(0, remaining));
  const error = document.querySelector('#conversation-error');
  if (files.length > selected.length) {
    error.textContent = 'A prompt may contain at most 10 images.';
  } else {
    error.textContent = '';
  }
  for (const file of selected) {
    const image = {
      id: nextPromptImageId++,
      name: file.name || 'Pasted image',
      file,
      preview_url: URL.createObjectURL(file),
      mime_type: file.type,
      width: 0,
      height: 0,
      attachment: null,
      state: 'processing',
      error: '',
      controller: new AbortController(),
      cancelled: false,
    };
    const invalid = imageFileError(file);
    if (invalid) {
      image.state = 'failed';
      image.error = invalid;
      image.file = null;
    } else {
      imageUploadQueue.push({ image, sessionId });
    }
    promptImages.push(image);
  }
  renderAttachments();
  pumpImageUploads();
}

function removePromptImage(image) {
  const index = promptImages.indexOf(image);
  if (index < 0) return;
  promptImages.splice(index, 1);
  revokePromptImage(image);
  imageUploadQueue = imageUploadQueue.filter(job => job.image !== image);
  renderAttachments();
  pumpImageUploads();
}

function renderAttachments() {
  // The draft the daemon keeps is text. An attachment lives in this browser
  // only, and a photograph that quietly disappears on reload is worse than one
  // somebody was told about.
  const session = snapshot?.sessions.find(x => x.id === currentSession);
  attachImage.hidden = !session?.prompt_images_supported;
  attachments.replaceChildren();
  if (promptImages.length) {
    attachments.append(
      el('p', 'dim', 'Images stay on this device until sent; a draft keeps only the text.'),
    );
  }
  for (const image of promptImages) {
    const chip = document.createElement('div');
    chip.className = `attachment attachment-${image.state}`;
    chip.setAttribute('aria-busy', String(image.state === 'processing'));
    const thumb = document.createElement('img');
    thumb.alt = '';
    thumb.src = image.preview_url;
    const caption = document.createElement('span');
    caption.textContent = image.state === 'processing'
      ? `${image.name} \u00b7 Uploading…`
      : image.state === 'failed'
        ? `${image.name} \u00b7 ${image.error}`
        : `${image.name} \u00b7 ${image.width}\u00d7${image.height}`;
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'danger';
    remove.setAttribute('aria-label', `Remove ${image.name}`);
    remove.textContent = '\u00d7';
    remove.onclick = () => removePromptImage(image);
    chip.append(thumb, caption, remove);
    attachments.append(chip);
  }
  syncSendButtonDisabled();
}
// ---------------------------------------------------------------------------
// Drafts and history
// ---------------------------------------------------------------------------
//
// A draft is stored by the daemon against this viewer and this session, so it
// survives a reload, a closed tab and a new phone. Unsent image attachments are
// not: they live in this browser's memory only, and the composer says so, since
// a photograph that quietly disappears is worse than one you were told about.

const DRAFT_DEBOUNCE_MS = 400;
let draftTimer = null;
let draftSaving = false;

function scheduleDraftSave() {
  if (draftTimer) clearTimeout(draftTimer);
  draftTimer = setTimeout(saveDraft, DRAFT_DEBOUNCE_MS);
}

async function saveDraft() {
  draftTimer = null;
  if (!currentSession || draftSaving) return;
  const sessionId = currentSession;
  const draft = composerText();
  draftSaving = true;
  try {
    await request(`/api/sessions/${encodeURIComponent(sessionId)}/draft`, {
      method: 'PUT',
      body: JSON.stringify({ draft }),
    });
  } catch {
    // A draft that could not be stored is still in the composer, which is the
    // copy that matters. Saying so on every keystroke would be noise.
  } finally {
    draftSaving = false;
  }
}

/// Put back what this viewer last typed here and did not send.
async function restoreDraft(sessionId, generation) {
  try {
    const stored = await request(`/api/sessions/${encodeURIComponent(sessionId)}/client-state`);
    if (generation !== conversationGeneration) return;
    // Anything typed while the request was in flight belongs to the person,
    // not to the server.
    if (stored.draft && !composerText()) {
      setComposerText(stored.draft);
      updateCommandPalette();
    }
    if (stored.through_event_ordinal > acknowledged) {
      acknowledged = stored.through_event_ordinal;
    }
  } catch {
    // An unavailable draft is not worth a message: the composer is empty and
    // the person can type.
  }
}

let historyOpen = false;

/// Search this project's earlier prompts and offer them in the palette.
async function searchHistory(query) {
  if (!currentSession) return;
  const generation = conversationGeneration;
  try {
    const found = await request(
      `/api/sessions/${encodeURIComponent(currentSession)}/history?q=${encodeURIComponent(query)}&scope=project`,
    );
    if (generation !== conversationGeneration || !historyOpen) return;
    paletteMatches = found.entries.map(text => ({
      insert: text,
      label: text.length > 80 ? `${text.slice(0, 79)}…` : text,
      hint: '',
    }));
    if (found.truncated) {
      // Saying the answer is partial is the whole reason the bound reports it.
      paletteMatches.push({
        insert: composerText(),
        label: `More matches than ${paletteMatches.length} — narrow the search`,
        hint: '',
      });
    }
    paletteSelected = 0;
    if (!paletteMatches.length) {
      commandPalette.replaceChildren(el('p', 'dim palette-row', 'No earlier prompts match.'));
      commandPalette.classList.remove('hidden');
      return;
    }
    commandPalette.replaceChildren(
      ...paletteMatches.map((match, index) => {
        const row = el('button', 'palette-row');
        row.type = 'button';
        row.setAttribute('role', 'option');
        row.setAttribute('aria-selected', String(index === paletteSelected));
        row.dataset.insert = match.insert;
        row.append(el('span', 'palette-name', match.label));
        return row;
      }),
    );
    commandPalette.classList.remove('hidden');
  } catch (err) {
    document.querySelector('#conversation-error').textContent = err.message;
  }
}

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------
//
// The rules behind these live in Rust and are published in the session
// projection. Whether fast mode exists, whether plan mode can be driven, and
// which values `model` and `effort` accept are facts about the harness, so the
// browser reads the published answer rather than deciding again. Where a check
// here and a check there ever disagree, the Rust one is right and this one is
// the bug.

function activeSession() {
  return snapshot?.sessions.find(session => session.id === currentSession);
}

function configOption(key) {
  return activeSession()?.config_options?.find(option => option.key === key);
}

/// The commands offered for what has been typed so far.
///
/// The list is the daemon's: it knows what this session's harness advertised
/// and what Mjolnir itself offers, and publishing it is what keeps the phone from
/// missing a command the terminal has.
function availableCommands() {
  return activeSession()?.available_commands || [];
}

let paletteMatches = [];
let paletteSelected = 0;

/// What the palette should offer, given the composer's text.
///
/// After a complete `/model ` the palette offers values rather than commands,
/// and a fully typed advertised value closes it so Enter submits instead of
/// accepting the text again.
function paletteState(text) {
  for (const key of ['model', 'effort']) {
    const prefix = `/${key} `;
    if (!text.startsWith(prefix)) continue;
    const option = configOption(key);
    if (!option) return null;
    const query = text.slice(prefix.length);
    if (option.choices.some(choice => choice.value === query)) return null;
    const matches = option.choices
      .filter(
        choice =>
          choice.value.toLowerCase().startsWith(query.toLowerCase()) ||
          choice.name.toLowerCase().includes(query.toLowerCase()),
      )
      .map(choice => ({
        insert: `/${key} ${choice.value}`,
        label: choice.value,
        hint: choice.name,
      }));
    return matches.length ? matches : null;
  }
  if (!text.startsWith('/') || /\s/.test(text)) return null;
  const query = text.slice(1).toLowerCase();
  const matches = availableCommands()
    .filter(
      command =>
        command.name.startsWith(query) || command.description.toLowerCase().includes(query),
    )
    .map(command => ({
      insert: `/${command.name} `,
      label: `/${command.name}${command.argument ? ` <${command.argument}>` : ''}`,
      hint: command.description,
    }));
  return matches.length ? matches : null;
}

function updateCommandPalette() {
  const matches = paletteState(composerText());
  if (!matches) {
    paletteMatches = [];
    commandPalette.classList.add('hidden');
    commandPalette.replaceChildren();
    return;
  }
  // Keep the highlighted entry by name across a re-render, so typing another
  // character does not silently move the selection under the reader.
  const previous = paletteMatches[paletteSelected]?.insert;
  paletteMatches = matches;
  paletteSelected = Math.max(
    0,
    matches.findIndex(match => match.insert === previous),
  );
  commandPalette.replaceChildren(
    ...matches.map((match, index) => {
      const row = el('button', 'palette-row');
      row.type = 'button';
      row.setAttribute('role', 'option');
      row.setAttribute('aria-selected', String(index === paletteSelected));
      row.dataset.insert = match.insert;
      row.append(el('span', 'palette-name', match.label), el('span', 'dim', match.hint));
      return row;
    }),
  );
  commandPalette.classList.remove('hidden');
}

function moveCommandSelection(delta) {
  if (!paletteMatches.length) return false;
  paletteSelected = (paletteSelected + delta + paletteMatches.length) % paletteMatches.length;
  updateCommandPaletteSelection();
  return true;
}

function updateCommandPaletteSelection() {
  [...commandPalette.children].forEach((row, index) => {
    row.setAttribute('aria-selected', String(index === paletteSelected));
  });
}

function acceptCommandSelection() {
  const match = paletteMatches[paletteSelected];
  if (!match) return false;
  setComposerText(match.insert);
  placeComposerCaretAtEnd();
  historyOpen = false;
  updateCommandPalette();
  scheduleDraftSave();
  return true;
}

/// Everything Mjolnir and the agent offer, as a system note in the transcript.
function showHelp() {
  const lines = ['Available commands:', '!<command> — run a shell command in this session [mj]'];
  for (const command of availableCommands()) {
    const argument = command.argument ? ` <${command.argument}>` : '';
    lines.push(
      `/${command.name}${argument} — ${command.description} [${command.source || 'mj'}]`,
    );
  }
  const note = el('article', 'entry tone-system');
  const heading = el('strong');
  const glyph = el('span', 'entry-glyph', '─');
  glyph.setAttribute('aria-hidden', 'true');
  heading.append(glyph, el('span', 'entry-label', 'Mjolnir'));
  note.append(heading, el('pre', 'entry-body', lines.join('\n')));
  feed.append(note);
  scrollToTail();
}

/// The shared `/review status` sentence, from the bounded config projection.
///
/// Keep this byte-for-byte aligned with `hel_chat::review_status_line`: the
/// same configuration should answer the same way on the terminal and phone.
function reviewStatusLine(review, open) {
  const enabled = review?.enabled === true;
  const profile = review?.profile;
  const tier = review?.tier || 'quick';
  let armed;
  if (enabled && profile) {
    armed = `Reviewing every completed turn with [review] profile ${JSON.stringify(profile)} (${tier} tier)`;
  } else if (enabled) {
    armed = '[review] enabled = true but no profile is named, so nothing can review';
  } else if (profile) {
    armed = `Automatic review is off; /review reviews one turn with ${JSON.stringify(profile)} (${tier} tier)`;
  } else {
    armed = 'Turn review needs a reviewer: set [review] profile in config.toml';
  }
  return open ? `${armed}. A review is open now.` : armed;
}

/// Run a local command, or report that nothing here can.
///
/// Returns true when the text was a command this surface handled, so the
/// caller knows not to send it to the agent as a prompt.
async function runLocalCommand(text) {
  const match = /^\/([a-zA-Z][\w-]*)\s*(.*)$/.exec(text);
  if (!match) return false;
  const [, name, argument] = match;
  const error = document.querySelector('#conversation-error');
  const session = activeSession();

  switch (name) {
    case 'help':
      setComposerText('');
      showHelp();
      return true;
    case 'detach':
      setComposerText('');
      navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
      return true;
    case 'model':
    case 'effort': {
      if (!argument) {
        error.textContent = `usage: /${name} <value>`;
        return true;
      }
      await sendAction({
        action: 'set-config',
        session_id: currentSession,
        key: name,
        value: argument,
      });
      return true;
    }
    case 'fast': {
      const option = configOption('model');
      const current = option?.current || '';
      if (!option) {
        error.textContent = 'Fast mode is unavailable for this agent.';
        return true;
      }
      // Fast mode is a model, so the toggle is between the current model and
      // its fast counterpart, both of which the harness advertised.
      const fast = option.choices.find(choice => /fast/i.test(choice.value));
      if (!fast) {
        error.textContent = 'Fast mode is unavailable for the active model.';
        return true;
      }
      const target = /fast/i.test(current)
        ? option.choices.find(choice => !/fast/i.test(choice.value))?.value
        : fast.value;
      if (!target) {
        error.textContent = 'Fast mode is unavailable for the active model.';
        return true;
      }
      await sendAction({
        action: 'set-config',
        session_id: currentSession,
        key: 'model',
        value: target,
      });
      return true;
    }
    case 'review': {
      const scope = argument.trim().toLowerCase();
      if (scope === 'status') {
        error.textContent = reviewStatusLine(
          snapshot?.review_config,
          Boolean(session?.turn_review),
        );
        setComposerText('');
        return true;
      }
      if (scope) {
        // Arming review is configuration, not a session gesture.
        error.textContent =
          'automatic review is configured in config.toml: [review] enabled, tier';
        setComposerText('');
        return true;
      }
      await sendAction({ action: 'start-review', session_id: currentSession });
      return true;
    }
    case 'plan':
    case 'implement': {
      if (!session?.capabilities?.set_plan_mode) {
        error.textContent = 'Plan mode is only available while the agent is idle.';
        return true;
      }
      const active = name === 'plan' ? !session.plan_mode_active : false;
      if (argument) {
        const sessionId = currentSession;
        setComposerText('');
        saveDraft();
        await sendSubmission({ action: 'prompt', session_id: sessionId, text: argument, images: [] }, [], async () => {
          await request('/api/actions', { method: 'POST', body: JSON.stringify({ action: 'set-plan-mode', session_id: sessionId, active }) });
        }, text);
      } else {
        await sendAction({ action: 'set-plan-mode', session_id: currentSession, active });
      }
      return true;
    }
    default:
      // Anything else is the agent's own command, and the agent is the one
      // that knows what to do with it.
      return false;
  }
}

/// Post one action and report its failure where the composer can be seen.
async function sendAction(body) {
  const error = document.querySelector('#conversation-error');
  const sessionId = body.session_id;
  const submittedGeneration = composerGeneration;
  try {
    await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    // Do not let an action that completed after navigation clear the next
    // conversation's draft or error state.
    if ((!sessionId || currentSession === sessionId) && composerGeneration === submittedGeneration) {
      setComposerText('');
      error.textContent = '';
    }
    await refresh();
    return true;
  } catch (err) {
    if ((!sessionId || currentSession === sessionId) && composerGeneration === submittedGeneration) error.textContent = err.message;
    return false;
  }
}

// Pending content belongs to its session, even while another conversation is open.
const pendingSubmissions = new Map();

function reconcileSubmissions(sessionId, ids) {
  for (const id of ids) {
    const pending = pendingSubmissions.get(id);
    if (pending?.sessionId !== sessionId) continue;
    pending.represented = true;
    if (pending.finished) pendingSubmissions.delete(id);
  }
}

function renderPendingSubmissions() {
  const container = document.querySelector('#pending-submissions');
  if (!container) return;
  const wasAtTail = atTail();
  container.replaceChildren();
  for (const pending of pendingSubmissions.values()) {
    if (pending.sessionId !== currentSession || pending.represented) continue;
    const row = el('article', 'entry tone-user');
    row.dataset.commandId = pending.id;
    row.append(el('strong', 'entry-label', 'You'));
    const body = el('div', 'entry-body');
    body.append(renderMarkdown(pending.text));
    for (const image of pending.images) {
      body.append(el('span', 'attachment-name', image.name || '[Image]'));
    }
    row.append(body, el('span', 'entry-time', pending.status));
    if (pending.failed) {
      const restore = button('Put back in composer', 'secondary', {});
      restore.addEventListener('click', () => {
        if (composerText() || promptImages.length) {
          document.querySelector('#conversation-error').textContent = 'Empty the composer before restoring this submission.';
          return;
        }
        setComposerText(pending.recoveryText ?? pending.text);
        promptImages = pending.images;
        pendingSubmissions.delete(pending.id);
        renderPendingSubmissions();
        renderAttachments();
        saveDraft();
      });
      row.append(restore);
    }
    container.append(row);
  }
  if (wasAtTail) scrollToTail();
}

async function sendSubmission(body, images = [], prerequisite = null, recoveryText = null) {
  const entropy = crypto.getRandomValues(new Uint8Array(16));
  const id = `web-${body.action}-${Array.from(entropy, byte => byte.toString(16).padStart(2, '0')).join('')}`;
  body.command_id = id;
  const pending = {
    id, sessionId: body.session_id, text: body.action === 'run-shell' ? `!${body.command}` : body.text,
    images, recoveryText, status: prerequisite ? 'Changing mode…' : 'Sending…', represented: false, finished: false,
  };
  pendingSubmissions.set(id, pending);
  renderPendingSubmissions();
  let submissionStarted = false;
  try {
    if (prerequisite) await prerequisite();
    pending.status = 'Sending…';
    renderPendingSubmissions();
    submissionStarted = true;
    await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    pending.finished = true;
    pending.status = 'Queued';
    if (pending.represented) pendingSubmissions.delete(id);
  } catch (err) {
    pending.finished = true;
    if (pending.represented) pendingSubmissions.delete(id);
    else {
      pending.failed = !submissionStarted || Boolean(err.status && err.status < 500);
      pending.status = `${pending.failed ? 'Not sent' : 'Delivery unconfirmed'}: ${err.message}`;
    }
  }
  renderPendingSubmissions();
  await refresh();
}

/// Guard against sending twice.
///
/// Enter calls submit directly, so it bypasses the disabled button entirely;
/// without this a fast double press sends the same prompt twice.
let promptInFlight = false;

async function submitPrompt() {
  if (!currentSession || promptInFlight || voiceIsActive()) return;
  const value = composerText();
  const images = promptImages;
  if (!value.trim() && !images.length) return;
  const error = document.querySelector('#conversation-error');
  if (images.some(image => image.state !== 'ready' || !image.attachment)) {
    error.textContent = 'Wait for every image to finish uploading, or remove failed images.';
    renderAttachments();
    return;
  }

  let ownsSubmitGuard = true;
  promptInFlight = true;
  sendButton.disabled = true;
  try {
    if (value.startsWith('/') && (await runLocalCommand(value.trim()))) return;

    if (value.startsWith('!') && images.length) {
      error.textContent = 'Shell commands cannot carry images.';
      return;
    }
    const body = value.startsWith('!')
      ? { action: 'run-shell', session_id: currentSession, command: value.slice(1) }
      : {
          action: 'prompt',
          session_id: currentSession,
          text: value,
          images: images.map(image => ({
            data_base64: '',
            mime_type: image.mime_type,
            width: image.width,
            height: image.height,
            attachment: image.attachment,
          })),
        };
    const payload = JSON.stringify(body);
    if (new TextEncoder().encode(payload).byteLength > MAX_PROMPT_REQUEST_BYTES) {
      error.textContent = 'Prompt attachments exceed the 32 MiB request limit.';
      return;
    }
    setComposerText('');
    clearPromptImages();
    updateCommandPalette();
    // The stored copy goes with the one on screen, so reopening does not put
    // back a prompt that has already run.
    saveDraft();
    error.textContent = '';
    promptInFlight = false;
    ownsSubmitGuard = false;
    syncSendButtonDisabled();
    await sendSubmission(body, images.map(image => ({ ...image, preview_url: null, cancelled: false })));
  } catch (err) {
    error.textContent = err.message;
  } finally {
    if (ownsSubmitGuard) promptInFlight = false;
    syncSendButtonDisabled();
    renderAttachments();
  }
}

const PROSE_ROLES = new Set(['user', 'agent', 'thought']);

// The marker the worker puts on a turn the harness ended without answering
// (#970). The row carries the prompt so it can go back in the composer without
// being retyped; it is never resent automatically, because mj cannot tell a
// prompt the harness dropped from one it acted on silently.
const PROMPT_UNANSWERED_MARKER = 'ACP prompt returned no session updates';

/// How close to the bottom still counts as reading the tail.
const TAIL_SLACK_PX = 48;

/// Whether the reader is at the tail, and so wants to be carried along.
function atTail() {
  const distance = feedScroll.scrollHeight - feedScroll.scrollTop - feedScroll.clientHeight;
  return distance <= TAIL_SLACK_PX;
}

function scrollToTail() {
  feedScroll.scrollTop = feedScroll.scrollHeight;
  jumpToLatest.classList.add('hidden');
}

function entryBody(entry) {
  const body = el('div', 'entry-body');
  if (PROSE_ROLES.has(entry.role)) {
    body.append(renderMarkdown(entry.lines.join('\n')));
  } else {
    body.append(renderToolOutput(entry.lines.join('\n')));
  }
  if (entry.diffstats?.length) {
    body.append(renderDiffStats(entry.diffstats));
  }
  return body;
}

/// The files a tool changed, from the projection's own numbers.
function renderDiffStats(diffstats) {
  const list = el('ul', 'diffstat');
  for (const stat of diffstats) {
    const item = el('li');
    item.append(el('span', 'diffstat-path', stat.path));
    item.append(el('span', 'diffstat-added', `+${stat.insertions}`));
    item.append(el('span', 'diffstat-removed', `−${stat.deletions}`));
    list.append(item);
  }
  return list;
}

function entryTimestamp(entry) {
  if (!entry.recorded_at_ms) return null;
  const node = el('time', 'entry-time', new Date(entry.recorded_at_ms).toLocaleTimeString());
  node.setAttribute('datetime', new Date(entry.recorded_at_ms).toISOString());
  return node;
}

/// Rewrite one entry's row.
///
/// Thinking and tool detail are collapsed by default, and which folds the
/// reader had opened is recorded and restored, so an update does not snap shut
/// something they were part way through reading.
function paintEntry(node, entry, unansweredPrompt) {
  const openFolds = new Set(
    [...node.querySelectorAll('details.block-fold[open] > summary')].map(
      summary => summary.textContent,
    ),
  );
  node.className = `entry tone-${entry.tone}`;
  const heading = el('strong');
  const glyph = el('span', 'entry-glyph', entry.glyph || '─');
  glyph.setAttribute('aria-hidden', 'true');
  heading.append(glyph, el('span', 'entry-label', entry.label));
  const time = entryTimestamp(entry);
  if (time) heading.append(time);

  const body = entryBody(entry);
  // Thinking is background: it is there for someone who wants it, and closed
  // for everyone else.
  if (entry.role === 'thought') {
    const fold = el('details', 'block-fold');
    const summary = el('summary', '', entry.label);
    fold.append(summary, body);
    node.replaceChildren(heading, fold);
  } else {
    node.replaceChildren(heading, body);
  }
  if (unansweredPrompt) {
    const controls = el('div', 'row');
    controls.append(
      button('Put back in composer', 'secondary', { restorePrompt: unansweredPrompt }),
    );
    node.append(controls);
  }
  for (const summary of node.querySelectorAll('details.block-fold > summary')) {
    if (openFolds.has(summary.textContent)) summary.parentElement.open = true;
  }
}

/// The prompt an unanswered-turn row offers to put back, or null.
///
/// The text comes from the newest user row before the warning, which is the
/// prompt that turn was running.
function unansweredPromptFor(entry, lastUserText) {
  if (entry.role === 'user') return null;
  const text = (entry.lines || []).join('\n');
  if (!text.includes(PROMPT_UNANSWERED_MARKER)) return null;
  return lastUserText || null;
}

function renderEntries(entries, replace) {
  if (typeof reconcileSubmissions === 'function') reconcileSubmissions(currentSession, entries.map(entry => entry.command_id).filter(Boolean));
  const wasAtTail = atTail();
  if (replace) {
    feed.replaceChildren();
    entryNodes.clear();
  }
  let appended = false;
  let lastUserText = null;
  for (const entry of entries) {
    if (entry.role === 'user') lastUserText = (entry.lines || []).join('\n');
    const unansweredPrompt = unansweredPromptFor(entry, lastUserText);
    let node = entryNodes.get(entry.id);
    if (!node) {
      node = el('article');
      node.dataset.entryId = entry.id;
      entryNodes.set(entry.id, node);
      feed.append(node);
      appended = true;
    }
    // An entry that has not moved is left alone: rewriting it would collapse
    // its folds and drop any text the reader had selected.
    if (node.dataset.updatedSeq === String(entry.updated_seq)) continue;
    node.dataset.updatedSeq = entry.updated_seq;
    paintEntry(node, entry, unansweredPrompt);
  }
  if (wasAtTail) scrollToTail();
  else if (appended) jumpToLatest.classList.remove('hidden');
  if (typeof renderPendingSubmissions === 'function') renderPendingSubmissions();
}

/// A counter that retires an in-flight request when the conversation changes.
///
/// Switching sessions quickly is how one session's text arrives under
/// another's header: the older fetch resolves last and wins. Every request
/// carries the generation it was issued in and drops itself if that generation
/// has moved on.
let conversationGeneration = 0;
let conversationInFlight = false;
let conversationPending = false;

function clearConversationContents() {
  entryNodes.clear();
  feed.replaceChildren();
  jumpToLatest.classList.add('hidden');
  elicitations.replaceChildren();
  elicitationCards.clear();
  reviewHost.replaceChildren();
  reviewSignature = null;
}

/// A lifecycle snapshot retires every transcript request issued under the
/// previous mode. This is independent of navigation: a late response from a
/// still-valid session is just as stale once its operation owns the session.
function syncConversationMode(session) {
  const transition = isTransitioningSession(session);
  const loading = !transition && isLoadingConversationSession(session);
  const operationId = session?.operation?.id || session?.state || session?.lifecycle || '';
  const next = transition ? `transition:${operationId}` : loading ? 'loading' : 'conversation';
  if (next === conversationMode) return;
  conversationMode = next;
  conversationGeneration += 1;
  conversationPending = false;
  cursor = 0;
  presentationKey = null;
  acknowledged = 0;
  conversationTransitionError.textContent = '';
  clearConversationContents();
}

function renderConversationTransition(session) {
  const transition = isTransitioningSession(session);
  const loading = !transition && isLoadingConversationSession(session);
  const unavailable = transition || loading;
  conversationTransition.hidden = !unavailable;
  feedScroll.hidden = unavailable;
  jumpToLatest.hidden = unavailable;
  elicitations.hidden = unavailable;
  reviewHost.hidden = unavailable;
  conversationSide.hidden = (
    queue.children.length === 0
    && shells.children.length === 0
    && backgroundTasks.children.length === 0
  );
  document.querySelector('#prompt-form').hidden = unavailable;
  cancelTurnButton.classList.toggle('hidden', unavailable || !session?.capabilities?.interrupt_turn);
  if (!unavailable) return;
  conversationTransitionTitle.textContent = loading ? 'Loading conversation' : session.title || session.id;
  conversationTransitionStage.textContent = loading ? 'Waiting for the conversation…' : sessionActivityLabel(session);
  const notice = loading
    ? ''
    : session.operation?.notice
      || session.configuration_issue
      || (session.has_error ? 'The operation needs recovery. Use the available action to try again.' : '');
  conversationTransitionNotice.textContent = notice;
  conversationTransitionNotice.hidden = !notice;
  conversationTransitionCancel.dataset.id = session.id;
  conversationTransitionCancel.disabled = !session.capabilities?.cancel_operation
    || pendingActions.has(`cancel:${session.id}`);
  conversationTransitionCancel.classList.toggle(
    'hidden',
    !session.capabilities?.cancel_operation,
  );
}

async function loadConversation(delta = false) {
  if (!currentSession) return;
  const current = snapshot?.sessions.find(session => session.id === currentSession);
  if (!current?.capabilities?.open || isTransitioningSession(current)) {
    if (isTransitioningSession(current) || isLoadingConversationSession(current)) {
      renderConversationTransition(current);
    }
    return;
  }
  // Revisions arrive in bursts. One load runs at a time and remembers that
  // another was asked for, so a burst costs one extra fetch rather than one
  // fetch each.
  if (conversationInFlight) {
    conversationPending = true;
    return;
  }
  conversationInFlight = true;
  const generation = conversationGeneration;
  const sessionId = currentSession;
  try {
    const query = new URLSearchParams();
    if (delta && cursor) {
      query.set('after_seq', String(cursor));
      if (presentationKey) query.set('presentation_key', presentationKey);
    }
    const suffix = query.toString() ? `?${query.toString()}` : '';
    const result = await request(
      `/api/conversations/${encodeURIComponent(sessionId)}${suffix}`,
    );
    const latest = snapshot?.sessions.find(session => session.id === sessionId);
    if (
      generation !== conversationGeneration
      || !latest?.capabilities?.open
      || isTransitioningSession(latest)
    ) return;
    renderEntries(result.entries, !delta || result.reset);
    presentationKey = typeof result.presentation_key === 'string'
      ? result.presentation_key
      : null;
    cursor = result.latest_seq;
    if (cursor > acknowledged) {
      const through = cursor;
      await request(`/api/conversations/${encodeURIComponent(sessionId)}/read`, {
        method: 'POST',
        body: JSON.stringify({ through }),
      });
      const latest = snapshot?.sessions.find(session => session.id === sessionId);
      if (
        generation !== conversationGeneration
        || !latest?.capabilities?.open
        || isTransitioningSession(latest)
      ) return;
      acknowledged = through;
    }
  } catch (err) {
    const latest = snapshot?.sessions.find(session => session.id === sessionId);
    if (
      generation !== conversationGeneration
      || !latest?.capabilities?.open
      || isTransitioningSession(latest)
    ) return;
    if (err.message === 'unauthorized') {
      showLogin();
      return;
    }
    document.querySelector('#conversation-error').textContent = err.message;
  } finally {
    conversationInFlight = false;
    if (conversationPending) {
      conversationPending = false;
      const latest = snapshot?.sessions.find(session => session.id === sessionId);
      if (
        currentSession === sessionId
        && latest?.capabilities?.open
        && !isTransitioningSession(latest)
      ) {
        loadConversation(generation === conversationGeneration);
      }
    }
  }
}

async function openConversation(id) {
  if (currentSession === id) return;
  cancelVoiceInput();
  const session = snapshot?.sessions.find(x => x.id === id);
  if (!session || (!session.capabilities?.open && !isTransitioningSession(session))) return;
  currentSession = id;
  conversationGeneration += 1;
  conversationMode = null;
  conversationPending = false;
  cursor = 0;
  presentationKey = null;
  acknowledged = 0;
  clearConversationContents();
  document.querySelector('#conversation-title').textContent = session.title;
  document.querySelector('#conversation-state').textContent = sessionLifecycleLabel(session);
  syncConversationMode(session);
  renderQueue(session);
  renderElicitations(session);
  renderTurnReview(session);
  renderConversationHeader(session);
  clearPromptImages();
  restoreDraft(id, conversationGeneration);
  if (!isTransitioningSession(session)) await loadConversation(false);
}

/// The header, the turn control and the composer, all from what the daemon
/// published about this session.
///
/// The placeholder says whether Send will send or queue, because a person
/// pressing it deserves to know which of those is about to happen.
function renderSessionTitle(node, session) {
  node.textContent = session.title;
  node.classList.toggle('idle-title', session.is_idle === true);
}

/// Show only the settings the live session currently has, using the harness's
/// human-readable choice name when one matches the raw current value. This is
/// deliberately a readout rather than a control: changing settings remains a
/// separate, explicit action and a snapshot refresh can remove it entirely.
function renderPromptSettings(session) {
  const settings = ['model', 'effort'].flatMap(key => {
    const option = session?.config_options?.find(item => item.key === key);
    const current = option?.current;
    if (current === null || current === undefined || current === '') return [];
    const rawValue = String(current);
    const choice = option.choices?.find(item => String(item.value) === rawValue);
    return [{ label: key === 'model' ? 'Model' : 'Effort', value: choice?.name || rawValue }];
  });

  promptSettings.replaceChildren(
    ...settings.map(setting => {
      const item = el('span', 'prompt-setting');
      item.append(
        el('span', 'prompt-setting-label', `${setting.label}:`),
        el('span', 'prompt-setting-value', setting.value),
      );
      return item;
    }),
  );
  promptSettings.classList.toggle('hidden', settings.length === 0);
}

// Operation delivery survives reconnect in the server snapshot. Local state only
// prevents duplicate clicks before the server accepts a request.
const pendingTurnControls = new Set();
const dismissedTurnControls = new Map();
const failedTurnControls = new Map();

function turnControlState(session) {
  const steering = session?.steering;
  const pending = Boolean(session?.cancelling_prompt_id) || steering?.status === 'pending';
  const uncertain = steering?.status === 'unconfirmed';
  const failed = steering?.status === 'failed';
  const targeted = session?.targeted_turn_control_supported === true;
  const queued = targeted && (session?.queued_prompts || [])[0];
  return {
    pending, uncertain, failed,
    label: session?.cancelling_prompt_id ? 'Interrupting turn…' : steering?.status === 'pending' ? 'Steering…'
      : uncertain ? 'Delivery unconfirmed' : queued ? 'Steer queued prompt' : 'Interrupt turn',
    command: targeted && session?.active_prompt_id ? queued
      ? { type: 'steer', data: { active_prompt_id: session.active_prompt_id, queued_prompt_id: queued.id || queued.command_id } }
      : { type: 'cancel_turn_for', data: { active_prompt_id: session.active_prompt_id } } : null,
  };
}

async function submitTurnControl(session, command) {
  if (!session || pendingTurnControls.has(session.id)) return;
  pendingTurnControls.add(session.id);
  failedTurnControls.delete(session.id);
  dismissedTurnControls.delete(session.id);
  renderTurnControl(session);
  try {
    await request('/api/actions', { method: 'POST', body: JSON.stringify({ action: 'turn-control', session_id: session.id, command }) });
    await refresh();
  } catch (error) {
    failedTurnControls.set(session.id, { message: error.message, activePromptId: command.data?.active_prompt_id });
  } finally {
    pendingTurnControls.delete(session.id);
    if (currentSession === session.id) renderTurnControl(snapshot.sessions.find(s => s.id === session.id) || session);
  }
}

function renderTurnControl(session) {
  const control = turnControlState(session);
  cancelTurnButton.textContent = control.label;
  cancelTurnButton.disabled = control.pending || control.uncertain || pendingTurnControls.has(session.id);
  let panel = document.querySelector('#turn-control-status');
  if (!panel) {
    panel = el('div', 'turn-control-status'); panel.id = 'turn-control-status';
    panel.setAttribute('role', 'status'); cancelTurnButton.parentElement.append(panel);
  }
  panel.replaceChildren();
  const signature = `${session.steering?.command_id}:${session.steering?.status}:${session.active_prompt_id || ''}`;
  const localError = failedTurnControls.get(session.id);
  if (!(control.uncertain || control.failed || localError) || dismissedTurnControls.get(session.id) === signature) return;
  const message = session.steering?.message || localError?.message || 'Steering failed; the prompt remains queued.';
  panel.append(el('p', '', message + (control.uncertain ? ' Delivery may already have occurred; retrying can deliver it twice.' : '')));
  const keep = button('Keep queued');
  keep.onclick = () => { dismissedTurnControls.set(session.id, signature); panel.replaceChildren(); };
  panel.append(keep);
  if (session.active_prompt_id && !session.cancelling_prompt_id && (localError ? localError.activePromptId === session.active_prompt_id : session.steering?.active_prompt_id === session.active_prompt_id)) {
    const cancel = button(control.uncertain ? 'Cancel turn' : 'Cancel turn and apply queued prompt');
    cancel.onclick = () => submitTurnControl(session, { type: 'cancel_turn_for', data: { active_prompt_id: session.active_prompt_id } });
    panel.append(cancel);
  } else if (control.uncertain && !session.active_prompt_id) {
    const retry = button('Retry queued prompt');
    retry.onclick = () => submitTurnControl(session, { type: 'resolve_steering', data: { steering_id: session.steering.command_id } });
    const remove = button('Remove queued prompt');
    remove.onclick = () => sendAction({ action: 'remove-queued-prompt', session_id: session.id, queue_id: session.steering.queued_prompt_id });
    panel.append(retry, remove);
  }
}

function renderConversationHeader(session) {
  syncConversationMode(session);
  renderSessionTitle(document.querySelector('#conversation-title'), session);
  renderPromptSettings(session);
  const children = session?.subagent_session_ids || [];
  const native = session?.native_subagents || [];
  const working = children.filter(id => snapshot.sessions.some(s => s.id === id && s.chat_phase === 'running')).length
    + new Set(native.filter(a => a.state === 'running').map(a => a.stable_id || a.session_id)).size;
  subagentsButton.textContent = `Subagents · ${working} working`;
  subagentsButton.title = `${children.length + native.length} retained agents`;
  subagentsButton.classList.toggle(
    'hidden',
    children.length + native.length === 0 || Boolean(route.subagentParentId),
  );
  const state = document.querySelector('#conversation-state');
  state.textContent = sessionLifecycleLabel(session);
  state.className = `pill state-${session.lifecycle}`;
  renderConversationTransition(session);
  cancelTurnButton.classList.toggle(
    'hidden',
    isTransitioningSession(session)
      || isLoadingConversationSession(session)
      || !session.capabilities?.interrupt_turn,
  );

  renderTurnControl(session);
  const running = session.chat_phase === 'running';
  const queued = (session.queued_prompts || []).length;
  promptText.dataset.placeholder = running
    ? 'The agent is working; this will queue'
    : 'Message the agent or use !command';
  sendButton.textContent = running || queued ? 'Queue' : 'Send';
  // A review holds the turn it reviewed: the daemon refuses prompts for this
  // session until it resolves, so the composer says so rather than letting a
  // person type into a refusal.
  const reviewing = Boolean(session.turn_review);
  if (reviewing) {
    promptText.dataset.placeholder =
      'A review of the last turn is open \u2014 forward, dismiss or cancel it';
  }
  const canPrompt = session.capabilities?.prompt !== false && !reviewing;
  promptText.setAttribute('contenteditable', String(canPrompt));
  voiceInput.disabled =
    !canPrompt
    || !currentSession
    || voiceState === 'permission'
    || voiceState === 'transcribing';
  syncSendButtonDisabled();
  if (
    voiceIsActive()
    && (!canPrompt || isTransitioningSession(session) || isLoadingConversationSession(session))
  ) {
    cancelVoiceInput();
  }
  if (session.plan_mode_active) {
    state.textContent = `${sessionLifecycleLabel(session)} · plan`;
  }
}

subagentsButton.onclick = () => {
  const session = activeSession();
  if (!session?.subagent_session_ids?.length && !session?.native_subagents?.length) return;
  navigate({ name: 'dashboard', subagentParentId: session.id });
};

/// Drop everything the conversation view was holding.
///
/// Leaving has to clear the keyed nodes and the pending elicitation cards, or
/// the next conversation opens on top of the last one's rows.
function leaveConversation() {
  cancelVoiceInput();
  currentSession = null;
  conversationMode = null;
  conversationGeneration += 1;
  conversationPending = false;
  cursor = 0;
  presentationKey = null;
  acknowledged = 0;
  clearConversationContents();
  clearPromptImages();
}

document.querySelector('#login-form').onsubmit = async e => {
  e.preventDefault();
  try {
    await request('/auth/session', {
      method: 'POST',
      body: JSON.stringify({ code: document.querySelector('#code').value }),
    });
    document.querySelector('#login-error').textContent = '';
    await restoreRoute();
  } catch (err) {
    document.querySelector('#login-error').textContent = err.message;
  }
};
// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

function closeMenu() {
  menu.classList.add('hidden');
  menuButton.setAttribute('aria-expanded', 'false');
}

/// Names the controller build this viewer is talking to, under the menu's
/// actions. A controller too old to publish its version leaves the label as
/// the product name rather than showing a blank or a guess.
function renderMenuVersion() {
  menuVersion.textContent = snapshot?.server_version
    ? `Mjolnir v${snapshot.server_version}`
    : 'Mjolnir';
}

menuButton.onclick = () => {
  const open = menu.classList.toggle('hidden');
  menuButton.setAttribute('aria-expanded', String(!open));
};

// A tap outside the menu closes it, and so does Escape. Both are capture-phase
// so a control inside the menu still receives its own click first.
document.addEventListener('pointerdown', event => {
  if (activeSessionPress && activeSessionPress.pointerId !== event.pointerId) cancelSessionPress();
  if (!menu.classList.contains('hidden') && !menu.contains(event.target) && !menuButton.contains(event.target)) {
    closeMenu();
  }
  if (openSessionMenuId) {
    const card = sessionCards.get(openSessionMenuId);
    if (!card?.contains(event.target)) closeSessionMenu();
  }
});
document.addEventListener('keydown', event => {
  if (event.key === 'Escape') {
    closeMenu();
    closeSessionMenu();
  }
});

menu.onclick = event => {
  const target = event.target.closest('button[data-route]');
  if (!target) return;
  closeMenu();
  navigate({ name: target.dataset.route });
};

logout.onclick = async () => {
  cancelVoiceInput();
  await request('/auth/session', { method: 'DELETE' });
  location.hash = '';
  location.reload();
};

backButton.onclick = () => {
  if (route.name === 'resume' && route.sessionId) {
    navigate({ name: 'resume', workspaceId: route.workspaceId });
    return;
  }
  if (route.subagentParentId) {
    navigate({ name: 'dashboard', subagentParentId: route.subagentParentId });
    return;
  }
  navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
};

resumeDetailBack.onclick = () => {
  navigate({ name: 'resume', workspaceId: route.workspaceId || selectedWorkspaceId() });
};

resumeSearch.oninput = () => {
  const state = resumeListState(route.workspaceId || selectedWorkspaceId());
  state.query = resumeSearch.value;
  // The index is the only search path: it is asked once the typing pauses
  // and answers in the background, and the list is drawn from its answer.
  scheduleWikiSearch(state.query);
  renderResumable();
};

workspaceStrip.onclick = event => {
  const tab = event.target.closest('button[data-workspace-id]');
  if (!tab) return;
  navigate({ name: 'dashboard', workspaceId: tab.dataset.workspaceId });
};

for (const node of document.querySelectorAll(
  '.page-actions button[data-route], #new-form button[data-route]',
)) {
  node.onclick = event => {
    event.preventDefault();
    navigate({ name: node.dataset.route, workspaceId: selectedWorkspaceId() });
  };
}

window.addEventListener('hashchange', applyRoute);

for (const panel of [targetsPanel, quotaPanel]) {
  panel.onclick = async event => {
    const target = event.target.closest('button[data-refresh]');
    if (target) await runRefresh(target, target.closest('.quota-profile')?.querySelector('.quota-error'));
  };
}

newBackButton.onclick = () => {
  if (!newDraft || newDraft.step === 0) return;
  abortPendingNewPreflight();
  newDraft.step -= 1;
  newError.textContent = '';
  renderNewForm();
};

newForm.onsubmit = async event => {
  event.preventDefault();
  try {
    if (document.activeElement?.id === 'new-bundle-source') {
      await createNewBundle();
      return;
    }
    await advanceNew();
  } catch (err) {
    newError.textContent = err.message;
  }
};

moveBackButton.onclick = () => {
  if (!moveDraft) return;
  if (moveDraft.preparation) {
    moveDraft.preparation = null;
    moveDraft.acknowledge = false;
    moveDraft.queue = 'discard';
    moveError.textContent = '';
    renderMoveForm();
  } else {
    navigate({ name: 'dashboard', workspaceId: moveDraft.workspaceId });
  }
};

moveForm.onsubmit = async event => {
  event.preventDefault();
  await advanceMove();
};

conversationSide.onclick = async event => {
  const target = event.target.closest('button[data-background-task-id]');
  if (!target) return;
  event.preventDefault();
  await stopBackgroundTask(target.dataset.backgroundTaskId);
};

/// One session action, from the row that carries it.
///
/// The pending set is checked at entry and released in a `finally`, so a
/// double tap cannot send twice and a failure cannot leave the control dead.
async function runSessionAction(dataset, errorNode, extra) {
  const key = `${dataset.action}:${dataset.id}`;
  if (pendingActions.has(key)) return false;
  const actionExtra = { ...(extra || {}) };
  delete actionExtra.workspace_id;
  delete actionExtra.isCurrent;
  if (dataset.action === 'repair-config') {
    const session = snapshot?.sessions.find(item => item.id === dataset.id);
    errorNode.textContent = session?.configuration_issue || 'Configuration is repaired. Retry opening the session.';
    return true;
  }
  if (dataset.action === 'open') {
    navigate({ name: 'conversation', sessionId: dataset.id });
    return true;
  }
  if (dataset.action === 'move') {
    const session = snapshot.sessions.find(item => item.id === dataset.id);
    if (!session) return false;
    closeSessionMenu();
    navigate({
      name: 'move',
      workspaceId: session.workspace_id || selectedWorkspaceId(),
      sessionId: session.id,
    });
    return true;
  }
  if (dataset.action === 'suspend') {
    const session = snapshot.sessions.find(item => item.id === dataset.id);
    const activeChildren = (session?.subagent_session_ids || [])
      .map(id => snapshot.sessions.find(item => item.id === id))
      .filter(child => child && ['live', 'starting', 'suspending'].includes(child.lifecycle));
    const question = 'Suspend session?\n\nSave a recovery copy and release the environment. You can resume this session later.'
      + (session?.chat_phase === 'running' ? '\n\nThe current turn will be interrupted.' : '')
      + (activeChildren.length ? `\n\nThis also suspends ${activeChildren.length} active sub-agent(s) first.` : '');
    if (!confirm(question)) return false;
  }
  if (dataset.action === 'destroy') {
    const session = snapshot.sessions.find(item => item.id === dataset.id);
    const choice = await confirmSessionDestruction(session);
    if (choice === null) return false;
    extra = { ...extra, delete_branch: choice };
  }
  const body = { action: dataset.action, session_id: dataset.id, ...extra };
  if (dataset.action === 'rename') {
    const session = snapshot.sessions.find(x => x.id === dataset.id);
    const title = prompt('New session name', session?.title || '');
    if (title === null || !title.trim()) return;
    body.title = title.trim();
  }
  if (dataset.action === 'resume') {
    // The resume page asks these as labelled controls; a row elsewhere falls
    // back to what the session last used.
    body.profile_id = extra?.profile_id || dataset.profile;
    body.target_id = extra?.target_id || dataset.target;
    body.workspace_id = extra?.workspace_id || dataset.workspace_id || selectedWorkspaceId();
    body.queue = extra?.queue || 'start';
    if (extra && Object.prototype.hasOwnProperty.call(extra, 'additional_mounts')) {
      body.additional_mounts = extra.additional_mounts;
      body.resource_allocation = extra.resource_allocation ?? null;
    }
  }
  pendingActions.add(key);
  const lifecycle = ['suspend', 'destroy'].includes(dataset.action);
  if (lifecycle) {
    const session = snapshot.sessions.find(item => item.id === dataset.id);
    pendingLifecycleActions.set(key, { id: dataset.id, action: dataset.action, previousError: session?.launch_error, observed: false });
  }
  renderRoute();
  try {
    if (lifecycle) {
      await request(`/api/v1/sessions/${dataset.id}/${dataset.action}`, {
        method: 'POST', body: JSON.stringify(dataset.action === 'suspend'
          ? { acknowledge_active_subagents: true } : { delete_branch: extra.delete_branch }),
      });
    } else {
      await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    }
    errorNode.textContent = '';
    await refresh();
    return true;
  } catch (err) {
    pendingLifecycleActions.delete(key);
    errorNode.textContent = err.message;
    return false;
  } finally {
    if (!pendingLifecycleActions.has(key)) pendingActions.delete(key);
    renderRoute();
  }
}

sessions.onclick = async e => {
  const menuTrigger = e.target.closest('button[data-session-menu]');
  if (menuTrigger) {
    e.preventDefault();
    e.stopPropagation?.();
    openSessionMenu(menuTrigger.dataset.sessionMenu, menuTrigger, true);
    return;
  }
  const target = e.target.closest('button[data-action]');
  if (target) {
    closeSessionMenu();
    await runSessionAction(target.dataset, actionError);
    return;
  }
  openSessionCard(e);
};

sessions.onkeydown = handleSessionCardKeydown;
sessions.addEventListener('keydown', handleSessionMenuKeydown);
sessions.addEventListener('pointerdown', beginSessionPress);
sessions.addEventListener('pointerup', cancelSessionPress);
sessions.addEventListener('pointercancel', cancelSessionPress);
sessions.addEventListener('scroll', cancelSessionPress, { passive: true });
document.addEventListener('scroll', cancelSessionPress, { capture: true, passive: true });
document.addEventListener('pointermove', moveSessionPress, { capture: true });
document.addEventListener('pointerup', cancelSessionPress, { capture: true });
document.addEventListener('pointercancel', cancelSessionPress, { capture: true });
sessions.addEventListener('contextmenu', event => {
  const card = sessionCardFromTarget(event.target);
  if (!card || !card._session || !sessionMenuActions(card._session).length) return;
  event.preventDefault();
  openSessionMenu(card.dataset.sessionId, card._menuTrigger);
});

resumeDetail.onclick = async e => {
  const target = e.target.closest('button[data-action]');
  if (!target) return;
  if (target.dataset.action === 'wiki-restore') {
    await restoreWikiSession(target.dataset.id);
    return;
  }
  const session = snapshot?.sessions.find(item =>
    item.id === target.dataset.id && item.workspace_id === route.workspaceId);
  if (!session) return;
  if (target.dataset.action === 'move') {
    navigate({ name: 'move', workspaceId: session.workspace_id, sessionId: session.id });
    return;
  }
  const draft = resumeDraft(session);
  const recovery = session.move_recovery;
  const workspaceId = session.workspace_id;
  const visit = resumeRouteVisit;
  draft.error = '';
  // Keep failures with their session even if the user has left or a snapshot
  // rebuilt the card while the request was pending.
  const errorSink = { set textContent(value) { draft.error = value; } };
  const success = await runSessionAction(target.dataset, errorSink, {
    workspace_id: workspaceId,
    target_id: draft.targetId,
    profile_id: draft.profileId,
    queue: draft.queue,
    ...(recovery ? {
      additional_mounts: recovery.source_additional_mounts || [],
      resource_allocation: recovery.source_resource_allocation ?? null,
    } : {}),
  });
  if (success
    && target.dataset.action === 'resume'
    && route.name === 'resume'
    && route.sessionId === session.id
    && route.workspaceId === workspaceId
    && resumeRouteVisit === visit) {
    navigate({ name: 'dashboard', workspaceId });
  }
};

document.querySelector('#prompt-form').onsubmit = e => {
  e.preventDefault();
  submitPrompt();
};
voiceInput.onclick = () => {
  if (voiceState === 'recording') stopVoiceRecording();
  else if (voiceState === 'idle') startVoiceInput();
};
voiceCancel.onclick = () => cancelVoiceInput();
promptText.addEventListener('input', () => {
  composerGeneration += 1;
  composerInputChanged();
  if (historyOpen) {
    searchHistory(composerText());
    return;
  }
  updateCommandPalette();
  scheduleDraftSave();
});

// Ctrl-R opens the reverse lookup, the way the terminal's history search does.
promptText.addEventListener('keydown', event => {
  if ((event.ctrlKey || event.metaKey) && event.key === 'r') {
    event.preventDefault();
    historyOpen = !historyOpen;
    if (historyOpen) searchHistory(composerText());
    else updateCommandPalette();
  }
});

commandPalette.onclick = event => {
  const row = event.target.closest('button[data-insert]');
  if (!row) return;
  setComposerText(row.dataset.insert);
  placeComposerCaretAtEnd();
  promptText.focus();
  updateCommandPalette();
};

feed.onclick = event => {
  const restore = event.target.closest('button[data-restore-prompt]');
  if (!restore) return;
  setComposerText(restore.dataset.restorePrompt);
  placeComposerCaretAtEnd();
  promptText.focus();
  updateCommandPalette();
};

jumpToLatest.onclick = scrollToTail;
feedScroll.addEventListener('scroll', () => {
  if (atTail()) jumpToLatest.classList.add('hidden');
});

cancelTurnButton.onclick = async () => {
  const session = snapshot.sessions.find(s => s.id === currentSession);
  const control = turnControlState(session);
  if (control.pending || control.uncertain) return;
  if (control.command) await submitTurnControl(session, control.command);
  else await sendAction({ action: 'interrupt-turn', session_id: currentSession });
};
conversationTransitionCancel.onclick = async () => {
  const id = conversationTransitionCancel.dataset.id || currentSession;
  if (!id) return;
  conversationTransitionCancel.disabled = true;
  try {
    await runSessionAction(
      { action: 'cancel', id },
      conversationTransitionError,
    );
  } finally {
    const session = snapshot?.sessions.find(item => item.id === id);
    if (session && currentSession === id) renderConversationHeader(session);
  }
};
// Rich text, and anything a paste or drop would inject as markup, never
// belongs in a prompt: refuse it here and re-insert the plain text instead.
promptText.addEventListener('beforeinput', e => {
  const kind = e.inputType || '';
  if (
    kind === 'insertHTML' ||
    kind.startsWith('insertFromDrop') ||
    kind.startsWith('insertFromPaste') ||
    kind.startsWith('format')
  )
    e.preventDefault();
});
promptText.addEventListener('paste', e => {
  const files = Array.from(e.clipboardData?.items || [])
    .filter(item => item.kind === 'file' && item.type.startsWith('image/'))
    .map(item => item.getAsFile())
    .filter(Boolean);
  if (files.length) {
    e.preventDefault();
    const session = snapshot?.sessions.find(x => x.id === currentSession);
    if (session?.prompt_images_supported) attachImageFiles(files);
    else
      document.querySelector('#conversation-error').textContent =
        'This session does not support image prompts.';
    return;
  }
  const text = e.clipboardData?.getData('text/plain');
  if (text === undefined) return;
  e.preventDefault();
  insertComposerText(text);
});
promptText.addEventListener('dragover', e => {
  e.preventDefault();
  const types = Array.from(e.dataTransfer?.types || []);
  if (e.dataTransfer)
    e.dataTransfer.dropEffect = types.some(type => type === 'text/plain' || type === 'Files')
      ? 'copy'
      : 'none';
});
promptText.addEventListener('drop', e => {
  e.preventDefault();
  placeComposerCaretAtPoint(e.clientX, e.clientY);
  const files = Array.from(e.dataTransfer?.files || []).filter(file =>
    file.type.startsWith('image/'),
  );
  if (files.length) {
    const session = snapshot?.sessions.find(x => x.id === currentSession);
    if (session?.prompt_images_supported) attachImageFiles(files);
    else
      document.querySelector('#conversation-error').textContent =
        'This session does not support image prompts.';
    return;
  }
  const text = e.dataTransfer?.getData('text/plain') || '';
  if (text) insertComposerText(text);
});
// An active IME composition steers its candidate with Enter and the arrows,
// so the composer must not read those keys until the composition ends.
promptText.addEventListener('keydown', e => {
  if (e.isComposing || e.keyCode === 229) return;
  // The palette owns the arrows, Tab and Enter while it is open, and gives
  // them back the moment it closes.
  if (paletteMatches.length) {
    if (e.key === 'ArrowDown' && moveCommandSelection(1)) return e.preventDefault();
    if (e.key === 'ArrowUp' && moveCommandSelection(-1)) return e.preventDefault();
    if ((e.key === 'Tab' || e.key === 'Enter') && !e.shiftKey && acceptCommandSelection()) {
      return e.preventDefault();
    }
    if (e.key === 'Escape') {
      paletteMatches = [];
      commandPalette.classList.add('hidden');
      return e.preventDefault();
    }
  }
  if (e.key === 'Escape') {
    e.preventDefault();
    if (e.repeat) return;
    const panel = document.querySelector('#turn-control-status');
    const keep = panel?.querySelector('button');
    if (keep) keep.click();
    else if (!cancelTurnButton.disabled && !cancelTurnButton.classList.contains('hidden')) cancelTurnButton.click();
    return;
  }
  if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
    e.preventDefault();
    submitPrompt();
    return;
  }
  if (e.key === 'Enter' && e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
    e.preventDefault();
    insertComposerLineBreak();
    return;
  }
  if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
    e.preventDefault();
    submitPrompt();
  }
});
attachImage.onclick = () => imagePicker.click();
imagePicker.onchange = () => {
  const files = Array.from(imagePicker.files || []);
  imagePicker.value = '';
  attachImageFiles(files);
};
queue.onclick = async e => {
  const edit = e.target.closest('button[data-edit-queue-id]');
  const remove = e.target.closest('button[data-queue-id]');
  const target = edit || remove;
  if (!target) return;
  const id = edit ? edit.dataset.editQueueId : remove.dataset.queueId;
  const session = activeSession();
  const queued = session?.queued_prompts?.find(prompt => prompt.id === id);
  const error = document.querySelector('#conversation-error');
  try {
    await request('/api/actions', {
      method: 'POST',
      body: JSON.stringify({
        action: 'remove-queued-prompt',
        session_id: currentSession,
        queue_id: id,
      }),
    });
    if (edit && queued) {
      setComposerText(queued.text);
      placeComposerCaretAtEnd();
      promptText.focus();
      updateCommandPalette();
    }
    error.textContent = '';
    await refresh();
  } catch (err) {
    // A removal that failed leaves the prompt queued, so the composer must not
    // be filled with a copy of something that is still going to run.
    error.textContent = err.message;
  }
};

shells.onclick = async e => {
  const button = e.target.closest('button[data-shell-id]');
  if (!button) return;
  try {
    await request('/api/actions', {
      method: 'POST',
      body: JSON.stringify({
        action: 'cancel-shell',
        session_id: currentSession,
        shell_command_id: button.dataset.shellId,
      }),
    });
    await refresh();
  } catch (err) {
    document.querySelector('#conversation-error').textContent = err.message;
  }
};
// ---------------------------------------------------------------------------
// Keyboard inset
// ---------------------------------------------------------------------------
//
// How much of the window the on-screen keyboard is covering, as a custom
// property the layout reads. The `offsetTop` term is the one naive versions
// miss: on iOS the visual viewport scrolls within the layout viewport, and
// without it the composer drifts by exactly that offset.
function syncKeyboardInset() {
  const viewport = window.visualViewport;
  const inset = viewport
    ? Math.max(0, window.innerHeight - viewport.height - viewport.offsetTop)
    : 0;
  document.documentElement.style.setProperty('--keyboard-inset', `${Math.round(inset)}px`);
}

if (window.visualViewport) {
  window.visualViewport.addEventListener('resize', syncKeyboardInset);
  window.visualViewport.addEventListener('scroll', syncKeyboardInset);
}
window.addEventListener('resize', syncKeyboardInset);
syncKeyboardInset();
document.body.dataset.connection = navigator.onLine ? 'online' : 'offline';

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// What the viewer believes about its link to the daemon.
let connection = 'online';

function setConnection(next) {
  if (connection === next) return;
  connection = next;
  document.body.dataset.connection = next;
  if (next === 'offline') announce('Offline. Showing the last state received.');
  if (next === 'reconnecting') announce('Reconnecting.');
  if (next === 'online') announce('Connected.');
}

function reconnect() {
  setConnection('reconnecting');
  startEvents();
  // A reconnect reconciles by full snapshot rather than assuming the deltas
  // missed while offline line up with the cursor.
  cursor = 0;
  presentationKey = null;
  refresh().then(ok => {
    if (ok) setConnection('online');
    const session = snapshot?.sessions.find(item => item.id === currentSession);
    if (ok && session?.capabilities?.open && !isTransitioningSession(session)) {
      loadConversation(false);
    }
  });
}

window.addEventListener('online', reconnect);
window.addEventListener('offline', () => setConnection('offline'));
window.addEventListener('pagehide', () => cancelVoiceInput());
window.addEventListener('beforeunload', () => cancelVoiceInput());

// A backgrounded progressive web app gets no `online` event, so the first
// signal that it is back is somebody unlocking the screen.
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && navigator.onLine) reconnect();
});
// Clocks are presentation-only updates.  The keyed card nodes remain mounted
// so focus, an open menu, and an in-progress pointer gesture survive each
// tick.
window.setInterval(updateSessionClocks, 1000);
if ('serviceWorker' in navigator) {
  // A registration that fails means the application is not installable, and
  // nothing more. Left uncaught it is an unhandled rejection, which is exactly
  // the page error the reliability suite refuses to see.
  navigator.serviceWorker.register('/service-worker.js').catch(() => {});
}
restoreRoute();
