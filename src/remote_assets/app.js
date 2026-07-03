// Mjolnir Web viewer entry module: state, rendering, network, and
// wiring. Served as an ES module; helpers live in sibling modules.
import { $, cloneTemplate, emptyNote, isNarrowScreen, narrowScreen, refreshTimestamps, scrollBehavior, setTimestamp, syncKeyboardInset, withViewTransition } from "./dom.js";
import { entryIcon, entryKind, entryLabel, renderRichText } from "./markdown.js";
import "./palette.js";
import {
  enableNotifications,
  notificationsEnabled,
  notificationsSupported,
  notify,
  updateTitleBadge,
} from "./notify.js";

const queryParams = new URLSearchParams(window.location.search);
const viewerToken = queryParams.get("token") || "";

const authScreenEl = $("auth-screen");
const appEl = $("app");
const authFormEl = $("auth-form");
const authTokenEl = $("auth-token");
const authSubmitEl = $("auth-submit");
const authErrorEl = $("auth-error");
const logoutButtonEl = $("logout-button");
const statusEl = $("status");
const sessionsEl = $("sessions");
const transcriptEl = $("transcript");
const transcriptTitleEl = $("transcript-title");
const transcriptMetaEl = $("transcript-meta");
const workingBadgeEl = $("working-badge");
const approvalBadgeEl = $("approval-badge");
const backButtonEl = $("back-button");
const permissionsEl = $("permissions");
const sessionConfigPanelEl = $("session-config-panel");
const sessionConfigSummaryEl = $("session-config-summary");
const sessionConfigEl = $("session-config");
const queueInputEl = $("queue-input");
const queueSubmitEl = $("queue-submit");
const queueStatusEl = $("queue-status");
const queueListEl = $("queue-list");
const jumpToLatestEl = $("jump-to-latest");
const newTaskButtonEl = $("new-task-button");
const stopTaskButtonEl = $("stop-task-button");
const cancelTurnButtonEl = $("cancel-turn-button");
const newTaskDialogEl = $("new-task-dialog");
const newTaskFormEl = $("new-task-form");
const newTaskAgentEl = $("new-task-agent");
const newTaskCwdEl = $("new-task-cwd");
const newTaskPromptEl = $("new-task-prompt");
const newTaskErrorEl = $("new-task-error");
const newTaskCancelEl = $("new-task-cancel");
const newTaskStartEl = $("new-task-start");
const sessionTpl = $("tpl-session");
const entryTpl = $("tpl-entry");
const queueTpl = $("tpl-queue-item");
const permissionTpl = $("tpl-permission");
const configOptionTpl = $("tpl-config-option");
const paletteEl = $("command-palette");

const POLL_MS = 2000;
const WORKING_WINDOW_MS = 6000;
// How long a sent config change may stay "applying…" before the control
// re-enables. The session applies changes only between turns, and a
// change it cannot apply at all (agent rejected it) never shows up in a
// snapshot — without this cap such a change would disable its combobox
// forever. The queued change itself is unaffected: it still applies (or
// dies with the session) server-side.
const CONFIG_APPLY_TIMEOUT_MS = 60000;

let sessions = [];
let queuedPrompts = [];
let selectedSessionId = null;
let refreshTimer = null;
let authenticated = false;
let shouldStickToBottom = true;
let forceScrollToBottom = false;
let pendingNewEntries = 0;
let configPanelUserToggled = false;
let expectedConfigPanelOpen = null;
const draftBySessionId = new Map();
const activityBySessionId = new Map();
const sessionCardById = new Map();
// Decisions already POSTed, keyed "<session_id>\u001f<request_id>",
// so the buttons stay disabled until the pending request disappears
// from the next snapshot.
const sentDecisions = new Set();
// Config changes already POSTed, mapping "<session_id>\u001f<target>" to
// { value, sentAt } for the change we sent. The select stays disabled
// (showing the sent value) until the next snapshot's current_value
// matches, confirming the session applied it, or until
// CONFIG_APPLY_TIMEOUT_MS passes.
const sentConfigChanges = new Map();
// Persistent combobox controllers keyed by "<session_id><target>", kept
// across polls so an open/searched dropdown is never torn down by a
// background snapshot refresh.
const configCombos = new Map();
// Keyed render state: the transcript DOM is patched in place instead of
// rebuilt on every poll, so text selection and scroll momentum survive.
let renderedSessionId = null;
let renderedEntries = [];
// Server-spawned tasks. `spawnEnabled` is null until the first
// `/api/tasks` probe answers: 200 enables the launcher UI, 403 means the
// server runs without --allow-spawn and every control stays hidden.
let spawnEnabled = null;
let serverTasks = [];
const taskBySessionId = new Map();
// Server-push stream. While it is open the 2s poll degrades to a slow
// safety net; every SSE payload also counts as a fresh sessions fetch.
let eventSource = null;
let sseConnected = false;
let lastSessionsFetch = 0;
const SSE_POLL_FALLBACK_MS = 30000;

function showAuth(message = "") {
  authenticated = false;
  authErrorEl.textContent = message;
  authScreenEl.hidden = false;
  appEl.hidden = true;
  sessions = [];
  queuedPrompts = [];
  selectedSessionId = null;
  sessionCardById.clear();
  activityBySessionId.clear();
  sentDecisions.clear();
  sentConfigChanges.clear();
  configCombos.clear();
  configPanelUserToggled = false;
  renderedSessionId = null;
  renderedEntries = [];
  spawnEnabled = null;
  serverTasks = [];
  taskBySessionId.clear();
  disconnectEventStream();
  if (newTaskDialogEl.open) {
    newTaskDialogEl.close();
  }
  if (refreshTimer) {
    clearInterval(refreshTimer);
    refreshTimer = null;
  }
}

function showApp() {
  authenticated = true;
  authErrorEl.textContent = "";
  authScreenEl.hidden = true;
  appEl.hidden = false;
  if (!refreshTimer) {
    refreshTimer = setInterval(refreshSessions, POLL_MS);
  }
  connectEventStream();
}

