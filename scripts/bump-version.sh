#!/usr/bin/env bash
set -euo pipefail

version_file="${1:-Cargo.toml}"

current_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${version_file}" | head -n 1)"
if [[ -z "${current_version}" ]]; then
  echo "failed to find version in ${version_file}" >&2
  exit 1
fi

IFS='.' read -r major minor patch <<<"${current_version}"
if [[ -z "${major}" || -z "${minor}" || -z "${patch}" ]]; then
  echo "unsupported version format: ${current_version}" >&2
  exit 1
fi

next_version="${major}.${minor}.$((patch + 1))"

python3 - "${version_file}" "${current_version}" "${next_version}" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
current = sys.argv[2]
next_version = sys.argv[3]
text = path.read_text()
needle = f'version = "{current}"'
if needle not in text:
    raise SystemExit(f"version marker {needle!r} not found in {path}")
path.write_text(text.replace(needle, f'version = "{next_version}"', 1))
PY

printf '%s\n' "${next_version}"
