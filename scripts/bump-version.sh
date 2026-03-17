#!/usr/bin/env bash
set -euo pipefail

version_file="${1:-Cargo.toml}"
lock_file="${2:-$(dirname "${version_file}")/Cargo.lock}"
package_name="$(sed -n 's/^name = "\(.*\)"/\1/p' "${version_file}" | head -n 1)"

current_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${version_file}" | head -n 1)"
if [[ -z "${current_version}" ]]; then
  echo "failed to find version in ${version_file}" >&2
  exit 1
fi
if [[ -z "${package_name}" ]]; then
  echo "failed to find package name in ${version_file}" >&2
  exit 1
fi

IFS='.' read -r major minor patch <<<"${current_version}"
if [[ -z "${major}" || -z "${minor}" || -z "${patch}" ]]; then
  echo "unsupported version format: ${current_version}" >&2
  exit 1
fi

next_version="${major}.${minor}.$((patch + 1))"

python3 - "${version_file}" "${lock_file}" "${package_name}" "${current_version}" "${next_version}" <<'PY'
from pathlib import Path
import re
import sys

version_path = Path(sys.argv[1])
lock_path = Path(sys.argv[2])
package_name = sys.argv[3]
current = sys.argv[4]
next_version = sys.argv[5]

version_text = version_path.read_text()
version_needle = f'version = "{current}"'
if version_needle not in version_text:
    raise SystemExit(f"version marker {version_needle!r} not found in {version_path}")
version_path.write_text(version_text.replace(version_needle, f'version = "{next_version}"', 1))

if lock_path.exists():
    lock_text = lock_path.read_text()
    pattern = re.compile(
        r'(\[\[package\]\]\nname = "'
        + re.escape(package_name)
        + r'"\nversion = ")[^"]+(")',
        re.MULTILINE,
    )
    updated_text, count = pattern.subn(rf'\g<1>{next_version}\2', lock_text, count=1)
    if count == 0:
        raise SystemExit(
            f'lockfile package marker for {package_name!r} not found in {lock_path}'
        )
    lock_path.write_text(updated_text)
PY

printf '%s\n' "${next_version}"
