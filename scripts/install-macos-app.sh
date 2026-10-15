#!/usr/bin/env bash
# Build DBX and install or update it in /Applications (macOS).
#
# Your data and settings are never touched: connections, settings, history
# and the encryption key live outside the app bundle, in
#   ~/Library/Application Support/com.dbx.app   (database, settings)
#   ~/Library/Application Support/dbx/secret.key (local encryption key)
# and the installed app uses the same identifier (com.dbx.app) as `make dev`,
# so both see the same connections. A copy of those files is taken before
# every install anyway (last 5 kept in ~/dbx-backups/install).
#
# Usage: scripts/install-macos-app.sh [options]
#   --full         Release-grade build (fat LTO, 1 codegen unit). Slow; the
#                  default is a faster local build (thin LTO, 16 units).
#   --skip-build   Install the last built bundle without rebuilding.
#   --no-launch    Do not open DBX after installing.
#   --force        Quit DBX even if it does not close within 30 s.
#   -h, --help     Show this help.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="DBX.app"
INSTALL_DIR="/Applications"
INSTALLED_APP="$INSTALL_DIR/$APP_NAME"
BUNDLE="$REPO_ROOT/target/release/bundle/macos/$APP_NAME"
DATA_DIR="$HOME/Library/Application Support/com.dbx.app"
KEY_FILE="$HOME/Library/Application Support/dbx/secret.key"
BACKUP_ROOT="$HOME/dbx-backups/install"
KEEP_BACKUPS=5
# Default features minus os-keyring: the app keeps its key in a local file and
# never uses the macOS Keychain.
FEATURES="duckdb-sidecar,dynamodb,mq-admin,sqlite-multiple-ciphers,system-fonts"

FULL=0 SKIP_BUILD=0 LAUNCH=1 FORCE=0
for arg in "$@"; do
  case "$arg" in
    --full) FULL=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --no-launch) LAUNCH=0 ;;
    --force) FORCE=1 ;;
    -h | --help) sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option: $arg (see --help)" >&2; exit 2 ;;
  esac
done

step() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
fail() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[[ "$(uname -s)" == "Darwin" ]] || fail "This script installs the macOS app only."

bundle_version() {
  /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$1/Contents/Info.plist" 2>/dev/null || echo "unknown"
}

# ---- 1. build -----------------------------------------------------------------
if [[ $SKIP_BUILD -eq 0 ]]; then
  command -v cargo >/dev/null || fail "cargo not found (install Rust: https://rustup.rs)."
  command -v pnpm >/dev/null || fail "pnpm not found (corepack enable, or npm i -g pnpm)."
  cd "$REPO_ROOT"
  if [[ ! -d node_modules ]]; then
    step "Installing JavaScript dependencies"
    pnpm install --frozen-lockfile
  fi

  free_gb=$(df -g "$REPO_ROOT" | awk 'NR==2 {print $4}')
  if [[ -n "$free_gb" && "$free_gb" -lt 25 ]]; then
    warn "Only ${free_gb} GB free; a release build needs roughly 20-30 GB. Consider 'cargo clean'."
  fi

  if [[ $FULL -eq 1 ]]; then
    step "Building DBX (release-grade, this takes a while)"
  else
    step "Building DBX (fast local build: thin LTO, 16 codegen units)"
    export CARGO_PROFILE_RELEASE_LTO=thin
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
  fi
  # Updater artifacts need the release signing key; a local install does not.
  pnpm tauri build --bundles app \
    --config '{"bundle":{"createUpdaterArtifacts":false}}' \
    -- --no-default-features --features "$FEATURES"
fi

[[ -d "$BUNDLE" ]] || fail "No built app at $BUNDLE. Run without --skip-build."
NEW_VERSION="$(bundle_version "$BUNDLE")"

# ---- 2. make sure the bundle is signed (ad-hoc is enough locally) ---------------
if ! codesign --verify --deep --strict "$BUNDLE" >/dev/null 2>&1; then
  step "Signing the app ad hoc"
  codesign --force --deep --sign - --entitlements "$REPO_ROOT/src-tauri/Entitlements.plist" "$BUNDLE"