async function apiFetch(url, options = {}) {
  const requestUrl = new URL(url, window.location.origin);
  if (viewerToken && !requestUrl.searchParams.has("token")) {
    requestUrl.searchParams.set("token", viewerToken);
  }
  const response = await fetch(requestUrl, {
    ...options,
    credentials: "same-origin",
    headers: {
      ...(options.headers || {}),
    },
  });
  if (response.status === 401) {
    showAuth("Your session expired. Enter the six-digit viewer code again.");
    throw new Error("HTTP 401");
  }
  return response;
}


/* ---- routing: #/session/<id> drives selection and the phone view ---- */

function routeSessionId() {
  const match = /^#\/session\/(.+)$/.exec(window.location.hash);
  return match ? decodeURIComponent(match[1]) : null;
}

function navigateToSession(sessionId) {
  const target = `#/session/${encodeURIComponent(sessionId)}`;
  if (window.location.hash === target) {
    applyRoute();
  } else {
    window.location.hash = target;
  }
}

function navigateToList() {
  window.location.hash = "#/";
}

function applyRoute() {
  const routed = routeSessionId();
  if (routed && routed !== selectedSessionId) {
    saveDraftForSelectedSession();
    selectedSessionId = routed;
    queuedPrompts = [];
    pendingNewEntries = 0;
    configPanelUserToggled = false;
    forceScrollToBottom = true;
    restoreDraftForSelectedSession();
    void refreshQueuedPrompts();
  }
  withViewTransition(() => {
    appEl.classList.toggle("show-chat", routed !== null);
    renderAll();
  });
}

function maybeAutoSelectSession() {
  if (routeSessionId() || isNarrowScreen() || !sessions.length) {
    return;
  }
  if (!selectedSessionId || !sessions.some((s) => s.session_id === selectedSessionId)) {
    selectedSessionId = sessions[0].session_id;
    // Reset per-session view state exactly like the route-change
    // path, so the previous session's queue never bleeds through.
    queuedPrompts = [];
    pendingNewEntries = 0;
    configPanelUserToggled = false;
    forceScrollToBottom = true;
    restoreDraftForSelectedSession();
    void refreshQueuedPrompts();
  }
}

/* ---- activity heuristics ---- */

function transcriptActivitySignature(session) {
  const transcript = Array.isArray(session.transcript) ? session.transcript : [];
  const last = transcript[transcript.length - 1];
  return `${transcript.length}:${last ? (last.text || "").length : 0}`;
}

function noteSessionActivity() {
  const now = Date.now();
  const seen = new Set();
  for (const session of sessions) {
    seen.add(session.session_id);
    const sig = transcriptActivitySignature(session);
    const prev = activityBySessionId.get(session.session_id);
    if (!prev) {
      activityBySessionId.set(session.session_id, { sig, at: 0 });
    } else if (prev.sig !== sig) {
      activityBySessionId.set(session.session_id, { sig, at: now });
    }
  }
  for (const id of [...activityBySessionId.keys()]) {
    if (!seen.has(id)) {
      activityBySessionId.delete(id);
    }
  }
}

function sessionIsWorking(sessionId) {
  // Trust the authoritative flag published by the session itself whenever it
  // is present (a boolean) — including an explicit `false`, which must win
  // over the activity heuristic so the working badge and cancel control clear
  // the instant a turn ends. Only records from older mj versions omit the
  // flag; for those, fall back to the transcript-activity heuristic.
  const session = sessions.find((s) => s.session_id === sessionId);
  if (typeof session?.working === "boolean") {
    return session.working;
  }
  const activity = activityBySessionId.get(sessionId);
  return Boolean(activity) && Date.now() - activity.at < WORKING_WINDOW_MS;
}

function sessionLabel(session) {
  return session.name || session.session_id;
}

function sessionPendingPermissions(session) {
  return session && Array.isArray(session.pending_permissions) ? session.pending_permissions : [];
}

function decisionKey(sessionId, requestId) {
  return `${sessionId}\u001f${requestId}`;
}

/* ---- session list (keyed, patched in place) ---- */

function createSessionCard(sessionId) {
  const root = cloneTemplate(sessionTpl);
  root.dataset.sessionId = sessionId;
  root.addEventListener("click", () => {
    saveDraftForSelectedSession();
    navigateToSession(sessionId);
  });
  return {
    root,
    title: root.querySelector(".session-title"),
    approval: root.querySelector(".session-approval"),
    project: root.querySelector(".session-project"),
    agent: root.querySelector(".session-agent"),
    counts: root.querySelector(".session-counts"),
    updated: root.querySelector(".session-updated"),
  };
}

function updateSessionCard(card, session) {
  card.root.classList.toggle("active", session.session_id === selectedSessionId);
  card.root.classList.toggle("working", sessionIsWorking(session.session_id));
  card.title.textContent = sessionLabel(session);
  card.project.textContent = session.project || "-";
  card.agent.textContent = session.agent || "-";
  const queued = session.queued_prompt_count ?? 0;
  card.counts.textContent = `${session.total_messages ?? 0} messages${queued ? ` · ${queued} queued` : ""}`;
  setTimestamp(card.updated, session.last_update || "");
  const needsApproval = sessionPendingPermissions(session).length > 0;
  card.root.classList.toggle("needs-approval", needsApproval);
  card.approval.hidden = !needsApproval;
}

function renderSessions() {
  if (!sessions.length) {
    sessionCardById.clear();
    sessionsEl.replaceChildren(emptyNote("No sessions have been published yet."));
    return;
  }
  if (sessionsEl.querySelector(".empty")) {
    sessionsEl.replaceChildren();
  }
  maybeAutoSelectSession();
  const seen = new Set();
  for (const session of sessions) {
    seen.add(session.session_id);
    let card = sessionCardById.get(session.session_id);
    if (!card) {
      card = createSessionCard(session.session_id);
      sessionCardById.set(session.session_id, card);
      sessionsEl.appendChild(card.root);
    }
    updateSessionCard(card, session);
  }
  for (const [id, card] of [...sessionCardById]) {
    if (!seen.has(id)) {
      card.root.remove();
      sessionCardById.delete(id);
    }
  }
  // Only touch DOM order when it actually changed; re-inserting nodes
  // restarts their CSS animations.
  const desired = sessions.map((s) => s.session_id);
  const current = [...sessionsEl.children].map((el) => el.dataset.sessionId);
  if (desired.join("\u001f") !== current.join("\u001f")) {
    for (const id of desired) {
      sessionsEl.appendChild(sessionCardById.get(id).root);
    }
  }
}

