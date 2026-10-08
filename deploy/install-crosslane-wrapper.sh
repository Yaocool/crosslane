#!/bin/sh
set -eu

target="/usr/local/bin/crosslane"
force=0
check_only=0

usage() {
  cat <<'EOF'
Usage: install-crosslane-wrapper.sh [--check] [--force] [--target PATH]

Installs the Docker-backed crosslane wrapper. Run with sudo when the target directory
requires administrator privileges. --check validates the target without writing it.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --force)
      force=1
      shift
      ;;
    --check)
      check_only=1
      shift
      ;;
    --target)
      [ "$#" -ge 2 ] || {
        echo "install-crosslane-wrapper.sh: --target requires a path" >&2
        exit 2
      }
      target="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "install-crosslane-wrapper.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

[ -n "$target" ] || {
  echo "install-crosslane-wrapper.sh: --target must not be empty" >&2
  exit 2
}
[ ! -d "$target" ] || {
  echo "install-crosslane-wrapper.sh: target must be a file path, not a directory: $target" >&2
  exit 1
}

script_dir="$(CDPATH= cd "$(dirname "$0")" && pwd)"
source_file="$script_dir/crosslane-docker"

[ -f "$source_file" ] || {
  echo "install-crosslane-wrapper.sh: wrapper not found: $source_file" >&2
  exit 1
}

if [ -e "$target" ]; then
  if cmp -s "$source_file" "$target"; then
    echo "Docker-backed crosslane wrapper is already installed at $target"
    exit 0
  fi

  managed=0
  # Keep custom install paths upgradeable across the project rename.
  if grep -qF -e "Managed by crosslane's install-crosslane-wrapper.sh." \
    -e "Managed by kiro-proxy's install-kproxy-wrapper.sh." "$target"; then
    managed=1
  else
    # Recognize the exact wrapper shipped before the managed marker was added.
    legacy_checksum="$(cksum "$target" 2>/dev/null || true)"
    case "$legacy_checksum" in
      "3654078470 1244 "*) managed=1 ;;
    esac
  fi

  if [ "$force" -ne 1 ] && [ "$managed" -ne 1 ]; then
    echo "install-crosslane-wrapper.sh: target already exists and is not managed by this project: $target" >&2
    echo "rerun with --force to replace it, or use --target for another path" >&2
    exit 1
  fi
fi

if [ "$check_only" -eq 1 ]; then
  exit 0
fi

target_dir="$(dirname "$target")"
target_name="$(basename "$target")"
temp_target=""
cleanup_temp() {
  [ -z "$temp_target" ] || rm -f -- "$temp_target"
}
trap cleanup_temp EXIT
trap 'exit 1' HUP INT TERM

temp_target="$(mktemp "$target_dir/.${target_name}.tmp.XXXXXX")"
install -m 0755 "$source_file" "$temp_target"
mv -f "$temp_target" "$target"
temp_target=""
trap - EXIT HUP INT TERM
echo "installed Docker-backed crosslane wrapper at $target"
echo "run 'crosslane health' after the Compose service is running"
