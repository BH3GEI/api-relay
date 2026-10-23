#!/bin/sh
# Build the LazyCat package content: a static linux/amd64 binary, cloudflared, and the start script.
# Needs the musl cross toolchain (macOS: brew install filosottile/musl-cross/musl-cross).
set -e
cd "$(dirname "$0")/.."
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
cargo build --release --target x86_64-unknown-linux-musl

# cloudflared, pinned and checked against the hash in Cloudflare's release notes
CLOUDFLARED_VERSION=2026.10.0
CLOUDFLARED_SHA256=d33ff2d14475178d2012c2c56beba87389ac5ded27649519f198a7d3134a99db
CLOUDFLARED=lazycat/cache/cloudflared-$CLOUDFLARED_VERSION
if [ ! -f "$CLOUDFLARED" ]; then
  mkdir -p lazycat/cache
  curl -fsSL -o "$CLOUDFLARED.tmp" \
    "https://github.com/cloudflare/cloudflared/releases/download/$CLOUDFLARED_VERSION/cloudflared-linux-amd64"
  mv "$CLOUDFLARED.tmp" "$CLOUDFLARED"
fi
echo "$CLOUDFLARED_SHA256  $CLOUDFLARED" | shasum -a 256 -c -

rm -rf lazycat/content
mkdir -p lazycat/content
cp target/x86_64-unknown-linux-musl/release/api-relay lazycat/start.sh lazycat/content/
cp "$CLOUDFLARED" lazycat/content/cloudflared
# cloudflared is Apache-2.0; its license travels with the binary
cp lazycat/cloudflared.LICENSE LICENSE lazycat/content/
chmod +x lazycat/content/api-relay lazycat/content/cloudflared lazycat/content/start.sh
