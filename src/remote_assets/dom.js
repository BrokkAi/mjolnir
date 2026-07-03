// Shared DOM, viewport, and timestamp helpers for the Mjolnir Web
// viewer. Pure with respect to app state: nothing in this module
// reads or writes session/task data.

export const $ = (id) => document.getElementById(id);
export const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
export const narrowScreen = window.matchMedia("(max-width: 800px)");
export const relFmt = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

export function scrollBehavior() {
  return reducedMotion.matches ? "auto" : "smooth";
}

export function syncKeyboardInset() {
  const viewport = window.visualViewport;
  const inset = viewport
    ? Math.max(0, window.innerHeight - viewport.height - viewport.offsetTop)
    : 0;
  document.documentElement.style.setProperty("--keyboard-inset", `${Math.round(inset)}px`);
}

export function isNarrowScreen() {
  return narrowScreen.matches || window.innerWidth <= 800;
}

export function withViewTransition(update) {
  if (document.startViewTransition && !reducedMotion.matches) {
    document.startViewTransition(update);
  } else {
    update();
  }
}

export function cloneTemplate(template) {
  return template.content.firstElementChild.cloneNode(true);
}

export function emptyNote(text) {
  const div = document.createElement("div");
  div.className = "empty";
  div.textContent = text;
  return div;
}

export function formatRelative(value) {
  const date = new Date(value);
  if (!value || Number.isNaN(date.getTime())) {
    return value || "";
  }
  const seconds = Math.round((date.getTime() - Date.now()) / 1000);
  const abs = Math.abs(seconds);
  if (abs < 45) {
    return "just now";
  }
  if (abs < 3600) {
    return relFmt.format(Math.trunc(seconds / 60), "minute");
  }
  if (abs < 86400) {
    return relFmt.format(Math.trunc(seconds / 3600), "hour");
  }
  return relFmt.format(Math.trunc(seconds / 86400), "day");
}

export function formatAbsolute(value) {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value || "" : date.toLocaleString();
}

export function setTimestamp(el, value) {
  el.dataset.ts = value || "";
  el.title = value ? formatAbsolute(value) : "";
  el.textContent = value ? formatRelative(value) : "";
}

export function refreshTimestamps() {
  for (const el of document.querySelectorAll("[data-ts]")) {
    if (el.dataset.ts) {
      el.textContent = formatRelative(el.dataset.ts);
    }
  }
}
