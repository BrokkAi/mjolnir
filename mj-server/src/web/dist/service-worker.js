// Service worker for the Mjolnir web app.
//
// Rules:
// - /api/ and /auth/ are never touched. They carry live state and credentials,
//   so this worker does not call respondWith for them at all.
// - /assets/* files have content-hashed names, so a cached copy never goes
//   stale: serve from the cache first and fill it from the network once.
// - Everything else that is a same-origin GET (the HTML shell, the manifest,
//   icons) goes to the network first and falls back to the cache only when the
//   network fails, so an upgraded daemon is never shadowed by an old shell.
//
// CACHE_VERSION must change whenever this file's caching rules change.
// Activation deletes every other cache.
importScripts('/push-sync.js');
const CACHE_VERSION = 'mjolnir-spa-v1';

self.addEventListener('install', event => {
  event.waitUntil(self.skipWaiting());
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

async function cacheFirst(request) {
  const cache = await caches.open(CACHE_VERSION);
  const cached = await cache.match(request);
  if (cached) return cached;
  const response = await fetch(request);
  if (response.ok) await cache.put(request, response.clone());
  return response;
}

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
  // The worker script itself must always be revalidated by the browser.
  if (url.pathname === '/service-worker.js') return;
  if (url.pathname.startsWith('/assets/')) {
    event.respondWith(cacheFirst(event.request));
    return;
  }
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
      data: { url: notice.url || '/', session_id: notice.session_id || null, episode: notice.episode || null, observed_revision: notice.observed_revision || 0 },
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
      // Use the app's router: a document navigation would erase drafts.
      client.postMessage({ type: 'mj-notification-open', url: target.href });
      await client.focus();
      return;
    }
    await self.clients.openWindow(target.href);
  })());
});

// Reconcile durable device intent even when no app window is open.
self.addEventListener('pushsubscriptionchange', event => {
  const update = MjPush.reconcile(self.registration, event.oldSubscription?.endpoint);
  event.waitUntil(update.catch(error => {
    console.warn('Notification subscription update failed', error);
    throw error;
  }));
});
