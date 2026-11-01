#!/usr/bin/env bash
# Build branchdiff for macOS and install it into ~/projects/tools.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --release

DEST="$HOME/projects/tools"
mkdir -p "$DEST"
cp -f target/release/branchdiff "$DEST/branchdiff"

echo "deployed: $DEST/branchdiff"
"$DEST/branchdiff" --version