#!/usr/bin/env bash
set -euo pipefail

# Ubuntu 24.04 runners already include Clang. Fetch the pinned official
# linker archive without a dependency on the Ubuntu package mirrors.
: "${RUNNER_TEMP:?RUNNER_TEMP must be set}"
: "${GITHUB_PATH:?GITHUB_PATH must be set}"
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
    echo 'This installer requires a Linux x86_64 runner.' >&2
    exit 1
fi
command -v clang
clang --version

mold_version=2.42.1
mold_sha256=6ff270c9bf07d2bec5c98aa324eb7c4daf6a1a4d815c05ff1708049616047855
mold_archive="mold-${mold_version}-x86_64-linux.tar.gz"
mold_install_dir="$(mktemp -d "${RUNNER_TEMP}/ryokan-mold.XXXXXX")"
mold_archive_path="${mold_install_dir}/${mold_archive}"
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    --retry 3 --retry-delay 2 --retry-connrefused \
    --connect-timeout 15 --max-time 120 \
    --output "$mold_archive_path" \
    "https://github.com/rui314/mold/releases/download/v${mold_version}/${mold_archive}"
printf '%s  %s\n' "$mold_sha256" "$mold_archive_path" | sha256sum --check --strict
tar -xzf "$mold_archive_path" -C "$mold_install_dir"

mold_bin_dir="${mold_install_dir}/mold-${mold_version}-x86_64-linux/bin"
test -x "${mold_bin_dir}/mold"
test -x "${mold_bin_dir}/ld.mold"
export PATH="${mold_bin_dir}:${PATH}"
mold --version

# Prove the repository's Clang + mold configuration can link and run.
printf 'int main(void) { return 0; }\n' > "${mold_install_dir}/link-check.c"
clang -fuse-ld=mold "${mold_install_dir}/link-check.c" -o "${mold_install_dir}/link-check"
"${mold_install_dir}/link-check"
printf '%s\n' "$mold_bin_dir" >> "$GITHUB_PATH"
