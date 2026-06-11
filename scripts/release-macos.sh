#!/usr/bin/env bash
# Build, relocatable-link, Developer ID-sign, notarize, and staple a
# macOS/arm64 distribution of tb-sandbox + the Cargo-built libkrun runner.
#
# Produces a self-contained layout:
#
#   tb-sandbox-<version>-macos-<arch>/
#     bin/tb-sandbox         (Developer ID, hardened runtime)
#     bin/tb-sandbox-runner  (Developer ID, hardened runtime, hypervisor entitlement)
#
# Both live in bin/: tb-sandbox finds tb-sandbox-runner next to itself
# (src/libkrun.rs `bundled_dirs`), so no DYLD_* env var is used.
#
# Outputs (under ./dist):
#   tb-sandbox-<version>-macos-<arch>.dmg      signed + notarized + STAPLED
#   tb-sandbox-<version>-macos-<arch>.tar.gz   same signed binaries (for a Homebrew tap)
#   *.sha256
#
# The guest image bundle (kernel + root disk) is DATA, not Mach-O: it is not
# signed and is distributed separately. See scripts/build-base-bundle.sh +
# build-agent-image.sh.
#
# --- Notarization (matches ../app-quorum/bundle-app.sh) ---------------------
# Credentials come from EITHER:
#   (a) APPLE_ID + APPLE_APP_PASSWORD env vars  (preferred; APPLE_TEAM_ID is
#       optional and auto-detected from the signing identity), or
#   (b) a notarytool keychain profile (NOTARY_PROFILE, default "tb-notary").
# Generate the app-specific password at https://account.apple.com -> App-Specific
# Passwords.
#
# Usage:
#   APPLE_ID="you@example.com" APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx" \
#     scripts/release-macos.sh
#
#   # or with an explicit identity / keychain profile:
#   SIGN_IDENTITY="Developer ID Application: Your Org (TEAMID)" \
#   NOTARY_PROFILE="tb-notary" scripts/release-macos.sh
#
# Env vars:
#   SIGN_IDENTITY        Developer ID Application identity. Default: auto-detect
#                        from the keychain. (CODESIGN_IDENTITY also accepted.)
#   APPLE_ID             Apple ID for notarization (with APPLE_APP_PASSWORD)
#   APPLE_APP_PASSWORD   app-specific password for APPLE_ID
#   APPLE_TEAM_ID        team id; auto-parsed from SIGN_IDENTITY "(TEAMID)" if unset
#   NOTARY_PROFILE       notarytool keychain profile (default: tb-notary), used
#                        only when APPLE_ID/APPLE_APP_PASSWORD are not set
#   VERSION              version string (default: Cargo.toml package version)
#   LIBKRUN_DIR          libkrun fork checkout (default: ../thousandbirds-libkrun)
#   OUT_DIR              output dir (default: dist)
#   SKIP_SIGN=1          build + relocatable-link only (no codesign/notarize)
#   SKIP_NOTARIZE=1      sign but do not notarize/staple
#   NO_DMG=1             skip the .dmg (tar.gz only; notarizes the tar.gz's zip,
#                        online ticket only -- cannot be stapled)
#   LLVM_PREFIX          (default: $(brew --prefix llvm))   } only used when
#   LLD_PREFIX           (default: $(brew --prefix lld))    } building libkrun
set -euo pipefail

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
LIBKRUN_DIR=${LIBKRUN_DIR:-"$ROOT_DIR/../thousandbirds-libkrun"}
OUT_DIR=${OUT_DIR:-"$ROOT_DIR/dist"}
ARCH=$(uname -m)
ENTITLEMENTS="$ROOT_DIR/packaging/os_mode.entitlements"
SIGN_IDENTITY=${SIGN_IDENTITY:-${CODESIGN_IDENTITY:-}}
APPLE_ID=${APPLE_ID:-}
APPLE_APP_PASSWORD=${APPLE_APP_PASSWORD:-}
APPLE_TEAM_ID=${APPLE_TEAM_ID:-}
NOTARY_PROFILE=${NOTARY_PROFILE:-tb-notary}

note() { printf '\033[1;34m>>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "must run on macOS"
[ -d "$LIBKRUN_DIR" ] || die "libkrun checkout not found at $LIBKRUN_DIR (set LIBKRUN_DIR)"
[ -f "$ENTITLEMENTS" ] || die "missing entitlements file: $ENTITLEMENTS"

VERSION=${VERSION:-$(awk -F'"' '/^version =/{print $2; exit}' "$ROOT_DIR/Cargo.toml")}
[ -n "$VERSION" ] || die "could not determine VERSION"

for tool in cargo codesign ditto xcrun; do
  command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done

# ---- Resolve signing identity (auto-detect like app-quorum) ----------------
if [ -z "${SKIP_SIGN:-}" ]; then
  if [ -z "$SIGN_IDENTITY" ]; then
    SIGN_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
      | awk -F'"' '/Developer ID Application/ {print $2; exit}')
    [ -n "$SIGN_IDENTITY" ] || die "no 'Developer ID Application' identity in the keychain; set SIGN_IDENTITY"
    note "auto-detected signing identity: $SIGN_IDENTITY"
  fi
  security find-identity -p codesigning -v 2>/dev/null | grep -qF "$SIGN_IDENTITY" \
    || die "signing identity not found in keychain: $SIGN_IDENTITY"
  # Team id: explicit, else parse the trailing (TEAMID) from the identity.
  if [ -z "$APPLE_TEAM_ID" ]; then
    APPLE_TEAM_ID=$(printf '%s' "$SIGN_IDENTITY" | sed -nE 's/.*\(([A-Z0-9]+)\)$/\1/p')
  fi
