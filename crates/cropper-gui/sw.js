// Serves exported PDFs from the Cache API under a readable URL
// (`<scope>_pdf/<id>/<file name>.pdf`) instead of an opaque blob: URL.
const CACHE = "cropper-pdf";
const KEEP = 3;

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("message", (event) => {
  const { url, bytes } = event.data || {};
  const port = event.ports[0];
  if (!url || !bytes) return;
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      await cache.put(
        url,
        new Response(bytes, {
          headers: { "Content-Type": "application/pdf", "Content-Disposition": "inline" },
        })
      );
      // Keep only the most recent few exports.
      const keys = await cache.keys();
      for (const stale of keys.slice(0, Math.max(0, keys.length - KEEP))) {
        await cache.delete(stale);
      }
      port?.postMessage("ok");
    })().catch(() => port?.postMessage("error"))
  );
});

self.addEventListener("fetch", (event) => {
  if (new URL(event.request.url).pathname.includes("/_pdf/")) {
    event.respondWith(caches.match(event.request).then((r) => r || new Response("Not found", { status: 404 })));
  }
});