/* ---- markdown-lite rendering (DOM building only, never innerHTML) ---- */

/* ---- transcript ---- */

function setEntryBody(refs, entry) {
  refs.body.replaceChildren();
  const text = entry.text || "";
  if (refs.kind === "agent" || refs.kind === "thought") {
    refs.body.appendChild(renderRichText(text));
  } else {
    const pre = document.createElement("pre");
    pre.className = "entry-text";
    pre.textContent = text;
    refs.body.appendChild(pre);
  }
}

function createEntryRefs(entry) {
  const el = cloneTemplate(entryTpl);
  const kind = entryKind(entry);
  el.dataset.kind = kind;
  el.querySelector(".entry-icon").textContent = entryIcon(kind);
  el.querySelector(".entry-kind").textContent = entryLabel(kind);
  setTimestamp(el.querySelector(".entry-time"), entry.timestamp || "");
  const refs = {
    kind,
    timestamp: entry.timestamp || "",
    text: entry.text || "",
    el,
    body: el.querySelector(".entry-body"),
  };
  setEntryBody(refs, entry);
  return refs;
}

function isNearTranscriptBottom() {
  const threshold = 48;
  const distanceFromBottom = transcriptEl.scrollHeight - transcriptEl.scrollTop - transcriptEl.clientHeight;
  return distanceFromBottom <= threshold;
}

function scrollTranscriptToBottom(behavior = scrollBehavior()) {
  transcriptEl.scrollTo({
    top: transcriptEl.scrollHeight,
    behavior,
  });
  shouldStickToBottom = true;
  jumpToLatestEl.hidden = true;
}

function updateJumpToLatestVisibility() {
  const hasOverflow = transcriptEl.scrollHeight > transcriptEl.clientHeight + 8;
  const nearBottom = isNearTranscriptBottom();
  shouldStickToBottom = nearBottom;
  jumpToLatestEl.hidden = !hasOverflow || nearBottom;
  jumpToLatestEl.classList.toggle("has-new", pendingNewEntries > 0);
  jumpToLatestEl.textContent = pendingNewEntries > 0
    ? `Jump to latest (${pendingNewEntries} new)`
    : "Jump to latest";
}

function renderTranscript(session) {
  const pinned = forceScrollToBottom || shouldStickToBottom || isNearTranscriptBottom();
  if (!session) {
    renderedSessionId = null;
    renderedEntries = [];
    transcriptTitleEl.textContent = "Transcript";
    transcriptMetaEl.textContent = "Select a session to inspect its history.";
    workingBadgeEl.hidden = true;
    transcriptEl.replaceChildren(
      emptyNote(routeSessionId() ? "This session is not currently connected." : "No transcript available."),
    );
    pendingNewEntries = 0;
    jumpToLatestEl.hidden = true;
    forceScrollToBottom = false;
    return;
  }

  const transcript = Array.isArray(session.transcript) ? session.transcript : [];
  transcriptTitleEl.textContent = sessionLabel(session);
  transcriptMetaEl.textContent = `${session.project || "-"} · ${session.agent || "-"} · ${transcript.length} entries`;
  workingBadgeEl.hidden = !sessionIsWorking(session.session_id);

  if (renderedSessionId !== session.session_id) {
    renderedSessionId = session.session_id;
    renderedEntries = [];
    transcriptEl.replaceChildren();
  }
  if (!transcript.length) {
    renderedEntries = [];
    transcriptEl.replaceChildren(emptyNote("This session has not produced any transcript entries yet."));
    pendingNewEntries = 0;
    jumpToLatestEl.hidden = true;
    forceScrollToBottom = false;
    return;
  }
  if (!renderedEntries.length) {
    transcriptEl.replaceChildren();
  }

  // Match the rendered prefix by position (kind + timestamp identify an
  // entry; only the last entry's text grows while streaming).
  let matched = 0;
  const limit = Math.min(renderedEntries.length, transcript.length);
  while (matched < limit) {
    const have = renderedEntries[matched];
    const want = transcript[matched];
    if (have.kind !== entryKind(want) || have.timestamp !== (want.timestamp || "")) {
      break;
    }
    matched += 1;
  }
  for (let i = 0; i < matched; i += 1) {
    const have = renderedEntries[i];
    const want = transcript[i];
    const text = want.text || "";
    if (have.text !== text) {
      have.text = text;
      setEntryBody(have, want);
    }
  }
  while (renderedEntries.length > matched) {
    renderedEntries.pop().el.remove();
  }
  let appendedCount = 0;
  for (let i = matched; i < transcript.length; i += 1) {
    const refs = createEntryRefs(transcript[i]);
    transcriptEl.appendChild(refs.el);
    renderedEntries.push(refs);
    appendedCount += 1;
  }

  if (pinned) {
    pendingNewEntries = 0;
    scrollTranscriptToBottom(forceScrollToBottom ? "auto" : scrollBehavior());
  } else {
    pendingNewEntries += appendedCount;
    updateJumpToLatestVisibility();
  }
  forceScrollToBottom = false;
}

/* ---- queue + composer ---- */

function saveDraftForSelectedSession() {
  if (selectedSessionId) {
    draftBySessionId.set(selectedSessionId, queueInputEl.value);
  }
}

function restoreDraftForSelectedSession() {
  const draft = selectedSessionId ? draftBySessionId.get(selectedSessionId) || "" : "";
  if (queueInputEl.value !== draft) {
    queueInputEl.value = draft;
    syncComposerHeight();
  }
}

function syncComposerHeight() {
  queueInputEl.style.height = "auto";
  const next = Math.min(queueInputEl.scrollHeight, 200);
  queueInputEl.style.height = `${Math.max(next, 56)}px`;
}