fi

# ---- Resolve notary auth mode ----------------------------------------------
NOTARY_AUTH=""
if [ -z "${SKIP_SIGN:-}" ] && [ -z "${SKIP_NOTARIZE:-}" ]; then
  if [ -n "$APPLE_ID" ] && [ -n "$APPLE_APP_PASSWORD" ]; then
    [ -n "$APPLE_TEAM_ID" ] || die "APPLE_TEAM_ID not set and not parseable from identity '$SIGN_IDENTITY'; set APPLE_TEAM_ID"
    NOTARY_AUTH="env"
    note "notary auth: APPLE_ID=$APPLE_ID team=$APPLE_TEAM_ID (app-specific password from env)"
  elif [ -n "$APPLE_ID" ] || [ -n "$APPLE_APP_PASSWORD" ]; then
    die "APPLE_ID and APPLE_APP_PASSWORD must be set together"
  elif security find-generic-password -s "com.apple.gke.notary.tool" -a "$NOTARY_PROFILE" >/dev/null 2>&1; then
    NOTARY_AUTH="profile"
    note "notary auth: keychain profile '$NOTARY_PROFILE'"
  else
    cat >&2 <<EOF
xx  No notary credentials. Either set:
      APPLE_ID="you@example.com" APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx" $0
    (APPLE_TEAM_ID optional; auto-detected from the signing identity)

    Or create a keychain profile named '$NOTARY_PROFILE' once:
      xcrun notarytool store-credentials $NOTARY_PROFILE \\
        --apple-id <you@example.com> --team-id <TEAMID> \\
        --password <app-specific-password>

    Generate the app-specific password at https://account.apple.com -> App-Specific Passwords.
    Or pass SKIP_NOTARIZE=1 to sign without notarizing.
EOF
    exit 1
  fi
fi

# notarize_and_staple <submit-path> <staple-path>
notarize_and_staple() {
  local submit_path="$1" staple_path="$2"
  local log="$OUT_DIR/.notary.log"
  local args=(submit "$submit_path" --wait)
  if [ "$NOTARY_AUTH" = "env" ]; then
    args+=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD")
  else
    args+=(--keychain-profile "$NOTARY_PROFILE")
  fi
  note "submitting to notary service (this can take a few minutes)"
  xcrun notarytool "${args[@]}" 2>&1 | tee "$log" || die "notarytool submit failed (see $log)"
  if ! grep -q "status: Accepted" "$log"; then
    local id
    id=$(awk -F': ' '/^  id:/ {print $2; exit}' "$log")
    [ -n "$id" ] && xcrun notarytool log "$id" \
      $( [ "$NOTARY_AUTH" = env ] && printf -- '--apple-id %s --team-id %s --password %s' "$APPLE_ID" "$APPLE_TEAM_ID" "$APPLE_APP_PASSWORD" || printf -- '--keychain-profile %s' "$NOTARY_PROFILE" ) || true
    die "notarization not Accepted (see $log)"
  fi
  note "notarization accepted; stapling $staple_path"
  xcrun stapler staple "$staple_path"
  xcrun stapler validate "$staple_path"
}

NAME="tb-sandbox-${VERSION}-macos-${ARCH}"
STAGE="$OUT_DIR/$NAME"
BIN="$STAGE/bin"

rm -rf "$STAGE"
mkdir -p "$BIN"

# ---- 1. Prepare libkrun Linux sysroot for Cargo builds ---------------------
if [ ! -f "$LIBKRUN_DIR/linux-sysroot/.sysroot_ready" ]; then
  note "preparing libkrun Linux sysroot"
  LLVM_PREFIX=${LLVM_PREFIX:-$(brew --prefix llvm 2>/dev/null || echo /opt/homebrew/opt/llvm)}
  LLD_PREFIX=${LLD_PREFIX:-$(brew --prefix lld 2>/dev/null || echo /opt/homebrew/opt/lld)}
  ( cd "$LIBKRUN_DIR" && \
    LIBCLANG_PATH="$LLVM_PREFIX/lib" \
    PATH="$LLD_PREFIX/bin:$LLVM_PREFIX/bin:$PATH" \
    make linux-sysroot/.sysroot_ready CLANG="$LLVM_PREFIX/bin/clang" )
fi

