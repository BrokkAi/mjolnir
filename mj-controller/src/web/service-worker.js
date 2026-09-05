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
const CACHE_VERSION = 'mjolnir-shell-v6';
const SHELL = ['/', '/viewer.css', '/viewer.js', '/manifest.webmanifest', '/icon.svg'];

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
