// Oxid dashboard service worker.
//
// The panel is a handful of static files served from the same binary as the
// API, so making it work offline is mostly a matter of not asking twice.
// What this buys, in order of how much it matters:
//
//   - The shell opens instantly and without the network. A devops checking
//     the fleet from a phone on a train gets the panel, and then whatever
//     the API can or cannot answer — rather than a browser error page.
//   - Installed as an app, it has to survive being opened with no
//     connection at all; a PWA that white-screens offline is worse than a
//     bookmark.
//
// What it deliberately does NOT do is cache API responses. Everything under
// /api/ is the live state of a cluster — a cached environment list is a
// lie, and a lie about whether something is running is worse than an error.
// Those requests go to the network and fail honestly when it is not there.

const VERSION = "oxid-v1";

// The whole shell. Small enough to fetch in one go on install, which is
// what makes the first offline open work rather than the second.
const SHELL = [
  "/",
  "/style.css",
  "/app.js",
  "/i18n.js",
  "/vendor/alpine.min.js",
  "/manifest.webmanifest",
  "/icon.svg",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(VERSION)
      // `addAll` rejects the whole install if any single file 404s, which
      // would leave no worker at all. Individually, a missing asset costs
      // only that asset.
      .then((cache) => Promise.allSettled(SHELL.map((url) => cache.add(url))))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== VERSION).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") {
    return;
  }
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) {
    return;
  }
  // Live state: never served from a cache. See the note at the top.
  if (url.pathname.startsWith("/api/")) {
    return;
  }

  // A deep link like /ui/projects/3 is the SPA shell; the daemon already
  // serves index.html for any unmatched GET, and offline we do the same
  // from the cache so a refresh on a nested route still opens.
  if (request.mode === "navigate") {
    event.respondWith(
      fetch(request).catch(() => caches.match("/", { ignoreSearch: true })),
    );
    return;
  }

  // Static assets: cache first, because they only change when the daemon
  // is replaced — and then revalidate in the background so the next load
  // picks up a new build without waiting for a cache version bump.
  event.respondWith(
    caches.match(request).then((hit) => {
      const live = fetch(request)
        .then((res) => {
          if (res.ok) {
            const copy = res.clone();
            caches.open(VERSION).then((cache) => cache.put(request, copy));
          }
          return res;
        })
        .catch(() => hit);
      return hit || live;
    }),
  );
});
