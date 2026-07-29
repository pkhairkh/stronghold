// Stronghold service worker — app shell caching + offline fallback.
//
// Implemented in: W8-T6
//
// Scope: /static/ (registered from /setup page)
// Strategy:
//   - Pre-cache the PWA shell (manifest, icons) on install
//   - Network-first for navigation requests (always try fresh /setup)
//   - Cache-first for same-origin static assets, with runtime cache fill
//   - Pass-through (no caching) for API/SSE/WebSocket requests

const CACHE_NAME = 'stronghold-shell-v1';

// Pre-cached at install time. /setup is served dynamically by the gateway
// (include_str! in Rust), so we cache the rendered response opportunistically
// at runtime instead of pre-caching here.
const PRE_CACHE_URLS = [
  '/static/manifest.json',
  '/static/icon.svg',
  '/static/icon-maskable.svg',
];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE_NAME)
      .then((cache) => cache.addAll(PRE_CACHE_URLS))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(
        keys.filter((k) => k !== CACHE_NAME).map((k) => caches.delete(k))
      ))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;

  // Never intercept non-GET requests (POST/PUT for API calls go straight through)
  if (req.method !== 'GET') return;

  const url = new URL(req.url);

  // Only handle same-origin requests; let cross-origin pass through
  if (url.origin !== self.location.origin) return;

  // Skip SSE streams and PTY websockets — they must be live
  if (req.headers.get('accept') === 'text/event-stream') return;
  if (req.mode === 'websocket') return;

  // Network-first for navigations (so users get fresh HTML when online)
  if (req.mode === 'navigate') {
    event.respondWith(
      fetch(req)
        .then((response) => {
          // Cache a copy of the navigated page for offline fallback
          if (response && response.ok) {
            const clone = response.clone();
            caches.open(CACHE_NAME).then((cache) => cache.put(req, clone));
          }
          return response;
        })
        .catch(() => caches.match(req).then((cached) => cached || caches.match('/setup')))
    );
    return;
  }

  // Cache-first for everything else, with runtime cache fill
  event.respondWith(
    caches.match(req).then((cached) => {
      if (cached) return cached;
      return fetch(req).then((response) => {
        // Only cache successful, basic (CORS-same-origin) responses
        if (response && response.ok && response.type === 'basic') {
          const clone = response.clone();
          caches.open(CACHE_NAME).then((cache) => cache.put(req, clone));
        }
        return response;
      }).catch(() => cached);
    })
  );
});
