#!/bin/sh
set -eu

script_name="$(basename "$0")"
script_dir="$(CDPATH= cd "$(dirname "$0")" && pwd)"
repo_root="$(CDPATH= cd "$script_dir/.." && pwd)"
compose_file="$repo_root/docker-compose.yml"
build_compose_file="$repo_root/docker-compose.build.yml"
image_state_file="${CROSSLANE_IMAGE_STATE_FILE:-$script_dir/.crosslane-image}"
default_image="ghcr.io/yaocool/crosslane:latest"

target="/usr/local/bin/crosslane"
project_name=""
health_timeout=60
deployment_mode="pull"
requested_image="${CROSSLANE_IMAGE:-}"
force=0
repair_volume=0

usage() {
  cat <<EOF
Usage: $script_name [OPTIONS]

Set up crosslane with Docker Compose and install a Docker-backed crosslane command
on the host.

Options:
  --target PATH         Host command path (default: /usr/local/bin/crosslane)
  --project-name NAME   Override the Docker Compose project name
  --timeout SECONDS     Health-check timeout (default: 60)
  --image IMAGE         Pull and deploy an image reference (default: saved image
                        or $default_image)
  --build               Build from source locally with the Compose build override
  --no-pull             Start the saved/existing image without pulling it
  --no-build            Deprecated alias for --no-pull
  --force               Replace an unrelated command already at --target
  --repair-volume       Recreate a Compose-owned volume whose data path is missing
  -h, --help            Show this help

Examples:
  ./deploy/$script_name
  ./deploy/$script_name --image ghcr.io/yaocool/crosslane:<published-tag>
  ./deploy/$script_name --target "\$HOME/.local/bin/crosslane"
  ./deploy/$script_name --build
  ./deploy/$script_name --no-pull --repair-volume
EOF
}

fail() {
  echo "$script_name: $*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      [ "$#" -ge 2 ] || fail "--target requires a path"
      target="$2"
      shift 2
      ;;
    --project-name)
      [ "$#" -ge 2 ] || fail "--project-name requires a name"
      project_name="$2"
      shift 2
      ;;
    --timeout)
      [ "$#" -ge 2 ] || fail "--timeout requires a number of seconds"
      health_timeout="$2"
      shift 2
      ;;
    --image)
      [ "$#" -ge 2 ] || fail "--image requires an image reference"
      [ -n "$2" ] || fail "--image must not be empty"
      requested_image="$2"
      deployment_mode="pull"
      shift 2
      ;;
    --build)
      deployment_mode="build"
      shift
      ;;
    --no-pull|--no-build)
      deployment_mode="existing"
      shift
      ;;
    --force)
      force=1
      shift
      ;;
    --repair-volume)
      repair_volume=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1 (use --help for usage)"
      ;;
  esac
done

[ -n "$target" ] || fail "--target must not be empty"
case "$health_timeout" in
  ''|*[!0-9]*) fail "--timeout must be a positive integer" ;;
esac
[ "$health_timeout" -gt 0 ] || fail "--timeout must be greater than zero"
[ -f "$compose_file" ] || fail "Compose file not found: $compose_file"
[ "$deployment_mode" != "build" ] || \
  [ -f "$build_compose_file" ] || fail "Compose build override not found: $build_compose_file"

if [ -z "$requested_image" ] && [ -f "$image_state_file" ]; then
  requested_image="$(sed -n '1p' "$image_state_file")"
fi
[ -n "$requested_image" ] || requested_image="$default_image"
case "$requested_image" in
  *[!A-Za-z0-9_./:@+-]*) fail "invalid image reference: $requested_image" ;;
esac
export CROSSLANE_IMAGE="$requested_image"

build_image="${CROSSLANE_BUILD_IMAGE:-crosslane:latest}"
case "$build_image" in
  ''|*[!A-Za-z0-9_./:@+-]*) fail "invalid CROSSLANE_BUILD_IMAGE: $build_image" ;;
esac
export CROSSLANE_BUILD_IMAGE="$build_image"

command -v docker >/dev/null 2>&1 || fail "docker command not found"
docker compose version >/dev/null 2>&1 || fail "Docker Compose v2 is required (docker compose)"
docker info >/dev/null 2>&1 || fail "cannot connect to Docker; start Docker and check your permissions"

