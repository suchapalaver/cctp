#!/usr/bin/env bash
set -euo pipefail

package_files="$(mktemp)"
trap 'rm -f "$package_files"' EXIT

cargo package --locked --list "$@" | tee "$package_files"

is_allowed_package_path() {
  case "$1" in
    .cargo_vcs_info.json | .env.example | Cargo.lock | Cargo.toml | Cargo.toml.orig | README.md | src/*.rs)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_forbidden_artifact() {
  case "$1" in
    .env.example)
      return 1
      ;;
    .direnv/* | target/* | .env | .env.* | cctp.toml | cctp.local.toml | *.local.toml)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

forbidden=()
unexpected=()
while IFS= read -r path; do
  if is_forbidden_artifact "$path"; then
    forbidden+=("$path")
  elif ! is_allowed_package_path "$path"; then
    unexpected+=("$path")
  fi
done <"$package_files"

if ((${#forbidden[@]})); then
  printf 'error: crate package file list contains local or generated artifacts:\n' >&2
  printf '  %s\n' "${forbidden[@]}" >&2
  exit 1
fi

if ((${#unexpected[@]})); then
  printf 'error: crate package file list contains unexpected files:\n' >&2
  printf '  %s\n' "${unexpected[@]}" >&2
  exit 1
fi
