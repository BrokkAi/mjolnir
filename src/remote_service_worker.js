// Service worker for the Mjolnir Web PWA.
//
// Served from the site root so its scope is "/" and it can control the app
// shell. The strategy is deliberately conservative:
//   * Never touch authenticated or live data. /api/, /live/, and /auth/ always
//     go straight to the network, uncached — caching them would serve stale or
//     auth-sensitive responses.
//   * The shell ("/"), manifest, and icons use network-first with a cache
//     fallback, so an online client always gets fresh markup while an offline
//     launch still renders (API calls then fail into the normal sign-in screen).

const CACHE = "mjolnir-shell-v3";
const SHELL = [
  "/",
  "/manifest.webmanifest",
  "/icons/icon.svg",
  "/icons/icon-192.png",
  "/icons/icon-512.png",
  "/icons/maskable-512.png",
  "/icons/apple-touch-icon.png",
  "/fonts/staatliches-400.woff2",
  "/fonts/rajdhani-500.woff2",
  "/fonts/rajdhani-600.woff2",
  "/fonts/rajdhani-700.woff2",
  "/fonts/jetbrains-mono.woff2",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key))),
      )
      .then(() => self.clients.claim()),
  );
});

function isDynamic(url) {
  return (
    url.pathname.startsWith("/api/") ||
    url.pathname.startsWith("/live/") ||
    url.pathname.startsWith("/auth/")
  );
}

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") {
    return;
  }
  const url = new URL(request.url);
  if (url.origin !== self.location.origin || isDynamic(url)) {
    return; // Leave auth/live/cross-origin requests entirely to the network.
  }
  event.respondWith(
    fetch(request)
      .then((response) => {
        if (response && response.ok) {
          const copy = response.clone();
          caches.open(CACHE).then((cache) => cache.put(request, copy));
        }
        return response;
      })
      .catch(() => caches.match(request).then((cached) => cached || caches.match("/"))),
  );
});