installer="$script_dir/install-crosslane-wrapper.sh"
[ -x "$installer" ] || fail "wrapper installer is not executable: $installer"
target_dir="$(dirname "$target")"

invoke_wrapper_installer() {
  check_mode="$1"
  set -- "$installer" --target "$target"
  if [ "$force" -eq 1 ]; then
    set -- "$@" --force
  fi
  if [ "$check_mode" = "check" ]; then
    set -- "$@" --check
  fi

  if [ ! -d "$target_dir" ] || [ -w "$target_dir" ] || [ "$(id -u)" -eq 0 ]; then
    "$@"
  else
    command -v sudo >/dev/null 2>&1 || \
      fail "$target_dir is not writable and sudo is unavailable; use --target with a writable path"
    sudo "$@"
  fi
}

echo "==> Checking the host crosslane command target $target"
invoke_wrapper_installer check

compose_base() {
  if [ -n "$project_name" ]; then
    docker compose \
      --project-name "$project_name" \
      --project-directory "$repo_root" \
      --file "$compose_file" \
      "$@"
  else
    docker compose \
      --project-directory "$repo_root" \
      --file "$compose_file" \
      "$@"
  fi
}

compose() {
  if [ "$deployment_mode" != "build" ]; then
    compose_base "$@"
  elif [ -n "$project_name" ]; then
    docker compose \
      --project-name "$project_name" \
      --project-directory "$repo_root" \
      --file "$compose_file" \
      --file "$build_compose_file" \
      "$@"
  else
    docker compose \
      --project-directory "$repo_root" \
      --file "$compose_file" \
      --file "$build_compose_file" \
      "$@"
  fi
}

host_path_state() {
  path="$1"
  parent="$(dirname "$path")"
  nearest_parent="$parent"
  while [ ! -e "$nearest_parent" ] && [ ! -L "$nearest_parent" ] && [ "$nearest_parent" != "/" ]; do
    nearest_parent="$(dirname "$nearest_parent")"
  done

  if [ -L "$path" ]; then
    echo symlink
  elif [ -d "$path" ]; then
    echo directory
  elif [ -e "$path" ]; then
    echo other
  elif [ "$(id -u)" -eq 0 ] || [ -x "$nearest_parent" ]; then
    echo missing
  elif command -v sudo >/dev/null 2>&1; then
    if sudo test -L "$path"; then
      echo symlink
    elif sudo test -d "$path"; then
      echo directory
    elif sudo test -e "$path"; then
      echo other
    elif sudo test -x "$nearest_parent"; then
      echo missing
    else
      echo unknown
    fi
  else
    echo unknown
  fi
}

create_data_volume() {
  docker volume create \
    --driver local \
    --label "com.docker.compose.project=$selected_project" \
    --label "com.docker.compose.volume=crosslane-data" \
    "$data_volume_name" >/dev/null
}

