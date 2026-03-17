#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

version="v9.9.9"
target="x86_64-unknown-linux-gnu"
release_dir="${tmp_dir}/release"
mock_bin_dir="${tmp_dir}/mock-bin"
install_bin_dir="${tmp_dir}/install-bin"

mkdir -p "${release_dir}" "${mock_bin_dir}" "${install_bin_dir}" "${tmp_dir}/archive"

cat > "${tmp_dir}/archive/tmuy" <<'EOF'
#!/bin/sh
printf 'tmuy fake\n'
EOF
chmod +x "${tmp_dir}/archive/tmuy"

archive_name="tmuy-${version}-${target}.tar.gz"
archive_path="${release_dir}/${archive_name}"
tar -C "${tmp_dir}/archive" -czf "${archive_path}" tmuy
sha256sum "${archive_path}" > "${release_dir}/${archive_name}.sha256"

cat > "${mock_bin_dir}/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

out=""
url=""
while (($#)); do
  case "$1" in
    -o)
      out="$2"
      shift 2
      ;;
    -H|-A|--header)
      shift 2
      ;;
    -fsSL|-fLsS|-fSL|-sSL|-fsS|-sS|-fL|-fsL|-sL|-f|-s|-S|-L)
      shift
      ;;
    --*)
      shift
      ;;
    -*)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done

case "${url}" in
  "https://api.github.com/repos/nick1udwig/tmuy/releases/latest")
    if [[ "${MOCK_CURL_DISABLE_API:-0}" == "1" ]]; then
      echo "unexpected latest-release API call" >&2
      exit 1
    fi
    content='{"tag_name":"v9.9.9"}'
    if [[ -n "${out}" ]]; then
      printf '%s\n' "${content}" > "${out}"
    else
      printf '%s\n' "${content}"
    fi
    ;;
  "https://github.com/nick1udwig/tmuy/releases/download/v9.9.9/tmuy-v9.9.9-x86_64-unknown-linux-gnu.tar.gz")
    cp "${MOCK_RELEASE_DIR}/tmuy-v9.9.9-x86_64-unknown-linux-gnu.tar.gz" "${out}"
    ;;
  "https://github.com/nick1udwig/tmuy/releases/download/v9.9.9/tmuy-v9.9.9-x86_64-unknown-linux-gnu.tar.gz.sha256")
    cp "${MOCK_RELEASE_DIR}/tmuy-v9.9.9-x86_64-unknown-linux-gnu.tar.gz.sha256" "${out}"
    ;;
  *)
    echo "unexpected curl url: ${url}" >&2
    exit 1
    ;;
esac
EOF
chmod +x "${mock_bin_dir}/curl"

PATH="${mock_bin_dir}:/usr/bin:/bin" \
  MOCK_RELEASE_DIR="${release_dir}" \
  HOME="${tmp_dir}/home" \
  sh "${repo_root}/install.sh" -b "${install_bin_dir}"

test -x "${install_bin_dir}/tmuy"
[[ "$("${install_bin_dir}/tmuy")" == "tmuy fake" ]]

PATH="${mock_bin_dir}:/usr/bin:/bin" \
  MOCK_RELEASE_DIR="${release_dir}" \
  MOCK_CURL_DISABLE_API=1 \
  HOME="${tmp_dir}/home" \
  sh "${repo_root}/install.sh" -b "${tmp_dir}/install-bin-2" -v "${version}"

test -x "${tmp_dir}/install-bin-2/tmuy"
[[ "$("${tmp_dir}/install-bin-2/tmuy")" == "tmuy fake" ]]

printf 'install script smoke tests passed\n'
