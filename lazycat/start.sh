#!/bin/sh
# Entry point inside the LazyCat app container.
# LAZYCAT=1 trusts the identity headers LazyCat's gateway sets; only safe behind that gateway.
export LAZYCAT=1
export DATABASE_URL="sqlite:/lzcapp/var/relay.db?mode=rwc"
export BIND_ADDR=127.0.0.1:3000
# API-only listener for the Cloudflare Tunnel; it bypasses LazyCat's gateway, so it serves no pages, /auth or /admin
export PUBLIC_BIND_ADDR=127.0.0.1:3001
export CLOUDFLARED_PATH=/lzcapp/pkg/content/cloudflared
exec /lzcapp/pkg/content/api-relay