fi

# ---- 3. back up data and settings (never modified, copied for safety) ----------
step "Backing up DBX data and settings"
stamp="$(date +%Y%m%d-%H%M%S)"
backup="$BACKUP_ROOT/$stamp"
mkdir -p "$backup"
chmod 700 "$BACKUP_ROOT" "$backup"
if [[ -d "$DATA_DIR" ]]; then
  # The database runs in WAL mode; use SQLite's online backup for a
  # consistent copy even if DBX is running.
  if [[ -f "$DATA_DIR/dbx.db" ]] && command -v sqlite3 >/dev/null; then
    sqlite3 "$DATA_DIR/dbx.db" ".backup '$backup/dbx.db'"
  elif [[ -f "$DATA_DIR/dbx.db" ]]; then
    cp -p "$DATA_DIR/dbx.db" "$backup/dbx.db"
  fi
  for extra in external-path-grants.json; do
    [[ -f "$DATA_DIR/$extra" ]] && cp -p "$DATA_DIR/$extra" "$backup/"
  done
fi
[[ -f "$KEY_FILE" ]] && cp -p "$KEY_FILE" "$backup/secret.key"
chmod -R go-rwx "$backup"
echo "Saved to $backup"
# Keep only the most recent backups.
ls -1d "$BACKUP_ROOT"/*/ 2>/dev/null | sort -r | tail -n +$((KEEP_BACKUPS + 1)) | while read -r old; do rm -rf "$old"; done

# ---- 4. quit the installed app -------------------------------------------------
if pgrep -f "$INSTALLED_APP/Contents/MacOS/" >/dev/null; then
  step "Quitting the running DBX"
  osascript -e 'tell application id "com.dbx.app" to quit' >/dev/null 2>&1 || true
  for _ in $(seq 1 30); do
    pgrep -f "$INSTALLED_APP/Contents/MacOS/" >/dev/null || break
    sleep 1
  done
  if pgrep -f "$INSTALLED_APP/Contents/MacOS/" >/dev/null; then
    [[ $FORCE -eq 1 ]] || fail "DBX is still running (unsaved work?). Close it, or rerun with --force."
    warn "Force-quitting DBX"
    pkill -f "$INSTALLED_APP/Contents/MacOS/" || true
    sleep 2
  fi
fi
if pgrep -f "$REPO_ROOT/target/debug/dbx" >/dev/null; then
  warn "A 'make dev' DBX is also running on the same data. Prefer running one at a time."
fi

# ---- 5. install (atomic swap, previous version kept until the new one is in place)
OLD_VERSION=""
[[ -d "$INSTALLED_APP" ]] && OLD_VERSION="$(bundle_version "$INSTALLED_APP")"
step "Installing DBX $NEW_VERSION into $INSTALL_DIR${OLD_VERSION:+ (replacing $OLD_VERSION)}"
staging="$INSTALL_DIR/.DBX.app.installing"
rm -rf "$staging"
ditto "$BUNDLE" "$staging"
if [[ -d "$INSTALLED_APP" ]]; then
  previous="$BACKUP_ROOT/$stamp/DBX-previous.app"
  mv "$INSTALLED_APP" "$previous"
fi
mv "$staging" "$INSTALLED_APP"
# Locally built: nothing to un-quarantine, but clear it in case the bundle was copied in.
xattr -dr com.apple.quarantine "$INSTALLED_APP" 2>/dev/null || true
echo "Installed $INSTALLED_APP"
[[ -n "$OLD_VERSION" ]] && echo "Previous app kept at $BACKUP_ROOT/$stamp/DBX-previous.app"

# ---- 6. launch ----------------------------------------------------------------------
if [[ $LAUNCH -eq 1 ]]; then
  step "Opening DBX"
  open "$INSTALLED_APP"
fi
echo
echo "Done. Data and settings are unchanged: $DATA_DIR"
