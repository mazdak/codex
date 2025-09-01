#!/usr/bin/env bash
set -euo pipefail

repo_full="${GITHUB_REPOSITORY:-}"
if [[ -n "${repo_full}" && "${repo_full}" == */* ]]; then
  default_owner="${repo_full%%/*}"
  default_repo="${repo_full##*/}"
else
  default_owner="mazdak"
  default_repo="codex"
fi

OWNER="${GITHUB_OWNER:-${default_owner}}"
REPO="${GITHUB_REPO:-${default_repo}}"
TAP_REPO="${TAP_REPO:-../homebrew-tap}"
FORMULA_PATH="${FORMULA_PATH:-$TAP_REPO/Formula/codex.rb}"
RELEASE_TAG="${CODEX_RELEASE_TAG:-${GITHUB_REF_NAME:-}}"

if [[ -z "${RELEASE_TAG}" ]]; then
  echo "error: CODEX_RELEASE_TAG is required (e.g. rust-v0.98.0)" >&2
  exit 1
fi

if [[ ! "${RELEASE_TAG}" =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+(-(alpha|beta)(\.[0-9]+)?)?$ ]]; then
  echo "error: release tag '${RELEASE_TAG}' doesn't match expected format" >&2
  exit 1
fi

VERSION="${RELEASE_TAG#rust-v}"

if [[ ! -d "${TAP_REPO}" ]]; then
  echo "error: homebrew-tap repository not found at ${TAP_REPO}" >&2
  exit 1
fi

mkdir -p "$(dirname "${FORMULA_PATH}")"

sha_for_target() {
  local target="$1"
  local asset="codex-${target}.tar.gz"
  local url="https://github.com/${OWNER}/${REPO}/releases/download/${RELEASE_TAG}/${asset}"
  local tmp_file
  local token
  local curl_args

  token=""
  if [[ -n "${GH_TOKEN:-}" ]]; then
    token="${GH_TOKEN}"
  elif [[ -n "${GITHUB_TOKEN:-}" ]]; then
    token="${GITHUB_TOKEN}"
  fi
  curl_args=(-fsSL)
  if [[ -n "${token}" ]]; then
    curl_args+=(-H "Authorization: Bearer ${token}")
  fi

  tmp_file="$(mktemp)"
  if ! curl "${curl_args[@]}" "${url}" -o "${tmp_file}"; then
    rm -f "${tmp_file}"
    return 1
  fi
  if [[ ! -s "${tmp_file}" ]]; then
    rm -f "${tmp_file}"
    return 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${tmp_file}" | awk '{print $1}'
  else
    shasum -a 256 "${tmp_file}" | awk '{print $1}'
  fi
  rm -f "${tmp_file}"
}

if ! SHA_MAC_ARM="$(sha_for_target "aarch64-apple-darwin")"; then
  echo "error: failed to download macOS arm64 asset for ${RELEASE_TAG}" >&2
  exit 1
fi
if ! SHA_MAC_INTEL="$(sha_for_target "x86_64-apple-darwin")"; then
  echo "error: failed to download macOS x86_64 asset for ${RELEASE_TAG}" >&2
  exit 1
fi
if ! SHA_LINUX_ARM="$(sha_for_target "aarch64-unknown-linux-musl")"; then
  echo "error: failed to download Linux arm64 asset for ${RELEASE_TAG}" >&2
  exit 1
fi
if ! SHA_LINUX_INTEL="$(sha_for_target "x86_64-unknown-linux-musl")"; then
  echo "error: failed to download Linux x86_64 asset for ${RELEASE_TAG}" >&2
  exit 1
fi

cat <<FORMULA > "${FORMULA_PATH}"
class Codex < Formula
  desc "Codex CLI"
  homepage "https://github.com/${OWNER}/${REPO}"
  version "${VERSION}"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/${OWNER}/${REPO}/releases/download/${RELEASE_TAG}/codex-aarch64-apple-darwin.tar.gz"
      sha256 "${SHA_MAC_ARM}"
    end

    on_intel do
      url "https://github.com/${OWNER}/${REPO}/releases/download/${RELEASE_TAG}/codex-x86_64-apple-darwin.tar.gz"
      sha256 "${SHA_MAC_INTEL}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/${OWNER}/${REPO}/releases/download/${RELEASE_TAG}/codex-aarch64-unknown-linux-musl.tar.gz"
      sha256 "${SHA_LINUX_ARM}"
    end

    on_intel do
      url "https://github.com/${OWNER}/${REPO}/releases/download/${RELEASE_TAG}/codex-x86_64-unknown-linux-musl.tar.gz"
      sha256 "${SHA_LINUX_INTEL}"
    end
  end

  def install
    target = if OS.mac?
      Hardware::CPU.arm? ? "aarch64-apple-darwin" : "x86_64-apple-darwin"
    else
      Hardware::CPU.arm? ? "aarch64-unknown-linux-musl" : "x86_64-unknown-linux-musl"
    end
    bin.install "codex-#{target}" => "codex"
  end

  test do
    system "#{bin}/codex", "--version"
  end
end
FORMULA

echo "Updated Homebrew formula at ${FORMULA_PATH}"
