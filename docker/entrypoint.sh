#!/usr/bin/env bash
set -Eeuo pipefail

# espup records the Xtensa LLVM and GCC paths here.
# shellcheck disable=SC1091
source /opt/esp/export-esp.sh

exec "$@"