function renderQueue(session) {
  queueListEl.replaceChildren();
  queueInputEl.disabled = !session;
  queueSubmitEl.disabled = !session;

  if (!session) {
    queueStatusEl.textContent = "Select a session to queue prompts.";
    queueSubmitEl.textContent = "Queue";
    return;
  }

  // Server-owned tasks accept live prompts; other sessions only take
  // queued prompts that run when their local owner is idle.
  const live = Boolean(selectedLiveTask());
  queueSubmitEl.textContent = live ? "Send" : "Queue";
  queueInputEl.placeholder = live
    ? "Send a prompt to this task"
    : "Queue a prompt for this session";

  queueStatusEl.textContent = queuedPrompts.length
    ? `${queuedPrompts.length} queued prompt${queuedPrompts.length === 1 ? "" : "s"} waiting.`
    : live
      ? "Prompts run immediately; a busy task queues them instead."
      : "Prompts queue here and run when the session is idle.";

  for (const prompt of queuedPrompts) {
    const item = cloneTemplate(queueTpl);
    setTimestamp(item.querySelector(".queue-item-time"), prompt.created_at || "");
    item.querySelector(".queue-item-text").textContent = prompt.text || "";
    queueListEl.appendChild(item);
  }
}

function renderPermissions(session) {
  permissionsEl.replaceChildren();
  const pending = sessionPendingPermissions(session);
  // Forget sent decisions once their request leaves the snapshot.
  if (session) {
    for (const key of [...sentDecisions]) {
      if (
        key.startsWith(`${session.session_id}\u001f`)
        && !pending.some((request) => decisionKey(session.session_id, request.request_id) === key)
      ) {
        sentDecisions.delete(key);
      }
    }
  }
  approvalBadgeEl.hidden = !pending.length;
  if (pending.length) {
    workingBadgeEl.hidden = true;
  }
  for (const request of pending) {
    const card = cloneTemplate(permissionTpl);
    card.querySelector(".permission-title").textContent = request.title || request.request_id;
    setTimestamp(card.querySelector(".permission-time"), request.requested_at || "");
    const optionsEl = card.querySelector(".permission-options");
    const sent = sentDecisions.has(decisionKey(session.session_id, request.request_id));
    for (const option of request.options || []) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "permission-option";
      button.dataset.kind = option.kind || "other";
      button.textContent = option.label || option.option_id;
      button.disabled = sent;
      button.addEventListener("click", () => {
        void submitPermissionDecision(session.session_id, request.request_id, option.option_id);
      });
      optionsEl.appendChild(button);
    }
    card.querySelector(".permission-status").textContent = sent
      ? "Decision sent — waiting for the session to apply it…"
      : "";
    permissionsEl.appendChild(card);
  }
}

async function submitPermissionDecision(sessionId, requestId, optionId) {
  const key = decisionKey(sessionId, requestId);
  if (sentDecisions.has(key)) {
    return;
  }
  sentDecisions.add(key);
  renderChat();
  try {
    const response = await apiFetch("/api/permission-decisions", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        session_id: sessionId,
        request_id: requestId,
        option_id: optionId,
      }),
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
  } catch (error) {
    sentDecisions.delete(key);
    if (error.message !== "HTTP 401") {
      queueStatusEl.textContent = `Decision failed: ${error.message}`;
    }
    renderChat();
  }
}

function sessionConfigOptions(session) {
  return session && Array.isArray(session.session_config) ? session.session_config : [];
}

function setConfigPanelOpen(open) {
  if (sessionConfigPanelEl.open === open) {
    return;
  }
  expectedConfigPanelOpen = open;
  sessionConfigPanelEl.open = open;
}

function updateConfigPanel(session, options) {
  const hasOptions = Boolean(session && options.length);
  sessionConfigPanelEl.hidden = !hasOptions;
  if (!hasOptions) {
    sessionConfigSummaryEl.textContent = "";
    configPanelUserToggled = false;
    setConfigPanelOpen(false);
    return;
  }

  sessionConfigSummaryEl.textContent =
    `${options.length} option${options.length === 1 ? "" : "s"}`;
  if (!configPanelUserToggled) {
    setConfigPanelOpen(!isNarrowScreen());
  }
}

function configOptionKey(option) {
  return `${option.target_kind}\u001f${option.config_id || ""}`;
}

function configChangeKey(sessionId, option) {
  return `${sessionId}\u001f${configOptionKey(option)}`;
}