prepare_data_volume() {
  volume_was_created=0
  if ! docker volume inspect "$data_volume_name" >/dev/null 2>&1; then
    echo "==> Creating Docker data volume $data_volume_name"
    create_data_volume
    volume_was_created=1
  fi

  volume_driver="$(docker volume inspect --format '{{.Driver}}' "$data_volume_name")"
  [ "$volume_driver" = "local" ] || {
    echo "==> Using Docker data volume $data_volume_name with driver $volume_driver"
    return
  }

  # Docker Desktop keeps the daemon's volume paths inside its Linux VM, so the
  # host cannot validate them directly. The affected stale-path failure occurs
  # on a native Linux Docker Engine and is checked below.
  [ "$(uname -s)" = "Linux" ] || return

  mountpoint="$(docker volume inspect --format '{{.Mountpoint}}' "$data_volume_name")"
  [ -n "$mountpoint" ] || fail "Docker returned an empty mountpoint for volume $data_volume_name"
  mountpoint_state="$(host_path_state "$mountpoint")"
  case "$mountpoint_state" in
    directory)
      return
      ;;
    unknown)
      echo "==> Warning: cannot verify Docker volume path $mountpoint; Docker will validate it during startup" >&2
      return
      ;;
    symlink|other)
      fail "Docker volume path is not a directory: $mountpoint; inspect Docker's data-root before continuing"
      ;;
  esac

  volume_dir="$(dirname "$mountpoint")"
  volume_root="$(dirname "$volume_dir")"
  volume_root_state="$(host_path_state "$volume_root")"
  [ "$volume_root_state" = "directory" ] || \
    fail "Docker volume root is unavailable: $volume_root; restore its disk mount or data-root before continuing"

  [ "$volume_was_created" -ne 1 ] || \
    fail "Docker created $data_volume_name but did not create its data path: $mountpoint; inspect the Docker daemon storage"

  volume_dir_state="$(host_path_state "$volume_dir")"
  case "$volume_dir_state" in
    directory|missing) ;;
    *)
      fail "Docker volume directory has an unsafe state ($volume_dir_state): $volume_dir; inspect it manually"
      ;;
  esac

  volume_project="$(docker volume inspect --format '{{index .Labels "com.docker.compose.project"}}' "$data_volume_name")"
  volume_logical_name="$(docker volume inspect --format '{{index .Labels "com.docker.compose.volume"}}' "$data_volume_name")"
  if [ "$volume_project" != "$selected_project" ] || [ "$volume_logical_name" != "crosslane-data" ]; then
    fail "volume $data_volume_name has a missing data path but is not owned by this Compose project; refusing to remove it"
  fi

  echo "==> Docker volume metadata exists but its data path is missing: $mountpoint" >&2
  if [ "$repair_volume" -ne 1 ]; then
    if [ -t 0 ]; then
      printf "Recreate the empty/broken volume %s? Existing inaccessible data will not be recoverable [y/N] " "$data_volume_name" >&2
      read -r answer
      case "$answer" in
        y|Y|yes|YES|Yes) ;;
        *) fail "volume repair cancelled; restore the missing data path or rerun with --repair-volume" ;;
      esac
    else
      fail "rerun with --repair-volume to recreate this broken volume non-interactively"
    fi
  fi

  echo "==> Recreating broken Docker data volume $data_volume_name"
  compose_base down --remove-orphans
  volume_users="$(docker ps --all --quiet --filter "volume=$data_volume_name")"
  [ -z "$volume_users" ] || \
    fail "volume $data_volume_name is still referenced by container(s): $volume_users"
  docker volume rm "$data_volume_name" >/dev/null
  create_data_volume

  repaired_mountpoint="$(docker volume inspect --format '{{.Mountpoint}}' "$data_volume_name")"
  repaired_state="$(host_path_state "$repaired_mountpoint")"
  [ "$repaired_state" = "directory" ] || \
    fail "Docker recreated $data_volume_name but its data path is still unavailable: $repaired_mountpoint"
}

echo "==> Validating Docker Compose configuration"
resolved_config="$(compose config)"
selected_project="$(printf '%s\n' "$resolved_config" | awk '
  /^name:[[:space:]]/ {
    sub(/^name:[[:space:]]*/, "")
    print
    exit
  }
')"
data_volume_name="$(printf '%s\n' "$resolved_config" | awk '
  $0 == "volumes:" { in_volumes = 1; next }
  in_volumes && $0 == "  crosslane-data:" { in_data_volume = 1; next }
  in_data_volume && /^    name:[[:space:]]/ {
    sub(/^    name:[[:space:]]*/, "")
    print
    exit
  }
')"
[ -n "$selected_project" ] || fail "could not resolve the Docker Compose project name"
[ -n "$data_volume_name" ] || fail "could not resolve the crosslane-data Docker volume name"
prepare_data_volume

install_host_command() {
  if [ ! -d "$target_dir" ]; then
    echo "==> Creating host command directory $target_dir"
    if mkdir -p "$target_dir" 2>/dev/null; then
      :
    elif [ "$(id -u)" -eq 0 ]; then
      install -d -m 0755 "$target_dir"
    else
      command -v sudo >/dev/null 2>&1 || \
        fail "cannot create $target_dir and sudo is unavailable; use --target with a writable path"
      sudo install -d -m 0755 "$target_dir"
    fi
  fi

  echo "==> Installing the host crosslane command at $target"
  invoke_wrapper_installer install
}

