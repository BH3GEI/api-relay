#!/bin/sh
# Entry point inside the LazyCat app container.
# LAZYCAT=1 trusts the identity headers LazyCat's gateway sets; only safe behind that gateway.
export LAZYCAT=1
export DATABASE_URL="sqlite:/lzcapp/var/relay.db?mode=rwc"
export BIND_ADDR=127.0.0.1:3000
# Listener for the Cloudflare Tunnel; it bypasses LazyCat's gateway, so it serves only the API and the password-login home page
export PUBLIC_BIND_ADDR=127.0.0.1:3001
export CLOUDFLARED_PATH=/lzcapp/pkg/content/cloudflared
# Tunnel connects, drops and restarts, kept across upgrades
export TUNNEL_LOG=/lzcapp/var/tunnel.log
exec /lzcapp/pkg/content/api-relay
