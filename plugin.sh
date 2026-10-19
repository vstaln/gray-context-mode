#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-context-mode.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-context-mode" ;;
    *) rel="x86_64-apple-darwin/gray-context-mode" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-context-mode" ;;
    *) rel="x86_64-unknown-linux-musl/gray-context-mode" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-context-mode.exe" "$here/target/release/gray-context-mode" "$here/target/debug/gray-context-mode.exe" "$here/target/debug/gray-context-mode"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-context-mode binary not found in $here" >&2
exit 1
