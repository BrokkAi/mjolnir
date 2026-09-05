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
  announcer = document.querySelector('#announcer'),
  workspaceStrip = document.querySelector('#workspaces'),
  sessions = document.querySelector('#sessions'),
  resumable = document.querySelector('#resumable'),
  targetsPanel = document.querySelector('#targets'),
  quotaPanel = document.querySelector('#quota'),
  logout = document.querySelector('#logout'),
  newForm = document.querySelector('#new-form'),
  newStep = document.querySelector('#new-step'),
  newProgress = document.querySelector('#new-progress'),
  newBackButton = document.querySelector('#new-back'),
  newNextButton = document.querySelector('#new-next'),
  newError = document.querySelector('#new-error'),
  actionError = document.querySelector('#action-error'),
  resumeError = document.querySelector('#resume-error'),
  feed = document.querySelector('#conversation-feed'),
  feedScroll = document.querySelector('#conversation-scroll'),
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
  elicitations = document.querySelector('#elicitations'),
  reviewHost = document.querySelector('#turn-review'),
  promptText = document.querySelector('#prompt-text'),
  attachments = document.querySelector('#attachments'),
  attachImage = document.querySelector('#attach-image'),
  imagePicker = document.querySelector('#image-picker');

/// Every page, by the route name that shows it.
const PAGES = {
  dashboard: document.querySelector('#dashboard'),
  new: document.querySelector('#new-page'),
  resume: document.querySelector('#resume-page'),
  targets: document.querySelector('#targets-page'),
  quota: document.querySelector('#quota-page'),
  conversation: document.querySelector('#conversation'),
};

/// Transcript nodes by entry id, so an update patches the row it belongs to
/// rather than searching the whole document for it.
const entryNodes = new Map();
let snapshot,
  route = { name: 'dashboard' },
  currentSession,
  cursor = 0,
  acknowledged = 0,
  eventSource;

/// Actions the browser has asked for and not yet heard back about.
///
/// A control is disabled because it is in this set, not because a handler
/// disabled it: state decides, so a re-render cannot lose the fact and a
/// failure cannot leave a button dead.
const pendingActions = new Set();

