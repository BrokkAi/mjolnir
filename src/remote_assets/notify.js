// Desktop notifications and the title/app badge for the Mjolnir Web viewer.
//
// Notifications fire only when the tab is hidden — while the app is visible
// its own badges are the signal — and only after the user enabled them via
// an explicit gesture (the palette command), which is also what browsers
// require for the permission prompt.

const STORAGE_KEY = "mj-notify-enabled";

let enabled = false;
try {
  enabled =
    localStorage.getItem(STORAGE_KEY) === "1" &&
    "Notification" in window &&
    Notification.permission === "granted";
} catch {
  // Storage can be unavailable (private mode); notifications just start off.
}

export function notificationsSupported() {
  return "Notification" in window;
}

export function notificationsEnabled() {
  return enabled && Notification.permission === "granted";
}

// Must be called from a user gesture the first time, or the permission
// prompt is silently denied by the browser.
export async function enableNotifications() {
  if (!notificationsSupported()) {
    return false;
  }
  if (Notification.permission === "default") {
    await Notification.requestPermission();
  }
  enabled = Notification.permission === "granted";
  try {
    localStorage.setItem(STORAGE_KEY, enabled ? "1" : "0");
  } catch {
    // Best effort; the in-memory flag still applies for this page.
  }
  return enabled;
}

export function notify({ title, body, tag }) {
  if (!notificationsEnabled() || !document.hidden) {
    return;
  }
  const notification = new Notification(title, {
    body,
    tag,
    icon: "/icons/icon-192.png",
  });
  notification.onclick = () => {
    window.focus();
    notification.close();
  };
}

// Reflect the number of pending approvals in the tab title and, where the
// PWA is installed, the OS app badge.
export function updateTitleBadge(count) {
  const base = "Mjolnir Web";
  document.title = count > 0 ? `(${count}) ${base}` : base;
  if (count > 0) {
    navigator.setAppBadge?.(count);
  } else {
    navigator.clearAppBadge?.();
  }
}
