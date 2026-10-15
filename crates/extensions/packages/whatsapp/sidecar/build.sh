#!/usr/bin/env bash
# Build the WhatsApp sidecar image from pinned local checkouts.
#
#   ./build.sh [extra docker build args...]
#
# Environment:
#   BAILEYS_DIR     Baileys checkout             (default: $HOME/oss/Baileys)
#   BRIDGE_DIR      whatsapp-rust-bridge checkout (default: $HOME/oss/whatsapp-rust-bridge)
#   IMAGE_TAG       image tag                    (default: ironclaw-whatsapp-sidecar:local)
#   BASE_REGISTRY   registry for the Docker official base images
#                   (default: docker.io/library; e.g. public.ecr.aws/docker/library).
#                   The images are pinned by digest, so a mirror cannot change them.
#   DOCKER_TARGET   build only up to this stage (bridge-build, baileys-build, app, runtime)
#
# The script checks that each checkout's HEAD is the pinned commit and that
# its working tree is clean, then passes the same commits to the build, where
# each stage checks them again and extracts its sources with `git archive`.
set -euo pipefail

BAILEYS_COMMIT=0af2386292907f7d9742d8d41f830d8c48208fa1
BRIDGE_COMMIT=4b185ecf26b2f0077606ba8b181cc6ae0d562ef9
WHATSAPP_RUST_COMMIT=88a0fe074fdc09d73be95cdd23743fba22ef172e

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BAILEYS_DIR="${BAILEYS_DIR:-$HOME/oss/Baileys}"
BRIDGE_DIR="${BRIDGE_DIR:-$HOME/oss/whatsapp-rust-bridge}"
IMAGE_TAG="${IMAGE_TAG:-ironclaw-whatsapp-sidecar:local}"
BASE_REGISTRY="${BASE_REGISTRY:-docker.io/library}"

fail() { echo "build.sh: $*" >&2; exit 1; }

check_checkout() {
  local name="$1" dir="$2" want="$3" head dirty
  [ -d "$dir/.git" ] || fail "$name: $dir is not a git checkout (no .git directory)"
  head="$(git -C "$dir" rev-parse HEAD)"
  [ "$head" = "$want" ] || fail "$name: $dir is at $head, expected $want"
  dirty="$(git -C "$dir" status --porcelain --untracked-files=no)"
  [ -z "$dirty" ] || fail "$name: $dir has uncommitted changes to tracked files"
  echo "$name: $dir at $head (pinned)"
}

check_checkout baileys "$BAILEYS_DIR" "$BAILEYS_COMMIT"
check_checkout whatsapp-rust-bridge "$BRIDGE_DIR" "$BRIDGE_COMMIT"

# The bridge's lockfile must pin whatsapp-rust at the expected commit.
lock_sources="$(grep -o 'git+https://github.com/jlucaso1/whatsapp-rust.git[^"]*' "$BRIDGE_DIR/Cargo.lock" | sort -u)"
[ "$lock_sources" = "git+https://github.com/jlucaso1/whatsapp-rust.git?branch=main#$WHATSAPP_RUST_COMMIT" ] \
  || fail "whatsapp-rust-bridge Cargo.lock does not pin whatsapp-rust at $WHATSAPP_RUST_COMMIT: $lock_sources"
echo "whatsapp-rust: locked at $WHATSAPP_RUST_COMMIT"

# Extra arguments go to `docker build`, but they may not override the pins
# checked above: the Dockerfile verifies the checkouts against whatever
# commits it is given.
expect_value=""
for arg in "$@"; do
  if [ -n "$expect_value" ]; then value="$arg"; expect_value=""
  else
    case "$arg" in
      --build-arg) expect_value=1; continue ;;
      --build-arg=*) value="${arg#--build-arg=}" ;;
      *) continue ;;
    esac
  fi
  case "${value%%=*}" in
    BAILEYS_COMMIT|BRIDGE_COMMIT|WHATSAPP_RUST_COMMIT)
      fail "${value%%=*} is pinned in build.sh and cannot be overridden" ;;
  esac
done

target_args=()
if [ -n "${DOCKER_TARGET:-}" ]; then
  target_args=(--target "$DOCKER_TARGET")
fi

set -x
DOCKER_BUILDKIT=1 docker build \
  --build-context "bridge=$BRIDGE_DIR" \
  --build-context "baileys=$BAILEYS_DIR" \
  --build-arg "BASE_REGISTRY=$BASE_REGISTRY" \
  --build-arg "BAILEYS_COMMIT=$BAILEYS_COMMIT" \
  --build-arg "BRIDGE_COMMIT=$BRIDGE_COMMIT" \
  --build-arg "WHATSAPP_RUST_COMMIT=$WHATSAPP_RUST_COMMIT" \
  "${target_args[@]}" \
  -t "$IMAGE_TAG" \
  "$@" \
  "$here"
