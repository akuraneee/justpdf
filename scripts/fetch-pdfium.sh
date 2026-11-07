#!/usr/bin/env bash
# Downloads prebuilt PDFium (https://github.com/bblanchon/pdfium-binaries) into third_party/pdfium/<platform>/.
# Usage: scripts/fetch-pdfium.sh android-arm64 android-arm android-x64 linux-x64
set -euo pipefail
PDFIUM_TAG="${PDFIUM_TAG:-chromium/8100}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/third_party/pdfium"
mkdir -p "$DEST"
for plat in "$@"; do
  dir="$DEST/$plat"
  if [[ -f "$dir/.tag" && "$(cat "$dir/.tag")" == "$PDFIUM_TAG" ]]; then
    continue
  fi
  echo "fetching pdfium $PDFIUM_TAG for $plat"
  rm -rf "$dir" && mkdir -p "$dir"
  url="https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_TAG/\//%2F}/pdfium-$plat.tgz"
  curl -sSfL --retry 3 "$url" | tar -xz -C "$dir"
  echo "$PDFIUM_TAG" > "$dir/.tag"
done
