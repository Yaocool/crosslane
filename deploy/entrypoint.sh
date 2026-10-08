#!/bin/sh
set -eu

if [ "$#" -gt 0 ]; then
  exec "$@"
fi

# Optional compatibility forwarder for bridge-network deployments. The default
# Compose setup uses host networking and does not set CROSSLANE_FORWARD_PORT.
if [ -n "${CROSSLANE_FORWARD_PORT:-}" ]; then
  internal_port="${CROSSLANE_HTTP_PORT:-5580}"
  socat "TCP-LISTEN:${CROSSLANE_FORWARD_PORT},bind=0.0.0.0,reuseaddr,fork" \
    "TCP:127.0.0.1:${internal_port}" &
fi

exec /usr/local/bin/crosslaned