# ---- 2. Build tb-sandbox + runner -----------------------------------------
note "building tb-sandbox and tb-sandbox-runner (release)"
( cd "$ROOT_DIR" && cargo build --release --bins )
TB="$ROOT_DIR/target/release/tb-sandbox"
RUNNER="$ROOT_DIR/target/release/tb-sandbox-runner"
[ -f "$TB" ] || die "tb-sandbox build failed"
[ -f "$RUNNER" ] || die "tb-sandbox-runner build failed"

# ---- 3. Stage flat layout --------------------------------------------------
note "staging $NAME"
cp "$TB" "$BIN/tb-sandbox"
cp "$RUNNER" "$BIN/tb-sandbox-runner"
chmod u+w "$BIN/tb-sandbox-runner" "$BIN/tb-sandbox"

# ---- 4. Codesign (Developer ID, hardened runtime) --------------------------
if [ -z "${SKIP_SIGN:-}" ]; then
  note "codesigning with: $SIGN_IDENTITY"
  codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" \
    -s "$SIGN_IDENTITY" "$BIN/tb-sandbox-runner"
  codesign --force --options runtime --timestamp \
    -s "$SIGN_IDENTITY" "$BIN/tb-sandbox"
  for m in tb-sandbox-runner tb-sandbox; do
    codesign --verify --strict --verbose=2 "$BIN/$m" || die "codesign verify failed for $m"
  done
  codesign -d --entitlements - "$BIN/tb-sandbox-runner" 2>/dev/null | grep -q "com.apple.security.hypervisor" \
    || die "tb-sandbox-runner missing hypervisor entitlement"
else
  note "SKIP_SIGN set -- leaving binaries unsigned"
fi

# ---- 5. Tarball (Homebrew tap) ---------------------------------------------
TARBALL="$OUT_DIR/${NAME}.tar.gz"
note "packaging $TARBALL"
( cd "$OUT_DIR" && tar -czf "$TARBALL" "$NAME" )

# ---- 6. DMG (signed, notarized, stapled -- offline-verifiable) -------------
DMG=""
if [ "${NO_DMG:-0}" != "1" ]; then
  DMG="$OUT_DIR/${NAME}.dmg"
  note "creating $DMG"
  rm -f "$DMG"
  hdiutil create -volname "$NAME" -srcfolder "$STAGE" -ov -format UDZO -fs HFS+ "$DMG" >/dev/null
  if [ -z "${SKIP_SIGN:-}" ]; then
    codesign --force --timestamp -s "$SIGN_IDENTITY" "$DMG"
  fi
fi

# ---- 7. Notarize + staple --------------------------------------------------
if [ -z "${SKIP_SIGN:-}" ] && [ -z "${SKIP_NOTARIZE:-}" ]; then
  if [ -n "$DMG" ]; then
    # Notarize+staple the dmg (offline-verifiable). The same signed binaries in
    # the tarball share the registered notarization ticket (online-verified).
    notarize_and_staple "$DMG" "$DMG"
  else
    # No dmg: notarize a zip of the binaries (ticket is online-only; loose
    # binaries / tarballs cannot be stapled).
    ZIP="$OUT_DIR/.${NAME}-notarize.zip"
    ( cd "$OUT_DIR" && ditto -c -k --sequesterRsrc --keepParent "$NAME" "$ZIP" )
    note "submitting zip to notary service (no dmg -> online ticket only, no staple)"
    if [ "$NOTARY_AUTH" = "env" ]; then
      xcrun notarytool submit "$ZIP" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
        --password "$APPLE_APP_PASSWORD" --wait || die "notarization failed"
    else
      xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait || die "notarization failed"
    fi
    rm -f "$ZIP"
  fi
else
  note "skipping notarization"
fi

# ---- 10. Checksums ---------------------------------------------------------
( cd "$OUT_DIR" && shasum -a 256 "$(basename "$TARBALL")" > "${TARBALL}.sha256" )
[ -n "$DMG" ] && ( cd "$OUT_DIR" && shasum -a 256 "$(basename "$DMG")" > "${DMG}.sha256" )

echo
if [ -n "${SKIP_SIGN:-}" ]; then
  DMG_LABEL="(UNSIGNED -- dry run)"; TAR_LABEL="(UNSIGNED -- dry run)"
elif [ -n "${SKIP_NOTARIZE:-}" ]; then
  DMG_LABEL="(signed, NOT notarized)"; TAR_LABEL="(signed, NOT notarized)"
else
  DMG_LABEL="(signed, notarized, stapled)"; TAR_LABEL="(signed, notarized; for a Homebrew tap)"
fi
note "done:"
[ -n "$DMG" ] && echo "    $DMG   $DMG_LABEL"
echo "    $TARBALL   $TAR_LABEL"
echo
echo "Reminder: ship a guest image bundle separately (scripts/build-base-bundle.sh +"
echo "build-agent-image.sh). tb-sandbox finds tb-sandbox-runner next to its own"
echo "binary -- no env vars required."