// A searchable single-select dropdown built from scratch so it stays
// usable with long option lists (e.g. model pickers) and matches the app
// styling. `key` ties it to its sent-change entry; `onSelect(value)` fires
// only when the user picks a different value. The renderer reuses one
// instance across polls via the returned controller.
function createCombobox(option, key, onSelect) {
  let choices = option.choices || [];
  let value = currentComboValue(option, key);
  let open = false;
  let filtered = choices.slice();
  let activeIndex = -1;

  const root = document.createElement("div");
  root.className = "config-combo";

  const trigger = document.createElement("button");
  trigger.type = "button";
  trigger.className = "config-combo-trigger";
  trigger.setAttribute("aria-haspopup", "listbox");
  trigger.setAttribute("aria-expanded", "false");
  const valueEl = document.createElement("span");
  valueEl.className = "config-combo-value";
  const chevron = document.createElement("span");
  chevron.className = "config-combo-chevron";
  chevron.setAttribute("aria-hidden", "true");
  trigger.append(valueEl, chevron);

  const popup = document.createElement("div");
  popup.className = "config-combo-popup";
  popup.hidden = true;
  const search = document.createElement("input");
  search.type = "text";
  search.className = "config-combo-search";
  search.placeholder = "Search…";
  search.setAttribute("aria-label", "Search options");
  const list = document.createElement("ul");
  list.className = "config-combo-list";
  list.setAttribute("role", "listbox");
  popup.append(search, list);
  root.append(trigger, popup);

  function labelFor(val) {
    const choice = choices.find((c) => c.value === val);
    return choice ? choice.label || choice.value : val || "—";
  }

  function renderValue() {
    valueEl.textContent = labelFor(value);
  }

  function renderList() {
    const q = search.value.trim().toLowerCase();
    filtered = choices.filter((c) => {
      if (!q) return true;
      return (
        (c.label || "").toLowerCase().includes(q)
        || (c.value || "").toLowerCase().includes(q)
        || (c.description || "").toLowerCase().includes(q)
      );
    });
    list.replaceChildren();
    if (!filtered.length) {
      const empty = document.createElement("li");
      empty.className = "config-combo-empty";
      empty.textContent = "No matches";
      list.appendChild(empty);
      return;
    }
    filtered.forEach((choice, index) => {
      const item = document.createElement("li");
      item.className = "config-combo-option";
      item.setAttribute("role", "option");
      if (choice.value === value) {
        item.classList.add("is-selected");
        item.setAttribute("aria-selected", "true");
      }
      if (index === activeIndex) {
        item.classList.add("is-active");
      }
      const label = document.createElement("span");
      label.className = "config-combo-option-label";
      label.textContent = choice.label || choice.value;
      item.appendChild(label);
      if (choice.description) {
        const desc = document.createElement("span");
        desc.className = "config-combo-option-desc";
        desc.textContent = choice.description;
        item.appendChild(desc);
      }
      // mousedown (not click) so the choice lands before the search
      // input's blur can close the popup out from under it.
      item.addEventListener("mousedown", (event) => {
        event.preventDefault();
        choose(choice.value);
      });
      item.addEventListener("mousemove", () => setActive(index));
      list.appendChild(item);
    });
  }

  function updateSearchMode() {
    popup.classList.toggle(
      "is-searching",
      isNarrowScreen()
        && open
        && (document.activeElement === search || search.value.trim().length > 0),
    );
  }

  function setActive(index) {
    activeIndex = index;
    [...list.children].forEach((el, i) => {
      el.classList.toggle("is-active", i === activeIndex);
    });
    const active = list.children[activeIndex];
    if (active && active.scrollIntoView) {
      active.scrollIntoView({ block: "nearest" });
    }
  }

  function onOutsidePointer(event) {
    if (!root.contains(event.target)) closePopup();
  }

  function openPopup() {
    if (open || trigger.disabled) return;
    open = true;
    popup.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
    search.value = "";
    activeIndex = -1;
    renderList();
    const selected = filtered.findIndex((c) => c.value === value);
    if (selected >= 0) setActive(selected);
    if (!isNarrowScreen()) {
      search.focus();
    }
    updateSearchMode();
    document.addEventListener("pointerdown", onOutsidePointer, true);
  }

  function closePopup() {
    if (!open) return;
    open = false;
    popup.hidden = true;
    trigger.setAttribute("aria-expanded", "false");
    if (document.activeElement === search) {
      search.blur();
    }
    popup.classList.remove("is-searching");
    renderValue();
    document.removeEventListener("pointerdown", onOutsidePointer, true);
  }

  function choose(val) {
    closePopup();
    if (val !== value) onSelect(val);
  }

  trigger.addEventListener("click", () => (open ? closePopup() : openPopup()));
  search.addEventListener("focus", () => {
    syncKeyboardInset();
    updateSearchMode();
  });
  search.addEventListener("click", () => {
    setTimeout(updateSearchMode, 0);
  });
  search.addEventListener("blur", () => {
    setTimeout(updateSearchMode, 0);
  });
  search.addEventListener("input", () => {
    activeIndex = -1;
    renderList();
    if (filtered.length) setActive(0);
    updateSearchMode();
  });
  search.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      if (filtered.length) setActive((activeIndex + 1) % filtered.length);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      if (filtered.length) setActive((activeIndex - 1 + filtered.length) % filtered.length);
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (activeIndex >= 0 && filtered[activeIndex]) choose(filtered[activeIndex].value);
    } else if (event.key === "Escape") {
      event.preventDefault();
      closePopup();
      trigger.focus();
    }
  });

  renderValue();

  return {
    element: root,
    isOpen: () => open,
    // Background polls refresh data, but never while the popup is open —
    // that would yank the list and search box out from under the user.
    // The new value/choices take effect on the next close instead.
    update(nextOption) {
      choices = nextOption.choices || [];
      value = currentComboValue(nextOption, key);
      const disabled = sentConfigChanges.has(key);
      trigger.disabled = disabled;
      if (disabled) closePopup();
      if (!open) renderValue();
    },
  };
}

// The value a combo should display: the optimistic sent value while a
// change is in flight, otherwise whatever the snapshot reports.
function currentComboValue(option, key) {
  const sent = sentConfigChanges.get(key);
  return sent ? sent.value : option.current_value || "";
}

function renderConfig(session) {
  const options = sessionConfigOptions(session);
  updateConfigPanel(session, options);
  // Forget sent changes the session has applied (current_value caught
  // up), that vanished from the snapshot, or that timed out waiting to
  // apply, so the control re-enables.
  if (session) {
    for (const [key, sent] of [...sentConfigChanges.entries()]) {
      if (!key.startsWith(`${session.session_id}\u001f`)) {
        continue;
      }
      const match = options.find(
        (option) => configChangeKey(session.session_id, option) === key,
      );
      if (
        !match
        || match.current_value === sent.value
        || Date.now() - sent.sentAt > CONFIG_APPLY_TIMEOUT_MS
      ) {
        sentConfigChanges.delete(key);
      }
    }
  }
  const desiredKeys = session
    ? options.map((option) => configChangeKey(session.session_id, option))
    : [];
  const currentKeys = [...configCombos.keys()];
  const sameStructure =
    !!session
    && currentKeys.length === desiredKeys.length
    && currentKeys.every((key, index) => key === desiredKeys[index]);

  if (!sameStructure) {
    sessionConfigEl.replaceChildren();
    configCombos.clear();
    if (!session) return;
    for (const option of options) {
      const key = configChangeKey(session.session_id, option);
      const row = cloneTemplate(configOptionTpl);
      row.querySelector(".config-option-name").textContent =
        option.name || option.config_id || "";
      const statusEl = row.querySelector(".config-option-status");
      const combo = createCombobox(option, key, (value) => {
        void submitConfigChange(session.session_id, option, value);
      });
      row.appendChild(combo.element);
      const entry = { combo, statusEl };
      configCombos.set(key, entry);
      applyConfigComboState(entry, key, option);
      sessionConfigEl.appendChild(row);
    }
    return;
  }

  // Same option set as last render: patch the existing combos in place so
  // an open dropdown or in-flight selection survives the poll refresh.
  for (const option of options) {
    const key = configChangeKey(session.session_id, option);
    const entry = configCombos.get(key);
    if (entry) applyConfigComboState(entry, key, option);
  }
}

