// Offline shell for the Mjolnir viewer.
//
// Two rules matter here. Requests under /api/ are never touched: they carry
// live session state and a cached answer would be a lie, so this worker does
// not call respondWith for them at all and lets the network handle them.
// Navigations go to the network first and fall back to the cached shell only
// when the network fails, so a phone that has been offline cannot keep serving
// an old application after the daemon is upgraded.
//
// CACHE_VERSION must change whenever any shell asset changes. Activation
// deletes every cache that is not the current one, so an upgrade cannot leave
// a previous version's assets behind.
const CACHE_VERSION = 'mjolnir-shell-v32';
const SHELL = [
  '/',
  '/viewer.css',
  '/viewer.js',
  '/manifest.webmanifest',
  '/icon.svg',
];

self.addEventListener('install', event => {
  event.waitUntil(
    caches
      .open(CACHE_VERSION)
      .then(cache => cache.addAll(SHELL))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', event => {
  event.waitUntil(
    caches
      .keys()
      .then(names =>
        Promise.all(names.filter(name => name !== CACHE_VERSION).map(name => caches.delete(name))),
      )
      .then(() => self.clients.claim()),
  );
});

async function networkFirst(request) {
  try {
    const response = await fetch(request);
    if (response.ok) {
      const cache = await caches.open(CACHE_VERSION);
      await cache.put(request, response.clone());
    }
    return response;
  } catch (error) {
    const cached = await caches.match(request);
    if (cached) return cached;
    throw error;
  }
}

self.addEventListener('fetch', event => {
  if (event.request.method !== 'GET') return;
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;
  // Live state and authentication never come from a cache.
  if (url.pathname.startsWith('/api/') || url.pathname.startsWith('/auth/')) return;
  event.respondWith(networkFirst(event.request));
});

// Every push displays a notification, including on iOS; no silent withdrawals.
self.addEventListener('push', event => {
  event.waitUntil((async () => {
    const notice = event.data ? event.data.json() : {};
    await self.registration.showNotification(notice.title || 'Mjolnir', {
      body: notice.body || 'Needs your input',
      renotify: true,
      tag: notice.tag || notice.session_id || 'mjolnir',
      data: { url: notice.url || '/', session_id: notice.session_id || null },
    });
  })());
});

self.addEventListener('notificationclick', event => {
  event.notification.close();
  event.waitUntil((async () => {
    const target = new URL(event.notification.data?.url || '/', self.location.origin);
    if (target.origin !== self.location.origin) return;
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const client of windows) {
      const current = new URL(client.url);
      if (current.origin !== target.origin || current.pathname !== '/') continue;
      // Use the viewer's router: a document navigation would erase drafts.
      client.postMessage({ type: 'mj-notification-open', url: target.href });
      await client.focus();
      return;
    }
    await self.clients.openWindow(target.href);
  })());
});

// Browser-driven refresh runs even with no viewer window. Use the current
// native subscription, so an older event cannot restore a device since disabled.
let subscriptionUpdates = Promise.resolve();
self.addEventListener('pushsubscriptionchange', event => {
  const update = subscriptionUpdates.then(async () => {
    const current = await self.registration.pushManager.getSubscription();
    const oldEndpoint = event.oldSubscription?.endpoint;
    let action;
    if (!current) {
      if (event.newSubscription || !oldEndpoint) return;
      action = { action: 'push-unsubscribe', endpoint: oldEndpoint };
    } else {
      action = oldEndpoint
        ? { action: 'push-renew', old_endpoint: oldEndpoint, subscription: current.toJSON() }
        : { action: 'push-subscribe', subscription: current.toJSON() };
    }
    const response = await fetch('/api/actions', {
      method: 'POST', credentials: 'include',
      headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(action),
    });
    if (!response.ok) throw new Error(`Could not update notification subscription (HTTP ${response.status}). Open the viewer to check its sign-in and device notification state.`);
  });
  // Rotation events for one native registration have one ordered owner. The
  // event still observes rejection; the queue reports it and admits later work.
  subscriptionUpdates = update.catch(error => console.warn('Notification subscription update failed', error));
  event.waitUntil(update);
});
