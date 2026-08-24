#!/bin/sh
set -e

cargo build --release
cargo install --path .

if command -v codex >/dev/null 2>&1; then
  plugin_dir="${HOME}/plugins/reviewer-codex"
  marketplace_dir="${HOME}/.agents/plugins"
  rm -rf "$plugin_dir"
  mkdir -p "$plugin_dir" "$marketplace_dir"
  cp -R plugins/reviewer-codex/. "$plugin_dir/"
  if [ ! -f "$marketplace_dir/marketplace.json" ]; then
    cp plugins/marketplace.json "$marketplace_dir/marketplace.json"
  fi
  plugin_version="0.1.0+codex.$(date -u +%Y%m%d%H%M%S)"
  perl -0pi -e 's/"version":\s*"[^"]+"/"version": "'"$plugin_version"'"/' "$plugin_dir/.codex-plugin/plugin.json"
  codex plugin add reviewer-codex@personal
fi
