#!/bin/sh
# Build the LazyCat package content: a static linux/amd64 binary plus its start script.
# Needs the musl cross toolchain (macOS: brew install filosottile/musl-cross/musl-cross).
set -e
cd "$(dirname "$0")/.."
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
cargo build --release --target x86_64-unknown-linux-musl
rm -rf lazycat/content
mkdir -p lazycat/content
cp target/x86_64-unknown-linux-musl/release/api-relay lazycat/start.sh lazycat/content/
chmod +x lazycat/content/api-relay lazycat/content/start.sh