async function request(url, options = {}) {
  const response = await fetch(url, {
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
    throw new Error(body.error || response.statusText);
  }
  if (response.status === 202 || response.status === 204) return null;
  return response.json();
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
  [new RegExp(`^#workspace/(${ID})/new$`), ([id]) => ({ name: 'new', workspaceId: id })],
  [new RegExp(`^#workspace/(${ID})/resume$`), ([id]) => ({ name: 'resume', workspaceId: id })],
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
  switch (next.name) {
    case 'new':
      return `#workspace/${next.workspaceId}/new`;
    case 'resume':
      return `#workspace/${next.workspaceId}/resume`;
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
  route = parseRoute(location.hash);
  if (!snapshot) return;

  // The dashboard names its workspace in the URL, so a reload, a Back press
  // and a shared link all return to the same one. An empty hash is the state
  // a first visit is in, and canonicalising it here is what gives every later
  // navigation something to go back to.
  if (route.name === 'dashboard' && !route.workspaceId) {
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
    if (!session?.capabilities?.open) {
      navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
      return;
    }
  }

  const name = PAGES[route.name] ? route.name : 'dashboard';
  for (const [key, page] of Object.entries(PAGES)) page.classList.toggle('hidden', key !== name);
  workspaceStrip.classList.toggle('hidden', name === 'conversation');
  backButton.classList.toggle('hidden', name === 'dashboard');
  shellTitle.textContent =
    {
      new: 'New session',
      resume: 'Resume',
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
  if (name !== 'new') newDraft = null;
  renderRoute();
  // A screen reader should land at the top of the page it just moved to
  // rather than wherever it happened to be.
  PAGES[name].setAttribute('tabindex', '-1');
  PAGES[name].focus({ preventScroll: true });
  announce(shellTitle.textContent);
}

function renderRoute() {
  if (!snapshot) return;
  renderWorkspaces();
  renderLaunchFailures();
  switch (route.name) {
    case 'new':
      renderNewForm();
      break;
    case 'resume':
      renderResumable();
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
  document.querySelector('#launch-failures').replaceChildren(...notices.map(failure => {
    const card = el('div', 'card');
    card.append(el('p', '', 'A session could not be started. Check the project and target, then retry. Details are in the daemon logs.'));
    const dismiss = el('button', 'secondary', 'Dismiss launch error');
    dismiss.onclick = () => {
      dismissedLaunchFailures.add(failure.id);
      renderLaunchFailures();
    };
    card.append(dismiss);
    return card;
  }));
}

function renderWorkspaces() {
  const selected = selectedWorkspaceId();
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

/// The state word and its icon. Colour alone never says what a session is
/// doing, because colour is the one channel a reader may not have.
const LIFECYCLE_ICON = {
  live: '●',
  starting: '◐',
  stopping: '◑',
  stopped: '○',
  failed: '×',
};

function liveSessions() {
  const workspaceId = selectedWorkspaceId();
  return (snapshot.sessions || []).filter(
    session =>
      session.workspace_id === workspaceId &&
      ['live', 'starting', 'stopping'].includes(session.lifecycle),
  );
}

/// Sessions grouped by project, in the order the projects first appear.
function byProject(list) {
  const groups = new Map();
  for (const session of list) {
    const key = session.project_key || session.bundle_id;
    if (!groups.has(key)) groups.set(key, { label: session.project_label || key, sessions: [] });
    groups.get(key).sessions.push(session);
  }
  return [...groups.values()];
}

function renderSessions() {
  const groups = byProject(liveSessions());
  if (!groups.length) {
    sessions.replaceChildren(el('p', 'dim', 'No live sessions in this workspace.'));
    return;
  }
  sessions.replaceChildren(
    ...groups.map(group => {
      const section = el('section', 'project');
      const heading = el('h2', 'project-heading');
      heading.append(el('span', '', group.label), el('span', 'dim', ` ${group.sessions.length}`));
      section.append(heading);
      const list = el('div', 'project-sessions');
      list.setAttribute('role', 'list');
      for (const session of group.sessions) {
        const row = sessionCard(session);
        const item = el('div');
        item.setAttribute('role', 'listitem');
        item.append(row);
        list.append(item);
      }
      section.append(list);
      return section;
    }),
  );
}

/// One session row.
///
/// Every control here appears because a capability the daemon published says
/// it may. Nothing on this page infers what is legal from a status string.
function sessionCard(session) {
  const card = el('article', 'card session');
  card.dataset.sessionId = session.id;
  const can = session.capabilities || {};
  if (can.open) {
    card.dataset.openable = 'true';
    card.setAttribute('role', 'link');
    card.setAttribute('tabindex', '0');
    card.setAttribute('aria-label', `Open session ${session.title || session.id}`);
  }

  const heading = el('h3');
  renderSessionTitle(heading, session);
  card.append(heading);

  const status = el('p', 'session-status');
  const state = el('span', `pill state-${session.lifecycle}`);
  state.append(
    withHiddenGlyph(LIFECYCLE_ICON[session.lifecycle] || '○'),
    el('span', '', sessionLifecycleLabel(session)),
  );
  status.append(state);
  if (session.has_error) status.append(el('span', 'pill alert', 'needs attention'));
  if (session.pending_elicitations?.length) {
    status.append(el('span', 'pill alert', 'input needed'));
  }
  const queued = (session.queued_prompts || []).length;
  if (queued) status.append(el('span', 'pill', `${queued} queued`));
  if (session.activity) status.append(el('span', 'pill', session.activity));
  card.append(status);

  if (session.operation) {
    const stage = session.operation.stages.map(entry => entry.label).join(' · ');
    card.append(
      el(
        'p',
        'session-operation',
        stage ? `${session.operation.kind} — ${stage}` : session.operation.kind,
      ),
    );
  }

  card.append(el('p', 'dim', `${session.target_id} · ${session.profile_id}`));

  if (session.preview?.length) {
    card.append(el('p', 'preview', session.preview.join('\n')));
  }

  const actions = el('div', 'row');
  if (can.rename)
    actions.append(action('Rename', 'secondary', { action: 'rename', id: session.id }));
  if (can.cancel_operation) {
    actions.append(action('Cancel', 'danger', { action: 'cancel', id: session.id }));
  }
  if (can.stop) actions.append(action('Stop', 'danger', { action: 'close', id: session.id }));
  if (can.resume) {
    actions.append(
      action('Resume', '', {
        action: 'resume',
        id: session.id,
        profile: session.profile_id,
        target: session.target_id,
      }),
    );
  }
  card.append(actions);
  return card;
}

// Durable "running" means the session is alive, not that a turn or background
// command is running. Leave activity to the separate turn/BG/idle indicator.
function sessionLifecycleLabel(session) {
  return session.state === 'running' ? 'live' : session.state;
}

/// A glyph that repeats what an adjacent word already says, so it is
/// decoration to a screen reader rather than a second reading of the same fact.
function withHiddenGlyph(glyph) {
  const node = el('span', 'state-glyph', glyph);
  node.setAttribute('aria-hidden', 'true');
  return node;
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
  navigate({ name: 'conversation', sessionId: card.dataset.sessionId });
  return true;
}

function handleSessionCardKeydown(event) {
  if (event.key !== 'Enter' && event.key !== ' ') return;
  if (!openSessionCard(event)) return;
  event.preventDefault();
}

// ---------------------------------------------------------------------------
// The other pages
// ---------------------------------------------------------------------------

function fillOptions(select, items, selected) {
  select.replaceChildren(
    ...items.map(item => {
      const option = el('option', '', item.label ?? item.id);
      option.value = item.id;
      if (item.id === selected) option.selected = true;
      return option;
    }),
  );
}

// ---------------------------------------------------------------------------
// The New wizard
// ---------------------------------------------------------------------------
//
// One decision per screen, in the order the terminal asks them, ending in a
// review that names every choice before anything is committed. A phone keyboard
// covering a modal is how the previous single flat form became unusable, so
// this is a route rather than a dialog.

/// The steps, in order. `applies` lets a step drop out — a container target has
/// no project directory to name, and a bundle with nothing dirty has nothing to
/// confirm.
const NEW_STEPS = [
  { key: 'profile', title: 'Profile', applies: () => true },
  { key: 'target', title: 'Target', applies: () => true },
  { key: 'project', title: 'Project', applies: () => true },
  { key: 'dirty', title: 'Uncommitted changes', applies: draft => draft.dirty.length > 0 },
  { key: 'review', title: 'Review', applies: () => true },
];

let newDraft = null;
let pendingNewPreflight = null;

function freshDraft() {
  return {
    step: 0,
    profileId: snapshot?.profiles[0]?.id || '',
    targetId: snapshot?.targets[0]?.id || '',
    bundleId: snapshot?.bundles[0]?.id || '',
    projectDirectory: '',
    title: '',
    dirty: [],
    acknowledged: false,
    preflighted: false,
  };
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
  if (!newDraft) newDraft = freshDraft();
  const steps = visibleSteps();
  newDraft.step = Math.min(newDraft.step, steps.length - 1);
  const step = steps[newDraft.step];
  newProgress.textContent = `Step ${newDraft.step + 1} of ${steps.length} · ${step.title}`;
  newBackButton.disabled = newDraft.step === 0;
  newNextButton.textContent = step.key === 'review' ? 'Start' : 'Next';

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
          newDraft.targetId = value;
          // Changing the target changes which project question is asked, and
          // invalidates anything the previous project answer was checked for.
          newDraft.preflighted = false;
          newDraft.dirty = [];
          newDraft.acknowledged = false;
          renderNewForm();
        }),
      );
      break;
    }
    case 'project': {
      if (targetIsBare(newDraft.targetId)) {
        body.append(
          textField(
            'Project directory',
            'new-project-directory',
            newDraft.projectDirectory,
            value => {
              newDraft.projectDirectory = value;
              newDraft.preflighted = false;
            },
          ),
        );
      } else {
        body.append(
          pickerField('Bundle', 'new-bundle', snapshot.bundles, newDraft.bundleId, value => {
            newDraft.bundleId = value;
            newDraft.preflighted = false;
            newDraft.dirty = [];
            newDraft.acknowledged = false;
          }),
        );
      }
      body.append(
        textField('Title (optional)', 'new-title', newDraft.title, value => {
          newDraft.title = value;
        }),
      );
      break;
    }
    case 'dirty': {
      body.append(
        el(
          'p',
          '',
          'These repositories have uncommitted changes. Starting a session copies them as they are.',
        ),
      );
      const list = el('ul');
      for (const repository of newDraft.dirty) list.append(el('li', '', repository));
      body.append(list);
      const label = el('label', 'field-inline');
      const box = el('input');
      box.type = 'checkbox';
      box.id = 'new-dirty-ack';
      box.checked = newDraft.acknowledged;
      box.onchange = () => {
        newDraft.acknowledged = box.checked;
      };
      label.append(box, el('span', '', 'Start anyway'));
      body.append(label);
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
      if (newDraft.dirty.length) rows.push(['Uncommitted changes', newDraft.dirty.join(', ')]);
      for (const [term, value] of rows) {
        review.append(el('dt', '', term), el('dd', '', value));
      }
      body.append(review);
    }
  }
  newStep.replaceChildren(body);
  const checking = pendingNewPreflight === newDraft;
  if (checking) {
    newNextButton.textContent = 'Checking…';
    newBackButton.disabled = true;
  }
  newNextButton.disabled = checking || newDraft.committing === true;
  for (const input of newStep.querySelectorAll('input, select')) input.disabled = checking;
}

