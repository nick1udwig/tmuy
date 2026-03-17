#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

cat > "${tmp_dir}/Cargo.toml" <<'EOF'
[package]
name = "demo"
version = "1.2.3"
edition = "2024"
EOF

next_version="$("${repo_root}/scripts/bump-version.sh" "${tmp_dir}/Cargo.toml")"
[[ "${next_version}" == "1.2.4" ]]
grep -q '^version = "1.2.4"$' "${tmp_dir}/Cargo.toml"
! grep -q '^version = "1.2.3"$' "${tmp_dir}/Cargo.toml"

printf 'bump-version smoke test passed\n'
