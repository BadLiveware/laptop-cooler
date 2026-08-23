#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/laptop-cooler"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

"$ROOT_DIR/dev" build
install -Dm755 "$ROOT_DIR/target/x86_64-unknown-linux-gnu/release/laptop-cooler-agent" "$HOME/.local/bin/laptop-cooler-agent"
install -Dm644 "$ROOT_DIR/laptop-cooler-agent.service" "$SYSTEMD_DIR/laptop-cooler-agent.service"
mkdir -p "$CONFIG_DIR"
if [[ ! -e "$CONFIG_DIR/config" ]]; then
  install -m600 "$ROOT_DIR/config.example" "$CONFIG_DIR/config"
  printf 'Created %s; set its token before starting the service.\n' "$CONFIG_DIR/config"
fi

systemctl --user daemon-reload
printf 'Test once with: %s --once\n' "$HOME/.local/bin/laptop-cooler-agent"
printf 'Enable with: systemctl --user enable --now laptop-cooler-agent.service\n'