function pickerField(label, id, items, value, onChange) {
  const field = el('label', 'field');
  field.append(el('span', '', label));
  const select = el('select');
  select.id = id;
  fillOptions(select, items, value);
  select.onchange = () => onChange(select.value);
  field.append(select);
  return field;
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

/// Ask the daemon whether this combination would launch, and what to warn
/// about, before the person commits to it.
async function preflightNew() {
  const draft = newDraft;
  if (pendingNewPreflight === draft) return false;
  const bare = targetIsBare(draft.targetId);
  pendingNewPreflight = draft;
  renderNewForm();
  try {
    const answer = await request('/api/preflight/new', {
      method: 'POST',
      body: JSON.stringify({
        workspace_id: selectedWorkspaceId(),
        profile_id: draft.profileId,
        bundle_id: draft.bundleId,
        target_id: draft.targetId,
        project_directory: bare ? draft.projectDirectory : null,
      }),
    });
    if (newDraft !== draft) return false;
    draft.dirty = answer.dirty_repositories || [];
    draft.preflighted = true;
    // A set the person has not seen cannot already be acknowledged.
    if (!draft.dirty.length) draft.acknowledged = false;
    return true;
  } catch (error) {
    if (newDraft !== draft) return false;
    throw error;
  } finally {
    if (pendingNewPreflight === draft) pendingNewPreflight = null;
    if (newDraft === draft) renderNewForm();
  }
}

async function advanceNew() {
  const steps = visibleSteps();
  const step = steps[newDraft.step];
  newError.textContent = '';

  if (step.key === 'project') {
    if (targetIsBare(newDraft.targetId) && !newDraft.projectDirectory.trim()) {
      newError.textContent = 'Name the project directory to open.';
      return;
    }
    if (!(await preflightNew())) return;
    newDraft.step = Math.min(newDraft.step + 1, visibleSteps().length - 1);
    renderNewForm();
    return;
  }
  if (step.key === 'dirty' && !newDraft.acknowledged) {
    newError.textContent = 'Confirm before starting over uncommitted changes.';
    return;
  }
  if (step.key !== 'review') {
    newDraft.step += 1;
    renderNewForm();
    return;
  }
  await commitNew();
}

async function commitNew() {
  const draft = newDraft;
  if (draft.committing) return;
  const bare = targetIsBare(newDraft.targetId);
  const body = {
    action: 'new',
    workspace_id: selectedWorkspaceId(),
    profile_id: newDraft.profileId,
    bundle_id: newDraft.bundleId,
    target_id: newDraft.targetId,
    project_directory: bare ? newDraft.projectDirectory : null,
    dirty_ack: newDraft.acknowledged ? newDraft.dirty : [],
  };
  if (newDraft.title.trim()) body.title = newDraft.title.trim();
  draft.committing = true;
  newNextButton.disabled = true;
  try {
    await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    newDraft = null;
    await refresh();
    navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
  } catch (err) {
    newError.textContent = err.message;
  } finally {
    draft.committing = false;
    newNextButton.disabled = false;
  }
}

/// Sessions that are not live and that Mjolnir owns, which is what "resume" means.
///
/// A session that cannot resume anywhere is still listed, with one plain
/// sentence saying why and where to finish it. Hiding it would leave a person
/// looking for a session they know exists.
function renderResumable() {
  const list = (snapshot.sessions || []).filter(session => session.capabilities?.resume);
  if (!list.length) {
    resumable.replaceChildren(el('p', 'dim', 'No sessions to resume.'));
    return;
  }
  resumable.replaceChildren(...list.map(resumableCard));
}

function resumableCard(session) {
  const card = el('article', 'card session');
  card.dataset.sessionId = session.id;
  card.append(el('h3', '', session.title));
  card.append(el('p', 'dim', `${sessionLifecycleLabel(session)} · ${session.profile_id}`));

  if (!session.compatible_resume_targets?.length) {
    card.append(
      el(
        'p',
        '',
        'This session cannot resume on any target configured here. Finish it in the terminal, where the repair and import options live.',
      ),
    );
    return card;
  }

  const profiles = el('label', 'field');
  profiles.append(el('span', '', 'Profile'));
  const profilePicker = el('select');
  profilePicker.dataset.role = 'resume-profile';
  fillOptions(profilePicker, snapshot.profiles, session.profile_id);
  profiles.append(profilePicker);
  card.append(profiles);

  const targets = el('label', 'field');
  targets.append(el('span', '', 'Target'));
  const targetPicker = el('select');
  targetPicker.dataset.role = 'resume-target';
  fillOptions(
    targetPicker,
    session.compatible_resume_targets.map(id => ({ id })),
    session.compatible_resume_targets.includes(session.target_id) ? session.target_id : undefined,
  );
  targets.append(targetPicker);
  card.append(targets);

  const queued = (session.queued_prompts || []).length;
  if (queued) {
    const choice = el('label', 'field');
    choice.append(el('span', '', `${queued} queued prompt${queued === 1 ? '' : 's'}`));
    const picker = el('select');
    picker.dataset.role = 'resume-queue';
    fillOptions(
      picker,
      [
        { id: 'start', label: 'Run them after resuming' },
        { id: 'discard', label: 'Discard them' },
      ],
      'start',
    );
    choice.append(picker);
    card.append(choice);
  }

  const row = el('div', 'row');
  row.append(
    action('Resume', '', {
      action: 'resume',
      id: session.id,
      profile: session.profile_id,
      target: session.target_id,
    }),
  );
  card.append(row);
  return card;
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
    refresh();
    if (currentSession) loadConversation(true);
  });
  // The browser reconnects a stream on its own; saying so is what stops the
  // page looking current while it is not.
  eventSource.addEventListener('error', () => {
    if (navigator.onLine) setConnection('reconnecting');
    else setConnection('offline');
  });
}

