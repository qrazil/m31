#!/usr/bin/env bash
# Build m31ssh from a checkout: `bash apps/m31ssh/build.sh [-o NAME]`
# (the same as `bash apps/build.sh m31ssh [-o NAME]`).
exec bash "$(dirname "$0")/../build.sh" m31ssh "$@"
