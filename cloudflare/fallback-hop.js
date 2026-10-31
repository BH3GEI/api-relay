// The fallback Worker's way out to frp, on its workers.dev address. A Worker on the zone's route
// can't reach another hostname in the same zone (Cloudflare answers 520/521 for the DNS-only frp
// name, service bindings included); this one, outside the zone, can.
//
// Env: FALLBACK_URL and FORWARD_SECRET, as for the fallback Worker. It only passes on requests
// carrying the secret, and only to FALLBACK_URL, so it is no open proxy.
export default {
  async fetch(req, env) {
    if (!env.FORWARD_SECRET || req.headers.get("x-relay-forward-secret") !== env.FORWARD_SECRET) {
      return new Response("Not found", { status: 404 });
    }
    const url = new URL(req.url);
    return fetch(new URL(url.pathname + url.search, env.FALLBACK_URL), req);
  },
};