function showLogin() {
  snapshot = undefined;
  currentSession = null;
  if (eventSource) {
    eventSource.close();
    eventSource = undefined;
  }
  // Nothing from the previous viewer may survive a sign-out in this tab.
  pendingActions.clear();
  pendingReviewSessions.clear();
  entryNodes.clear();
  elicitationCards.clear();
  sentElicitations.clear();
  promptImages = [];
  login.classList.remove('hidden');
  app.classList.add('hidden');
  menuButton.classList.add('hidden');
  backButton.classList.add('hidden');
  closeMenu();
}

async function refresh() {
  try {
    snapshot = await request('/api/snapshot');
    login.classList.add('hidden');
    app.classList.remove('hidden');
    menuButton.classList.remove('hidden');
    if (currentSession) {
      const session = snapshot.sessions.find(x => x.id === currentSession);
      if (!session?.capabilities?.open) {
        navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
        return true;
      }
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
  if (conversationSummary) {
    conversationSummary.textContent =
      prompts.length && running.length
        ? 'Queue and shells'
        : prompts.length
          ? 'Queued prompts'
          : 'Shell commands';
  }
  conversationSide.hidden = prompts.length === 0 && running.length === 0;
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
function elicitationOptionLabel(option) {
  return option.description ? `${option.title} \u2014 ${option.description}` : option.title;
}
function elicitationControl(field) {
  if (field.kind === 'single_select' || field.kind === 'multi_select') {
    const select = document.createElement('select');
    select.multiple = field.kind === 'multi_select';
    if (!select.multiple && !field.required) select.appendChild(new Option('', ''));
    for (const option of field.options || [])
      select.appendChild(new Option(elicitationOptionLabel(option), option.value));
    if (field.kind === 'single_select' && field.default != null) select.value = field.default;
    if (select.multiple && (field.default || []).length)
      for (const option of select.options) option.selected = field.default.includes(option.value);
    return select;
  }
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
    const values = [...control.selectedOptions].map(option => option.value);
    return values.length || field.required ? values : undefined;
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
// content. A custom answer replaces the select it belongs to unless the
// request pairs it with one specific option, which is how Mjolnir's chat form
// submits the same request.
function buildElicitationForm(form, request, register) {
  const entries = [];
  for (const field of request.fields || []) {
    const wrapper = document.createElement('label');
    wrapper.className = 'elicitation-field';
    const label = document.createElement('span');
    label.textContent = `${field.title}${field.required ? ' *' : ''}`;
    const control = elicitationControl(field);
    control.required = Boolean(field.required) && field.kind !== 'boolean';
    register(control);
    wrapper.append(label, control);
    if (field.description) {
      const description = document.createElement('span');
      description.className = 'dim';
      description.textContent = field.description;
      wrapper.append(description);
    }
    if (field.kind === 'multi_select') {
      const check = () => {
        const count = control.selectedOptions.length;
        const few =
          field.min_items != null && (count > 0 || field.required) && count < field.min_items;
        const many = field.max_items != null && count > field.max_items;
        control.setCustomValidity(
          few
            ? `Select at least ${field.min_items} option(s).`
            : many
              ? `Select at most ${field.max_items} option(s).`
              : '',
        );
      };
      control.addEventListener('change', check);
      check();
    }
    form.append(wrapper);
    entries.push({ field, control });
  }
  const customByOwner = new Map();
  for (const entry of entries) {
    const owner = entry.field.custom_answer_for;
    if (!owner || entry.field.kind !== 'text' || customByOwner.has(owner)) continue;
    const target = entries.find(candidate => candidate.field.id === owner);
    if (!target || !Array.isArray(target.field.options)) continue;
    customByOwner.set(owner, entry);
  }
  return () => {
    for (const entry of entries)
      if (entry.field.kind === 'text') entry.control.value = entry.control.value.trim();
    if (!form.reportValidity()) return null;
    const active = new Map();
    for (const [owner, entry] of customByOwner)
      if (entry.control.value !== '') active.set(owner, entry);
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
let composerRevision = 0,
  composerPreserveEmptyBreak = false,
  promptImages = [];
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
function setComposerText(text) {
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
function readFileAsDataUrl(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.addEventListener('load', () => resolve(String(reader.result || '')), { once: true });
    reader.addEventListener('error', () => reject(reader.error || new Error('file read failed')), {
      once: true,
    });
    reader.readAsDataURL(file);
  });
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
async function promptImageFromFile(file) {
  if (!file.type.startsWith('image/'))
    throw new Error(`${file.name || 'That file'} is not an image`);
  if (file.size >= MAX_PROMPT_REQUEST_BYTES)
    throw new Error(`${file.name || 'That image'} is too large for the 32 MiB request limit`);
  const [dataUrl, size] = await Promise.all([readFileAsDataUrl(file), imageDimensions(file)]);
  const comma = dataUrl.indexOf(',');
  if (comma < 0 || !dataUrl.slice(comma + 1))
    throw new Error(`Could not read ${file.name || 'that image'}`);
  return {
    data_base64: dataUrl.slice(comma + 1),
    mime_type: file.type,
    width: size.width,
    height: size.height,
    name: file.name || 'Pasted image',
  };
}
async function attachImageFiles(files) {
  const session = snapshot?.sessions.find(x => x.id === currentSession);
  if (!currentSession || !session?.prompt_images_supported || !files.length) return;
  const sessionId = currentSession;
  try {
    const added = [];
    for (const file of files) added.push(await promptImageFromFile(file));
    if (currentSession !== sessionId) return;
    promptImages = promptImages.concat(added);
    renderAttachments();
    document.querySelector('#conversation-error').textContent = '';
  } catch (err) {
    document.querySelector('#conversation-error').textContent = err.message;
  }
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
  for (const [index, image] of promptImages.entries()) {
    const chip = document.createElement('div');
    chip.className = 'attachment';
    const thumb = document.createElement('img');
    thumb.alt = '';
    thumb.src = `data:${image.mime_type};base64,${image.data_base64}`;
    const caption = document.createElement('span');
    caption.textContent = `${image.name} \u00b7 ${image.width}\u00d7${image.height}`;
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'danger';
    remove.setAttribute('aria-label', `Remove ${image.name}`);
    remove.textContent = '\u00d7';
    remove.onclick = () => {
      promptImages.splice(index, 1);
      renderAttachments();
    };
    chip.append(thumb, caption, remove);
    attachments.append(chip);
  }
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
      await sendAction({ action: 'set-plan-mode', session_id: currentSession, active });
      // A trailing instruction is a prompt to send once the mode has changed.
      if (argument) {
        await sendAction({
          action: 'prompt',
          session_id: currentSession,
          text: argument,
          images: [],
        });
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
  try {
    await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    // Do not let an action that completed after navigation clear the next
    // conversation's draft or error state.
    if (!sessionId || currentSession === sessionId) {
      setComposerText('');
      error.textContent = '';
    }
    await refresh();
    return true;
  } catch (err) {
    if (!sessionId || currentSession === sessionId) error.textContent = err.message;
    return false;
  }
}

/// Guard against sending twice.
///
/// Enter calls submit directly, so it bypasses the disabled button entirely;
/// without this a fast double press sends the same prompt twice.
let promptInFlight = false;

async function submitPrompt() {
  if (!currentSession || promptInFlight) return;
  const value = composerText();
  const images = promptImages;
  if (!value.trim() && !images.length) return;
  const error = document.querySelector('#conversation-error');

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
            data_base64: image.data_base64,
            mime_type: image.mime_type,
            width: image.width,
            height: image.height,
          })),
        };
    const payload = JSON.stringify(body);
    if (new TextEncoder().encode(payload).byteLength > MAX_PROMPT_REQUEST_BYTES) {
      error.textContent = 'Prompt attachments exceed the 32 MiB request limit.';
      return;
    }
    await request('/api/actions', { method: 'POST', body: payload });
    // The composer is cleared only once the daemon has taken the prompt, so a
    // refusal leaves the text where it can be edited and sent again.
    setComposerText('');
    promptImages = [];
    renderAttachments();
    updateCommandPalette();
    // The stored copy goes with the one on screen, so reopening does not put
    // back a prompt that has already run.
    saveDraft();
    error.textContent = '';
    await refresh();
  } catch (err) {
    error.textContent = err.message;
  } finally {
    promptInFlight = false;
    sendButton.disabled = false;
  }
}

const PROSE_ROLES = new Set(['user', 'agent', 'thought']);

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
function paintEntry(node, entry) {
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
  for (const summary of node.querySelectorAll('details.block-fold > summary')) {
    if (openFolds.has(summary.textContent)) summary.parentElement.open = true;
  }
}

function renderEntries(entries, replace) {
  const wasAtTail = atTail();
  if (replace) {
    feed.replaceChildren();
    entryNodes.clear();
  }
  let appended = false;
  for (const entry of entries) {
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
    paintEntry(node, entry);
  }
  if (wasAtTail) scrollToTail();
  else if (appended) jumpToLatest.classList.remove('hidden');
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

async function loadConversation(delta = false) {
  if (!currentSession) return;
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
    const result = await request(
      `/api/conversations/${encodeURIComponent(sessionId)}${delta && cursor ? `?after_seq=${cursor}` : ''}`,
    );
    if (generation !== conversationGeneration) return;
    renderEntries(result.entries, !delta || result.reset);
    cursor = result.latest_seq;
    if (cursor > acknowledged) {
      const through = cursor;
      await request(`/api/conversations/${encodeURIComponent(sessionId)}/read`, {
        method: 'POST',
        body: JSON.stringify({ through }),
      });
      if (generation !== conversationGeneration) return;
      acknowledged = through;
    }
  } catch (err) {
    if (generation !== conversationGeneration) return;
    if (err.message === 'unauthorized') {
      showLogin();
      return;
    }
    document.querySelector('#conversation-error').textContent = err.message;
  } finally {
    conversationInFlight = false;
    if (conversationPending && generation === conversationGeneration) {
      conversationPending = false;
      loadConversation(true);
    }
  }
}

async function openConversation(id) {
  if (currentSession === id) return;
  const session = snapshot?.sessions.find(x => x.id === id);
  if (!session?.capabilities?.open) return;
  currentSession = id;
  conversationGeneration += 1;
  conversationPending = false;
  cursor = 0;
  acknowledged = 0;
  entryNodes.clear();
  feed.replaceChildren();
  document.querySelector('#conversation-title').textContent = session.title;
  document.querySelector('#conversation-state').textContent = sessionLifecycleLabel(session);
  renderQueue(session);
  renderElicitations(session);
  renderTurnReview(session);
  renderConversationHeader(session);
  promptImages = [];
  renderAttachments();
  restoreDraft(id, conversationGeneration);
  await loadConversation(false);
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

function renderConversationHeader(session) {
  renderSessionTitle(document.querySelector('#conversation-title'), session);
  const state = document.querySelector('#conversation-state');
  state.textContent = sessionLifecycleLabel(session);
  state.className = `pill state-${session.lifecycle}`;
  cancelTurnButton.classList.toggle('hidden', !session.capabilities?.cancel_turn);

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
  sendButton.disabled = !canPrompt || promptInFlight;
  if (session.plan_mode_active) {
    state.textContent = `${sessionLifecycleLabel(session)} · plan`;
  }
}

/// Drop everything the conversation view was holding.
///
/// Leaving has to clear the keyed nodes and the pending elicitation cards, or
/// the next conversation opens on top of the last one's rows.
function leaveConversation() {
  currentSession = null;
  conversationGeneration += 1;
  conversationPending = false;
  cursor = 0;
  acknowledged = 0;
  entryNodes.clear();
  feed.replaceChildren();
  elicitations.replaceChildren();
  elicitationCards.clear();
  reviewHost.replaceChildren();
  reviewSignature = null;
  promptImages = [];
  renderAttachments();
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

menuButton.onclick = () => {
  const open = menu.classList.toggle('hidden');
  menuButton.setAttribute('aria-expanded', String(!open));
};

// A tap outside the menu closes it, and so does Escape. Both are capture-phase
// so a control inside the menu still receives its own click first.
document.addEventListener('pointerdown', event => {
  if (menu.classList.contains('hidden')) return;
  if (menu.contains(event.target) || menuButton.contains(event.target)) return;
  closeMenu();
});
document.addEventListener('keydown', event => {
  if (event.key === 'Escape') closeMenu();
});

menu.onclick = event => {
  const target = event.target.closest('button[data-route]');
  if (!target) return;
  closeMenu();
  navigate({ name: target.dataset.route });
};

logout.onclick = async () => {
  await request('/auth/session', { method: 'DELETE' });
  location.hash = '';
  location.reload();
};

backButton.onclick = () => {
  // Back means the page behind this one, which is the dashboard for the
  // workspace this route belongs to.
  navigate({ name: 'dashboard', workspaceId: selectedWorkspaceId() });
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
  newDraft.step -= 1;
  newError.textContent = '';
  renderNewForm();
};

newForm.onsubmit = async event => {
  event.preventDefault();
  try {
    await advanceNew();
  } catch (err) {
    newError.textContent = err.message;
  }
};

/// One session action, from the row that carries it.
///
/// The pending set is checked at entry and released in a `finally`, so a
/// double tap cannot send twice and a failure cannot leave the control dead.
async function runSessionAction(dataset, errorNode, extra) {
  const key = `${dataset.action}:${dataset.id}`;
  if (pendingActions.has(key)) return;
  if (dataset.action === 'open') {
    navigate({ name: 'conversation', sessionId: dataset.id });
    return;
  }
  if (dataset.action === 'close') {
    const session = snapshot.sessions.find(item => item.id === dataset.id);
    const active = session?.chat_phase === 'running';
    const question = active
      ? 'Stop active session?\n\nThe current turn will be interrupted. Mjolnir will then save a recovery copy and destroy the target.'
      : 'Stop session?\n\nMjolnir will save a recovery copy and destroy the target.';
    if (!confirm(question)) return;
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
    body.workspace_id = selectedWorkspaceId();
    body.queue = extra?.queue || 'start';
  }
  pendingActions.add(key);
  renderRoute();
  try {
    await request('/api/actions', { method: 'POST', body: JSON.stringify(body) });
    errorNode.textContent = '';
    await refresh();
  } catch (err) {
    errorNode.textContent = err.message;
  } finally {
    pendingActions.delete(key);
    renderRoute();
  }
}

sessions.onclick = async e => {
  const target = e.target.closest('button[data-action]');
  if (target) {
    await runSessionAction(target.dataset, actionError);
    return;
  }
  openSessionCard(e);
};

sessions.onkeydown = handleSessionCardKeydown;

resumable.onclick = async e => {
  const target = e.target.closest('button[data-action]');
  if (!target) return;
  const card = target.closest('.session');
  const pick = role => card?.querySelector(`select[data-role="${role}"]`)?.value;
  await runSessionAction(target.dataset, resumeError, {
    target_id: pick('resume-target'),
    profile_id: pick('resume-profile'),
    queue: pick('resume-queue'),
  });
};

document.querySelector('#prompt-form').onsubmit = e => {
  e.preventDefault();
  submitPrompt();
};
promptText.addEventListener('input', () => {
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

jumpToLatest.onclick = scrollToTail;
feedScroll.addEventListener('scroll', () => {
  if (atTail()) jumpToLatest.classList.add('hidden');
});

cancelTurnButton.onclick = async () => {
  await sendAction({ action: 'cancel-turn', session_id: currentSession });
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
  refresh().then(ok => {
    if (ok) setConnection('online');
    if (ok && currentSession) loadConversation(false);
  });
}

window.addEventListener('online', reconnect);
window.addEventListener('offline', () => setConnection('offline'));

// A backgrounded progressive web app gets no `online` event, so the first
// signal that it is back is somebody unlocking the screen.
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && navigator.onLine) reconnect();
});
if ('serviceWorker' in navigator) {
  // A registration that fails means the application is not installable, and
  // nothing more. Left uncaught it is an unhandled rejection, which is exactly
  // the page error the reliability suite refuses to see.
  navigator.serviceWorker.register('/service-worker.js').catch(() => {});
}
restoreRoute();
