#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <version> <target> <binary-path>" >&2
  exit 1
fi

version="$1"
target="$2"
binary_path="$3"

dist_dir="dist"
archive_name="tmuy-v${version}-${target}.tar.gz"
archive_path="${dist_dir}/${archive_name}"
checksum_path="${archive_path}.sha256"

mkdir -p "${dist_dir}"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

cp "${binary_path}" "${tmp_dir}/tmuy"
chmod +x "${tmp_dir}/tmuy"
tar -C "${tmp_dir}" -czf "${archive_path}" tmuy

if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${archive_path}" > "${checksum_path}"
else
  shasum -a 256 "${archive_path}" > "${checksum_path}"
fi