function applyConfigComboState(entry, key, option) {
  const applying = sentConfigChanges.has(key);
  entry.combo.update(option);
  entry.statusEl.textContent = applying ? "applying…" : "";
}

async function submitConfigChange(sessionId, option, value) {
  const key = configChangeKey(sessionId, option);
  if (sentConfigChanges.get(key)?.value === value) {
    return;
  }
  sentConfigChanges.set(key, { value, sentAt: Date.now() });
  renderChat();
  try {
    const response = await apiFetch("/api/config-changes", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        session_id: sessionId,
        target_kind: option.target_kind,
        config_id: option.config_id ?? null,
        value,
      }),
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
  } catch (error) {
    sentConfigChanges.delete(key);
    if (error.message !== "HTTP 401") {
      queueStatusEl.textContent = `Config change failed: ${error.message}`;
    }
    renderChat();
  }
}

function renderChat() {
  const session = sessions.find((s) => s.session_id === selectedSessionId) || null;
  renderTranscript(session);
  renderConfig(session);
  renderPermissions(session);
  renderQueue(session);
  updateTaskControls();
}

function renderAll() {
  renderSessions();
  renderChat();
  detectNotableTransitions();
}

/* ---- desktop signals: notifications + title badge ---- */

// Last state we compared against, per session, so only *transitions* notify
// (a permission newly pending, a task turn finishing) — never steady state.
const notifiedState = new Map();

function detectNotableTransitions() {
  let totalApprovals = 0;
  for (const session of sessions) {
    const pending = sessionPendingPermissions(session).length;
    totalApprovals += pending;
    const previous = notifiedState.get(session.session_id);
    if (previous && pending > previous.approvals) {
      notify({
        title: "Approval needed",
        body: `${sessionLabel(session)} is waiting for a permission decision.`,
        tag: `mj-approval-${session.session_id}`,
      });
    }
    // Turn-finished only for server-owned tasks: those are the ones the
    // user delegated and walked away from. Local TUI sessions finishing
    // turns while mirrored here would be noise.
    if (
      previous &&
      previous.working &&
      session.working === false &&
      taskBySessionId.has(session.session_id)
    ) {
      notify({
        title: "Task turn finished",
        body: `${sessionLabel(session)} is idle and ready for the next prompt.`,
        tag: `mj-turn-${session.session_id}`,
      });
    }
    notifiedState.set(session.session_id, {
      approvals: pending,
      working: session.working === true,
    });
  }
  for (const id of [...notifiedState.keys()]) {
    if (!sessions.some((s) => s.session_id === id)) {
      notifiedState.delete(id);
    }
  }
  updateTitleBadge(totalApprovals);
}

/* ---- network ---- */

async function tryResumeViewerSession() {
  if (viewerToken) {
    showApp();
    await refreshSessions();
    applyRoute();
    return;
  }
  try {
    const response = await fetch("/live/sessions", {
      credentials: "same-origin",
      cache: "no-store",
    });
    if (response.status === 401) {
      showAuth();
      authTokenEl.focus();
      return;
    }
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }

    sessions = await response.json();
    noteSessionActivity();
    showApp();
    statusEl.textContent = `Updated ${new Date().toLocaleTimeString()}`;
    applyRoute();
    await refreshQueuedPrompts();
  } catch (error) {
    showAuth(`Sign-in check failed: ${error.message}`);
    authTokenEl.focus();
  }
}

