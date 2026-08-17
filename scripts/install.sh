#!/bin/sh
# Install a prebuilt `lambo` binary from a GitHub Release.
#
#   curl -fsSL https://github.com/nrynss/lambo/releases/latest/download/install.sh | sh
#
# Downloads the right binary for this OS + architecture, verifies its SHA-256
# against the checksum published alongside the release, and installs it to a
# directory on PATH. Does not require curl + sh + sha256sum beyond the
# standard tools found on any macOS or Linux box.
#
# Overrides (all optional):
#   LAMBO_VERSION    the release version to install, e.g. "0.2.0" (default:
#                    the latest release). Use it to pin a specific version.
#   LAMBO_INSTALL_DIR  install directory (default ~/.local/bin). Must be
#                    writable by the current user.
#   LAMBO_REPO       owner/repo on GitHub (default nrynss/lambo).
#
# The binary is installed with mode 0755. Add $LAMBO_INSTALL_DIR to PATH if it
# is not already there, then run `lambo --version` to confirm.

set -eu

REPO="${LAMBO_REPO:-nrynss/lambo}"
VERSION="${LAMBO_VERSION:-}"
INSTALL_DIR="${LAMBO_INSTALL_DIR:-$HOME/.local/bin}"
BASE_URL="https://github.com/${REPO}/releases/download"

# --- Detect OS -------------------------------------------------------------
OS="$(uname -s)"
case "$OS" in
  Linux)  OS="linux" ;;
  Darwin) OS="macos" ;;
  *)
    echo "error: unsupported platform '$OS' (install.sh supports macOS and Linux; Windows users should grab the .exe from the release page)" >&2
    exit 1
    ;;
esac

# --- Detect architecture ----------------------------------------------------
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *)
    echo "error: unsupported architecture '$ARCH'" >&2
    exit 1
    ;;
esac

# No macos-x86_64 asset is published: the Intel macOS runner class never picked
# up the release job, so that target was dropped from the matrix rather than
# leaving every release blocked on it. Say so plainly here — otherwise this
# resolves to an asset URL that does not exist and the user sees a bare 404.
if [ "$OS" = "macos" ] && [ "$ARCH" = "x86_64" ]; then
  echo "error: no prebuilt binary is published for Intel macOS (macos-x86_64)." >&2
  echo "  Apple silicon is supported; Intel Macs need a build from source:" >&2
  echo "    cargo install --git https://github.com/${REPO} --features ship lambo" >&2
  exit 1
fi

echo "platform: ${OS}-${ARCH}"

# --- Resolve version --------------------------------------------------------
if [ -z "$VERSION" ]; then
  echo "LAMBO_VERSION unset, resolving the latest release..."
  VERSION="$(
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
      | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p' \
      | head -1
  )"
  if [ -z "$VERSION" ]; then
    echo "error: could not resolve the latest release version" >&2
    exit 1
  fi
  echo "latest release version: ${VERSION}"
else
  echo "installing pinned version: ${VERSION}"
fi

ASSET="lambo-${VERSION}-${OS}-${ARCH}"
URL="${BASE_URL}/v${VERSION}/${ASSET}"
SHA_URL="${URL}.sha256"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

BIN_PATH="${TMP_DIR}/${ASSET}"

echo "downloading ${URL}"
curl -fsSL "${URL}" -o "${BIN_PATH}"
echo "downloading checksum ${SHA_URL}"
curl -fsSL "${SHA_URL}" -o "${TMP_DIR}/${ASSET}.sha256"

# --- Verify SHA-256 ---------------------------------------------------------
# The published checksum file is "<hash>  <filename>", which is the shared
# output format of GNU `sha256sum` and BSD/macOS `shasum -a 256`.
EXPECTED="$(sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' "${TMP_DIR}/${ASSET}.sha256" | head -1)"
if [ -z "$EXPECTED" ]; then
  echo "error: checksum file for ${ASSET} is empty or malformed" >&2
  exit 1
fi
# macOS ships neither `sha256sum` nor GNU coreutils, so a hardcoded call to it
# fails on exactly the platform the macOS release assets exist for. `shasum` is
# in the base install there; Linux has `sha256sum`. Accept either.
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "${BIN_PATH}" | sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "${BIN_PATH}" | sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p')"
else
  echo "error: need sha256sum or shasum to verify the download; install either and retry" >&2
  echo "  refusing to install an unverified binary" >&2
  exit 1
fi
if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "error: checksum mismatch for ${ASSET}" >&2
  echo "  expected ${EXPECTED}" >&2
  echo "  actual   ${ACTUAL}" >&2
  exit 1
fi
echo "checksum verified"

# --- Install ----------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
chmod 0755 "${BIN_PATH}"
mv "${BIN_PATH}" "${INSTALL_DIR}/lambo"

echo "installed lambo ${VERSION} to ${INSTALL_DIR}/lambo"
echo
echo "Verify it is on PATH and check the version:"
echo "  ${INSTALL_DIR}/lambo --version"
if ! command -v lambo >/dev/null 2>&1; then
  echo "Note: ${INSTALL_DIR} is not on PATH. Add it, for example:"
  echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile"
fi
