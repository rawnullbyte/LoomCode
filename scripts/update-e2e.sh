#!/usr/bin/env bash
# End-to-end check of the release ⇄ updater contract, outside the test suite.
#
# Serves a fake GitHub Releases API whose assets are laid out exactly as
# `.github/workflows/release.yml` produces them, then runs the real `loom` binary from a
# scratch install directory and asserts it replaced itself with the published binary.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(cargo metadata --no-deps --format-version 1 \
  | python3 -c 'import json,sys; pkgs=json.load(sys.stdin)["packages"]; print(next(p["version"] for p in pkgs if p["name"]=="loon-cli"))')"
TARGET="$(cat target/release/build/loon-cli-*/output 2>/dev/null \
  | grep -o 'LOON_BUILD_TARGET=[a-z0-9_-]*' | head -1 | cut -d= -f2)"
[ -n "$TARGET" ] || { echo "FAIL: LOON_BUILD_TARGET not found in build output"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
DIST="$WORK/release"
mkdir -p "$DIST"

# ── Stage the assets the way the workflow packages them ──────────────────────────────
NEW_VERSION="9.9.9"
NAME="loom-${NEW_VERSION}-${TARGET}"
printf '#!/bin/sh\necho "loom %s"\n' "$NEW_VERSION" > "$DIST/$NAME"
chmod 0755 "$DIST/$NAME"
( cd "$DIST" && sha256sum "$NAME" > "$NAME.sha256" )

# ── Serve them, plus a release document that points at this server ───────────────────
PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
BASE="http://127.0.0.1:$PORT"

python3 - "$PORT" "$DIST" "$NAME" "$NEW_VERSION" <<'PY' &
import http.server, json, os, sys
port, dist, name, version = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
base = f"http://127.0.0.1:{port}"

release = {
    "tag_name": f"v{version}",
    "prerelease": False,
    "draft": False,
    "assets": [
        {"name": name, "browser_download_url": f"{base}/download/{name}"},
        {"name": f"{name}.sha256", "browser_download_url": f"{base}/download/{name}.sha256"},
        {"name": f"{name}.tar.gz", "browser_download_url": f"{base}/download/{name}.tar.gz"},
    ],
}

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path == "/repos/owner/repo/releases?per_page=10":
            body = json.dumps([release]).encode()
        elif self.path.startswith("/download/"):
            asset = os.path.join(dist, self.path.rsplit("/", 1)[1])
            if not os.path.exists(asset):
                self.send_error(404)
                return
            body = open(asset, "rb").read()
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

http.server.HTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
SERVER=$!
trap 'kill $SERVER 2>/dev/null; rm -rf "$WORK"' EXIT
sleep 0.4

# ── Install a scratch copy at "9.9.8" and let it update itself ───────────────────────
INSTALL="$WORK/bin"
mkdir -p "$INSTALL"
cp target/release/loom "$INSTALL/loom"
BEFORE="$(sha256sum "$INSTALL/loom" | cut -d' ' -f1)"

OUTPUT="$(LOON_HOME="$WORK/home" \
  LOON_UPDATE_API_BASE="$BASE" \
  LOON_UPDATE_REPOSITORY="owner/repo" \
  LOON_UPDATE=auto \
  "$INSTALL/loom" update --no-restart 2>&1)"
echo "$OUTPUT"
AFTER_BIN="$INSTALL/loom"

# ── Assertions ──────────────────────────────────────────────────────────────────────
fail() { echo "FAIL: $*"; exit 1; }

grep -q "$NEW_VERSION" <<<"$OUTPUT" || fail "the command did not report $NEW_VERSION"
EXPECTED="$(sha256sum "$DIST/$NAME" | cut -d' ' -f1)"
ACTUAL="$(sha256sum "$AFTER_BIN" | cut -d' ' -f1)"
[ "$ACTUAL" = "$EXPECTED" ] || fail "installed binary is not the published asset"
[ "$ACTUAL" != "$BEFORE" ] || fail "the binary was not replaced"
[ -x "$AFTER_BIN" ] || fail "the installed binary is not executable"
"$AFTER_BIN" >/dev/null 2>&1 || fail "the installed binary does not run"
REPORTED="$("$AFTER_BIN" 2>/dev/null)"
grep -q "loom $NEW_VERSION" <<<"$REPORTED" || fail "wrong version runs: $REPORTED"
ls -a "$INSTALL" | grep -q '^\.loom' && fail "a staging file was left behind"

echo
echo "PASS: $TARGET replaced $BEFORE with $ACTUAL and runs as $NEW_VERSION"