wait_for_health() {
  elapsed=0
  while :; do
    # Compose supports -T; --no-tty fails before crosslane health can even run.
    if health_output="$(compose_base exec -T crosslaned /usr/local/bin/crosslane health 2>&1)"; then
      return 0
    else
      health_status=$?
    fi
    if [ "$elapsed" -ge "$health_timeout" ]; then
      printf '==> Health check failed after %ss (exit %s):\n%s\n' \
        "$health_timeout" "$health_status" "$health_output" >&2
      return 1
    fi
    sleep 1
    elapsed=$((elapsed + 1))
  done
}

previous_container="$(compose_base ps --all --quiet crosslaned 2>/dev/null | awk 'NF { print; exit }')"
previous_image_id=""
rollback_image=""
if [ -n "$previous_container" ]; then
  previous_image_id="$(docker inspect --format '{{.Image}}' "$previous_container" 2>/dev/null || true)"
fi
if [ -n "$previous_image_id" ]; then
  rollback_suffix="$(printf '%s' "$selected_project" | tr -c 'A-Za-z0-9_.-' '-')"
  rollback_image="crosslane-rollback:${rollback_suffix}"
  echo "==> Saving the current image as $rollback_image"
  docker image tag "$previous_image_id" "$rollback_image"
fi

deployment_failed=0
case "$deployment_mode" in
  build)
    echo "==> Building crosslane locally and starting $build_image"
    if ! compose up --detach --build; then
      deployment_failed=1
    fi
    ;;
  existing)
    echo "==> Starting the existing image without pulling: $requested_image"
    if ! compose_base up --detach --no-build; then
      deployment_failed=1
    fi
    ;;
  pull)
    echo "==> Pulling image while the current container keeps running: $requested_image"
    if ! compose_base pull crosslaned; then
      fail "could not pull $requested_image; the current container was left untouched"
    fi
    echo "==> Replacing the container without building on this host"
    if ! compose_base up --detach --no-build; then
      deployment_failed=1
    fi
    ;;
esac

if [ "$deployment_failed" -eq 0 ]; then
  echo "==> Waiting for crosslaned to become healthy"
  wait_for_health || deployment_failed=1
fi

if [ "$deployment_failed" -ne 0 ]; then
  echo "==> Deployment failed; recent daemon logs follow" >&2
  compose_base logs --tail 80 crosslaned >&2 || true
  if [ -n "$rollback_image" ]; then
    echo "==> Rolling back to $rollback_image" >&2
    export CROSSLANE_IMAGE="$rollback_image"
    if compose_base up --detach --no-build && wait_for_health; then
      fail "new deployment failed; the previous image was restored successfully"
    fi
    compose_base logs --tail 80 crosslaned >&2 || true
    fail "new deployment failed and automatic rollback did not become healthy"
  fi
  fail "deployment failed and no previous image was available for rollback"
fi

# Keep the existing wrapper untouched until the matching image has passed its
# health check. The installer replaces it atomically.
install_host_command

echo "==> Verifying the host command"
CROSSLANE_COMPOSE_PROJECT="$selected_project" "$target" health

if [ "$deployment_mode" != "build" ]; then
  image_state_tmp="$image_state_file.tmp.$$"
  (umask 077 && printf '%s\n' "$requested_image" > "$image_state_tmp") || \
    fail "could not save the deployed image to $image_state_tmp"
  mv "$image_state_tmp" "$image_state_file" || \
    fail "could not save the deployed image to $image_state_file"
fi

if [ "$deployment_mode" = "build" ]; then
  deployed_image="$build_image"
else
  deployed_image="$requested_image"
fi

host_command="crosslane"
case ":${PATH}:" in
  *:"$target_dir":*) ;;
  *) host_command="$target" ;;
esac

cat <<EOF

crosslane is ready.

  Host command:  $target
  Compose stack: $selected_project
  Image:         $deployed_image
  Persistent data is stored in the crosslane-data Docker volume.

Next steps:
  $host_command help
  $host_command status
  $host_command service create --name main
  $host_command account import --stdin < accounts.json
  $host_command account probe --all

Save the API key printed by 'service create'. To inspect the service:
  $host_command service list

Follow daemon logs:
  docker compose --project-name "$selected_project" -f "$compose_file" logs -f crosslaned
EOF
