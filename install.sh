#!/bin/sh
# Installs toss on a Mac, from Terminal:
#   curl -fsSL https://raw.githubusercontent.com/plamen-g/toss/master/install.sh | sh
# It downloads the newest release's toss.app from GitHub, puts it in Applications and opens it.
# A download made by curl isn't marked as coming from the internet, so macOS doesn't ask about it.
set -eu
[ "$(uname -s)" = Darwin ] || { echo "This is for a Mac. Elsewhere, see https://github.com/plamen-g/toss#install"; exit 1; }
tmp=$(mktemp -d)
echo "Downloading toss..."
curl -fsSL -o "$tmp/toss.zip" https://github.com/plamen-g/toss/releases/latest/download/toss-macos.zip
ditto -x -k "$tmp/toss.zip" "$tmp"
osascript -e 'quit app "toss"' >/dev/null 2>&1 || true # an older toss that's running
rm -rf /Applications/toss.app
ditto "$tmp/toss.app" /Applications/toss.app
rm -rf "$tmp"
open /Applications/toss.app
echo "toss is in Applications, and in the menu bar."
