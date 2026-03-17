#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

cat > "${tmp_dir}/Cargo.toml" <<'EOF'
[package]
name = "tmuy"
version = "1.2.3"
edition = "2024"
EOF

cat > "${tmp_dir}/Cargo.lock" <<'EOF'
version = 4

[[package]]
name = "tmuy"
version = "1.2.3"
EOF

next_version="$("${repo_root}/scripts/bump-version.sh" "${tmp_dir}/Cargo.toml" "${tmp_dir}/Cargo.lock")"
[[ "${next_version}" == "1.2.4" ]]
grep -q '^version = "1.2.4"$' "${tmp_dir}/Cargo.toml"
! grep -q '^version = "1.2.3"$' "${tmp_dir}/Cargo.toml"
grep -q '^version = "1.2.4"$' "${tmp_dir}/Cargo.lock"
! grep -q '^version = "1.2.3"$' "${tmp_dir}/Cargo.lock"

printf 'bump-version smoke test passed\n'
