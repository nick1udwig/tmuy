#!/bin/sh
set -eu

PROJECT="tmuy"
REPO="${TMUY_INSTALL_REPO:-nick1udwig/tmuy}"
API_URL="${TMUY_INSTALL_API_URL:-https://api.github.com/repos/${REPO}/releases/latest}"
RELEASE_BASE_URL="${TMUY_INSTALL_RELEASE_BASE_URL:-https://github.com/${REPO}/releases/download}"
BIN_DIR="${TMUY_INSTALL_DIR:-${HOME}/.local/bin}"
VERSION="${TMUY_INSTALL_VERSION:-}"

usage() {
  cat <<EOF
Install ${PROJECT} from the latest GitHub Release.

Usage:
  sh install.sh [-b DIR] [-v TAG]

Options:
  -b, --bin-dir DIR   install into DIR (default: ${BIN_DIR})
  -v, --version TAG   install a specific release tag instead of latest
  -h, --help          show this help

Environment:
  TMUY_INSTALL_DIR           default install directory
  TMUY_INSTALL_VERSION       default release tag override
  TMUY_INSTALL_REPO          GitHub repo slug override
  TMUY_INSTALL_API_URL       latest release API URL override
  TMUY_INSTALL_RELEASE_BASE_URL
                            release download base URL override
EOF
}

say() {
  printf '%s\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

fetch() {
  url="$1"
  dest="$2"

  if command -v curl >/dev/null 2>&1; then
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" -o "${dest}" "${url}"
    else
      curl -fsSL -o "${dest}" "${url}"
    fi
    return
  fi

  if command -v wget >/dev/null 2>&1; then
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      wget -qO "${dest}" --header="Authorization: Bearer ${GITHUB_TOKEN}" "${url}"
    else
      wget -qO "${dest}" "${url}"
    fi
    return
  fi

  die "need curl or wget"
}

latest_tag() {
  meta_file="$1"
  fetch "${API_URL}" "${meta_file}"

  tag="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "${meta_file}" | head -n 1)"
  [ -n "${tag}" ] || die "failed to determine latest release tag from ${API_URL}"
  printf '%s\n' "${tag}"
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "${os}" in
    Linux)
      case "${arch}" in
        x86_64|amd64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
        aarch64|arm64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
        *) die "unsupported Linux architecture: ${arch}" ;;
      esac
      ;;
    Darwin)
      case "${arch}" in
        aarch64|arm64) printf '%s\n' "aarch64-apple-darwin" ;;
        *) die "unsupported macOS architecture: ${arch}" ;;
      esac
      ;;
    *)
      die "unsupported operating system: ${os}"
      ;;
  esac
}

compute_sha256() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${file}" | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${file}" | awk '{print $1}'
    return
  fi
  die "need sha256sum or shasum to verify release archive"
}

install_binary() {
  archive="$1"
  checksum_file="$2"
  target_bin="$3"
  temp_dir="$4"

  expected="$(awk '{print $1; exit}' "${checksum_file}")"
  [ -n "${expected}" ] || die "failed to read checksum from ${checksum_file}"

  actual="$(compute_sha256 "${archive}")"
  [ "${expected}" = "${actual}" ] || die "checksum mismatch for ${archive}"

  tar -xzf "${archive}" -C "${temp_dir}"
  [ -f "${temp_dir}/${PROJECT}" ] || die "release archive did not contain ${PROJECT}"

  mkdir -p "${BIN_DIR}"
  cp "${temp_dir}/${PROJECT}" "${target_bin}"
  chmod +x "${target_bin}"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -b|--bin-dir)
      [ "$#" -ge 2 ] || die "missing value for $1"
      BIN_DIR="$2"
      shift 2
      ;;
    -v|--version)
      [ "$#" -ge 2 ] || die "missing value for $1"
      VERSION="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

need_cmd uname
need_cmd tar
need_cmd mkdir
need_cmd cp
need_cmd chmod

tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT INT HUP TERM

target="$(detect_target)"
tag="${VERSION}"
if [ -z "${tag}" ]; then
  tag="$(latest_tag "${tmp_dir}/release.json")"
fi

archive_name="${PROJECT}-${tag}-${target}.tar.gz"
checksum_name="${archive_name}.sha256"
archive_url="${RELEASE_BASE_URL}/${tag}/${archive_name}"
checksum_url="${RELEASE_BASE_URL}/${tag}/${checksum_name}"
archive_path="${tmp_dir}/${archive_name}"
checksum_path="${tmp_dir}/${checksum_name}"
target_path="${BIN_DIR}/${PROJECT}"

say "installing ${PROJECT} ${tag} for ${target}"
fetch "${archive_url}" "${archive_path}"
fetch "${checksum_url}" "${checksum_path}"
install_binary "${archive_path}" "${checksum_path}" "${target_path}" "${tmp_dir}"

say "installed ${PROJECT} to ${target_path}"
case ":${PATH:-}:" in
  *:"${BIN_DIR}":*) ;;
  *)
    say "note: ${BIN_DIR} is not on PATH"
    ;;
esac