async function refreshSessions() {
  if (!authenticated) {
    return;
  }
  // While the event stream is delivering, polling is only a safety
  // net: skip the fetch unless the stream has been quiet too long.
  if (sseConnected && Date.now() - lastSessionsFetch < SSE_POLL_FALLBACK_MS) {
    refreshTimestamps();
    return;
  }
  try {
    const response = await apiFetch("/live/sessions", {
      cache: "no-store",
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    sessions = await response.json();
    lastSessionsFetch = Date.now();
    noteSessionActivity();
    statusEl.textContent = `Updated ${new Date().toLocaleTimeString()}`;
    renderAll();
    refreshTimestamps();
    await refreshQueuedPrompts();
    await refreshServerTasks();
  } catch (error) {
    if (error.message !== "HTTP 401") {
      statusEl.textContent = `Update failed: ${error.message}`;
    }
  }
}

async function refreshQueuedPrompts() {
  if (!authenticated) {
    return;
  }
  const sessionId = selectedSessionId;
  if (!sessionId) {
    queuedPrompts = [];
    renderQueue(null);
    return;
  }
  try {
    const response = await apiFetch(`/api/queued-prompts?session_id=${encodeURIComponent(sessionId)}`, {
      cache: "no-store",
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    queuedPrompts = await response.json();
    const selected = sessions.find((session) => session.session_id === selectedSessionId) || null;
    renderQueue(selected);
  } catch (error) {
    if (error.message !== "HTTP 401") {
      queueStatusEl.textContent = `Queue update failed: ${error.message}`;
    }
  }
}

async function submitQueuedPrompt() {
  const sessionId = selectedSessionId;
  const text = queueInputEl.value;
  if (!sessionId || !text.trim()) {
    return;
  }
  queueSubmitEl.disabled = true;
  try {
    let queuedInstead = false;
    const liveTask = selectedLiveTask();
    let delivered = false;
    if (liveTask) {
      // Live-send to the server-owned task; a 409 means a turn is in
      // flight, so fall back to the queue rather than failing.
      const response = await apiFetch(
        `/api/tasks/${encodeURIComponent(liveTask.task_id)}/prompt`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ text }),
        },
      );
      if (response.ok) {
        delivered = true;
      } else if (response.status !== 409) {
        throw new Error(`HTTP ${response.status}`);
      }
    }
    if (!delivered) {
      const response = await apiFetch("/api/queued-prompts", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          session_id: sessionId,
          text,
        }),
      });
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}`);
      }
      queuedInstead = Boolean(liveTask);
    }
    queueInputEl.value = "";
    draftBySessionId.set(sessionId, "");
    syncComposerHeight();
    pendingNewEntries = 0;
    forceScrollToBottom = true;
    if (queuedInstead) {
      queueStatusEl.textContent = "Task is mid-turn; prompt queued instead.";
    }
    await refreshQueuedPrompts();
  } catch (error) {
    if (error.message !== "HTTP 401") {
      queueStatusEl.textContent = `Prompt submit failed: ${error.message}`;
    }
  } finally {
    queueSubmitEl.disabled = !selectedSessionId;
  }
}

async function cancelSelectedTaskTurn() {
  const task = selectedLiveTask();
  if (!task) {
    return;
  }
  cancelTurnButtonEl.disabled = true;
  try {
    const response = await apiFetch(
      `/api/tasks/${encodeURIComponent(task.task_id)}/cancel`,
      { method: "POST" },
    );
    if (!response.ok) {
      queueStatusEl.textContent = `Cancel failed: HTTP ${response.status}`;
    }
  } catch (error) {
    if (error.message !== "HTTP 401") {
      queueStatusEl.textContent = `Cancel failed: ${error.message}`;
    }
  } finally {
    cancelTurnButtonEl.disabled = false;
  }
}

async function submitAuth(event) {
  event.preventDefault();
  const code = authTokenEl.value;
  if (!code.trim()) {
    authErrorEl.textContent = "Enter the six-digit viewer code from mj server.";
    return;
  }
  authSubmitEl.disabled = true;
  authErrorEl.textContent = "";
  try {
    const response = await fetch("/auth/session", {
      method: "POST",
      credentials: "same-origin",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ code }),
    });
    if (response.status === 401) {
      authErrorEl.textContent = "That viewer code was rejected.";
      return;
    }
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    authTokenEl.value = "";
    showApp();
    await refreshSessions();
    applyRoute();
  } catch (error) {
    authErrorEl.textContent = `Sign-in failed: ${error.message}`;
  } finally {
    authSubmitEl.disabled = false;
  }
}

async function logout() {
  try {
    await fetch("/auth/session", {
      method: "DELETE",
      credentials: "same-origin",
    });
  } finally {
    showAuth("Logged out.");
  }
}

/* ---- server-push events ---- */

function upsertSessionRecord(record) {
  const index = sessions.findIndex((s) => s.session_id === record.session_id);
  if (index >= 0) {
    sessions[index] = record;
  } else {
    sessions.push(record);
  }
  // Keep the server's ordering: newest activity first, id tiebreak.
  sessions.sort(
    (a, b) =>
      (b.last_update || "").localeCompare(a.last_update || "") ||
      (a.session_id || "").localeCompare(b.session_id || ""),
  );
}

function applyServerTasks(tasks) {
  spawnEnabled = tasks != null;
  serverTasks = tasks || [];
  taskBySessionId.clear();
  for (const task of serverTasks) {
    if (task.session_id) {
      taskBySessionId.set(task.session_id, task);
    }
  }
  updateTaskControls();
}

function connectEventStream() {
  if (eventSource || !authenticated) {
    return;
  }
  const url = new URL("/api/events", window.location.origin);
  if (viewerToken) {
    url.searchParams.set("token", viewerToken);
  }
  eventSource = new EventSource(url);
  eventSource.onopen = () => {
    sseConnected = true;
    statusEl.textContent = "Live";
  };
  eventSource.onerror = () => {
    // EventSource reconnects on its own; while it is down the 2s poll
    // takes over again. A 401 cannot be observed here — the poll's 401
    // handling is what returns the app to the sign-in screen.
    sseConnected = false;
  };
  eventSource.addEventListener("snapshot", (event) => {
    const payload = JSON.parse(event.data);
    sessions = Array.isArray(payload.sessions) ? payload.sessions : [];
    applyServerTasks(payload.tasks ?? null);
    lastSessionsFetch = Date.now();
    noteSessionActivity();
    statusEl.textContent = "Live";
    renderAll();
    refreshTimestamps();
  });
  eventSource.addEventListener("session", (event) => {
    const record = JSON.parse(event.data);
    upsertSessionRecord(record);
    lastSessionsFetch = Date.now();
    noteSessionActivity();
    renderAll();
    if (record.session_id === selectedSessionId) {
      void refreshQueuedPrompts();
    }
    // A task's session id becomes known shortly after spawn; refetch
    // the task list once when an unmapped session appears while a
    // task is still waiting for its id.
    if (
      spawnEnabled === true &&
      !taskBySessionId.has(record.session_id) &&
      serverTasks.some((task) => !task.session_id)
    ) {
      void refreshServerTasks();
    }
  });
  eventSource.addEventListener("session_disconnected", (event) => {
    const payload = JSON.parse(event.data);
    sessions = sessions.filter((s) => s.session_id !== payload.session_id);
    lastSessionsFetch = Date.now();
    renderAll();
  });
  eventSource.addEventListener("tasks", (event) => {
    applyServerTasks(JSON.parse(event.data));
  });
}

function disconnectEventStream() {
  if (eventSource) {
    eventSource.close();
    eventSource = null;
  }
  sseConnected = false;
}

/* ---- server tasks ---- */

function updateTaskControls() {
  newTaskButtonEl.hidden = spawnEnabled !== true;
  const task = taskBySessionId.get(selectedSessionId);
  const live = Boolean(task && task.running);
  stopTaskButtonEl.hidden = !live;
  cancelTurnButtonEl.hidden = !(live && sessionIsWorking(selectedSessionId));
}

function selectedLiveTask() {
  const task = taskBySessionId.get(selectedSessionId);
  return task && task.running ? task : null;
}

async function refreshServerTasks() {
  if (!authenticated || spawnEnabled === false) {
    return;
  }
  try {
    const response = await apiFetch("/api/tasks", { cache: "no-store" });
    if (response.status === 403) {
      spawnEnabled = false;
      serverTasks = [];
      taskBySessionId.clear();
      updateTaskControls();
      return;
    }
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    spawnEnabled = true;
    serverTasks = await response.json();
    taskBySessionId.clear();
    for (const task of serverTasks) {
      if (task.session_id) {
        taskBySessionId.set(task.session_id, task);
      }
    }
    updateTaskControls();
  } catch (error) {
    // Task state is auxiliary; the main poll surfaces connectivity
    // problems, so a failed refresh only leaves the last state in place.
    if (error.message === "HTTP 401") {
      throw error;
    }
  }
}

async function openNewTaskDialog() {
  newTaskErrorEl.textContent = "";
  newTaskStartEl.disabled = false;
  try {
    const response = await apiFetch("/api/agents", { cache: "no-store" });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    const agents = await response.json();
    newTaskAgentEl.replaceChildren();
    for (const agent of agents) {
      const option = document.createElement("option");
      option.value = agent.source_id;
      option.textContent =
        agent.kind === "default" ? `${agent.label} (default)` : agent.label;
      newTaskAgentEl.append(option);
    }
    if (!agents.length) {
      newTaskErrorEl.textContent =
        "No agents configured on the server. Run interactive `mj` once to pick one.";
    }
  } catch (error) {
    if (error.message === "HTTP 401") {
      return;
    }
    newTaskAgentEl.replaceChildren();
    newTaskErrorEl.textContent = `Could not load agents: ${error.message}`;
  }
  newTaskDialogEl.showModal();
}

async function submitNewTask(event) {
  event.preventDefault();
  const body = {};
  if (newTaskAgentEl.value) {
    body.agent = newTaskAgentEl.value;
  }
  if (newTaskCwdEl.value.trim()) {
    body.cwd = newTaskCwdEl.value.trim();
  }
  if (newTaskPromptEl.value.trim()) {
    body.prompt = newTaskPromptEl.value.trim();
  }
  newTaskStartEl.disabled = true;
  newTaskErrorEl.textContent = "";
  try {
    const response = await apiFetch("/api/tasks", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!response.ok) {
      newTaskErrorEl.textContent = await response.text();
      newTaskStartEl.disabled = false;
      return;
    }
    newTaskDialogEl.close();
    newTaskPromptEl.value = "";
    await refreshServerTasks();
  } catch (error) {
    if (error.message !== "HTTP 401") {
      newTaskErrorEl.textContent = `Start failed: ${error.message}`;
      newTaskStartEl.disabled = false;
    }
  }
}

async function stopSelectedTask() {
  const task = taskBySessionId.get(selectedSessionId);
  if (!task || !window.confirm(`Stop this task (${task.agent})?`)) {
    return;
  }
  stopTaskButtonEl.disabled = true;
  try {
    const response = await apiFetch(`/api/tasks/${encodeURIComponent(task.task_id)}`, {
      method: "DELETE",
    });
    if (!response.ok && response.status !== 404) {
      statusEl.textContent = `Stop failed: HTTP ${response.status}`;
    }
    await refreshServerTasks();
  } catch (error) {
    if (error.message !== "HTTP 401") {
      statusEl.textContent = `Stop failed: ${error.message}`;
    }
  } finally {
    stopTaskButtonEl.disabled = false;
  }
}

/* ---- wiring ---- */

paletteEl.commandsProvider = () => {
  const commands = [];
  if (spawnEnabled === true) {
    commands.push({
      label: "New task…",
      hint: "spawn an agent",
      run: () => void openNewTaskDialog(),
    });
  }
  const task = selectedLiveTask();
  if (task) {
    if (sessionIsWorking(selectedSessionId)) {
      commands.push({
        label: "Cancel turn",
        hint: task.agent,
        run: () => void cancelSelectedTaskTurn(),
      });
    }
    commands.push({
      label: "Stop task",
      hint: task.agent,
      run: () => void stopSelectedTask(),
    });
  }
  for (const session of sessions) {
    if (session.session_id === selectedSessionId) {
      continue;
    }
    commands.push({
      label: `Go to: ${sessionLabel(session)}`,
      hint: `${session.project || "-"} · ${session.agent || "-"}`,
      run: () => navigateToSession(session.session_id),
    });
  }
  if (notificationsSupported() && !notificationsEnabled()) {
    commands.push({
      label: "Enable desktop notifications",
      hint: "approvals and finished turns",
      run: () => void enableNotifications(),
    });
  }
  commands.push({ label: "Log out", run: () => void logout() });
  return commands;
};
window.addEventListener("keydown", (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    if (authenticated) {
      paletteEl.toggle();
    }
  }
});

authFormEl.addEventListener("submit", submitAuth);
logoutButtonEl.addEventListener("click", logout);
backButtonEl.addEventListener("click", navigateToList);
sessionConfigPanelEl.addEventListener("toggle", () => {
  if (expectedConfigPanelOpen === sessionConfigPanelEl.open) {
    expectedConfigPanelOpen = null;
    return;
  }
  expectedConfigPanelOpen = null;
  configPanelUserToggled = true;
});
window.addEventListener("hashchange", applyRoute);
narrowScreen.addEventListener("change", () => {
  configPanelUserToggled = false;
  if (authenticated) {
    renderAll();
  }
});
window.addEventListener("resize", syncKeyboardInset);
if (window.visualViewport) {
  window.visualViewport.addEventListener("resize", syncKeyboardInset);
  window.visualViewport.addEventListener("scroll", syncKeyboardInset);
}
jumpToLatestEl.addEventListener("click", () => {
  pendingNewEntries = 0;
  scrollTranscriptToBottom();
});
transcriptEl.addEventListener("scroll", () => {
  if (isNearTranscriptBottom()) {
    pendingNewEntries = 0;
  }
  updateJumpToLatestVisibility();
}, { passive: true });
newTaskButtonEl.addEventListener("click", () => {
  void openNewTaskDialog();
});
newTaskFormEl.addEventListener("submit", submitNewTask);
newTaskCancelEl.addEventListener("click", () => newTaskDialogEl.close());
stopTaskButtonEl.addEventListener("click", () => {
  void stopSelectedTask();
});
cancelTurnButtonEl.addEventListener("click", () => {
  void cancelSelectedTaskTurn();
});
queueSubmitEl.addEventListener("click", submitQueuedPrompt);
queueInputEl.addEventListener("input", () => {
  saveDraftForSelectedSession();
  syncComposerHeight();
});
queueInputEl.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey && !event.metaKey && !event.ctrlKey && !event.altKey) {
    event.preventDefault();
    submitQueuedPrompt();
    return;
  }
  if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
    event.preventDefault();
    submitQueuedPrompt();
  }
});

syncComposerHeight();
syncKeyboardInset();
applyRoute();
void tryResumeViewerSession();
