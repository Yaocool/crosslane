#!/bin/sh
set -eu

script_name="$(basename "$0")"
script_dir="$(CDPATH= cd "$(dirname "$0")" && pwd)"
setup_script="$script_dir/docker-setup.sh"
upgrade_image="${CROSSLANE_UPGRADE_IMAGE:-ghcr.io/yaocool/crosslane:latest}"

usage() {
  cat <<EOF
Usage: $script_name [DOCKER-SETUP-OPTIONS]

Pull and deploy the latest published stable crosslane image. The existing
container keeps running during the pull, and docker-setup.sh restores the
previous image if startup or the health check fails.

Environment:
  CROSSLANE_UPGRADE_IMAGE  Image/channel to follow
                        (default: ghcr.io/yaocool/crosslane:latest)

Examples:
  ./deploy/$script_name
  ./deploy/$script_name --timeout 120
  CROSSLANE_UPGRADE_IMAGE=registry.example.com/crosslane:stable ./deploy/$script_name

Safe docker-setup options such as --target, --project-name, --timeout, --force,
and --repair-volume are forwarded. Build, no-pull, and a second --image option
are rejected so this command always performs a registry upgrade.
EOF
}

fail() {
  echo "$script_name: $*" >&2
  exit 1
}

[ -x "$setup_script" ] || fail "setup script is not executable: $setup_script"
[ -n "$upgrade_image" ] || fail "CROSSLANE_UPGRADE_IMAGE must not be empty"

for argument in "$@"; do
  case "$argument" in
    -h|--help)
      usage
      exit 0
      ;;
    --build|--no-build|--no-pull|--image|--image=*)
      fail "$argument is not allowed; set CROSSLANE_UPGRADE_IMAGE to select another registry image"
      ;;
  esac
done

exec "$setup_script" --image "$upgrade_image" "$@"
