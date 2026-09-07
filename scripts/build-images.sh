#!/usr/bin/env bash
# Build the multi-architecture server, worker and hybrid images.
#
# The same thing the publish workflow does, runnable by hand.
#
#   aurcache-server  the backend alone -- what a split deployment runs.
#   aurcache-worker  a build worker, for a split deployment.
#   aurcache         the hybrid (server + embedded worker) compatibility image.
#
# The worker and hybrid images install AURCache as Arch packages, which the
# packager stage cross-compiles on the build host; only their runtime stages are
# per-architecture, so an emulated build spends its time on `pacman -U` rather
# than on cargo. The server image is Debian and cross-compiles the same way.
#
#   scripts/build-images.sh docker.example.com
#   scripts/build-images.sh --push --tag v0.5.0 docker.example.com
#   scripts/build-images.sh --images worker --platforms linux/amd64 localhost:5000
#
# armv7 needs a cross toolchain that is not in Arch's repositories and takes
# the better part of an hour to build from the AUR. Point `--toolchain-repo` at
# a pacman repository that already has it -- an AURCache instance, for
# example -- and that becomes a few seconds. See packaging/build-cross-toolchain.sh.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname -- "$SCRIPT_DIR")"
readonly SCRIPT_DIR REPO_ROOT

readonly DEFAULT_PLATFORMS="linux/amd64,linux/arm64,linux/arm/v7"
readonly DEFAULT_BUILDER="aurcache-multiarch"

usage() {
    sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//; $d'
    cat <<'EOF'
Options:
  -t, --tag TAG             Image tag (default: latest)
  -i, --images LIST         Comma-separated: server, worker, hybrid
                            (default: all three)
  -p, --platforms LIST      Target platforms (default: linux/amd64,linux/arm64,linux/arm/v7)
      --push                Push to the registry; without it the images are
                            built and discarded (see the note below)
      --toolchain-repo URL  pacman repository holding the prebuilt armv7 cross
                            toolchain, e.g. 'http://host:8081/$arch'
      --toolchain-repo-name NAME
                            That repository's section name, which decides the
                            database file pacman fetches (default: repo)
      --builder NAME        buildx builder to use (default: aurcache-multiarch,
                            created on demand)
  -h, --help                This message

Without --push, what happens to the result depends on the daemon's image
store. The classic store holds one architecture per tag, so a multi-platform
build is discarded and only the build cache survives -- still useful for
checking that every architecture builds. Docker's containerd image store holds
multi-arch images, and this script loads into it when the daemon has it
enabled. A single-platform build is always loaded.
EOF
}

tag=latest
images=server,worker,hybrid
platforms=$DEFAULT_PLATFORMS
push=0
toolchain_repo=
toolchain_repo_name=
builder=$DEFAULT_BUILDER
registry=

while (($#)); do
    case $1 in
        -t | --tag) tag=$2; shift 2 ;;
        -i | --images) images=$2; shift 2 ;;
        -p | --platforms) platforms=$2; shift 2 ;;
        --push) push=1; shift ;;
        --toolchain-repo) toolchain_repo=$2; shift 2 ;;
        --toolchain-repo-name) toolchain_repo_name=$2; shift 2 ;;
        --builder) builder=$2; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *)
            [[ -n $registry ]] && { echo "unexpected argument: $1" >&2; exit 2; }
            registry=$1
            shift
            ;;
    esac
done

if [[ -z $registry ]]; then
    echo "error: no registry/namespace given" >&2
    usage >&2
    exit 2
fi

# A multi-platform build needs the docker-container driver; the default `docker`
# driver builds for one architecture only and fails with a message that does not
# say so. Created rather than demanded, since one builder is as good as another.
if [[ $platforms == *,* ]] && ! docker buildx inspect "$builder" >/dev/null 2>&1; then
    echo "==> creating buildx builder '$builder'"
    docker buildx create --name "$builder" --driver docker-container --bootstrap >/dev/null
fi
builder_args=()
[[ $platforms == *,* ]] && builder_args=(--builder "$builder")

# Foreign runtime stages run under qemu-user through binfmt. Checked rather than
# assumed: without it the build fails partway with "exec format error", which
# does not point at the cause.
if [[ $platforms == *arm* ]] && [[ ! -e /proc/sys/fs/binfmt_misc/qemu-arm ]] &&
    [[ ! -e /proc/sys/fs/binfmt_misc/qemu-aarch64 ]]; then
    echo "warning: no qemu binfmt handler registered; ARM stages will fail." >&2
    echo "         register one with: docker run --privileged --rm tonistiigi/binfmt --install all" >&2
fi

# Where the result goes.
#
# The classic image store holds one architecture per tag, so a multi-platform
# build cannot be loaded into it -- buildx refuses, and the only options are
# pushing or discarding. Docker's containerd image store does hold multi-arch
# images, so when the daemon is using it the build can be loaded and run
# locally. Enable it with, in /etc/docker/daemon.json:
#
#   { "features": { "containerd-snapshotter": true } }
#
# and restart the daemon. Images in the classic store stop being *visible*
# while it is active (they are not deleted, and turning it off brings them
# back), which is worth knowing before switching on a machine with a lot of
# them.
containerd_store=0
if docker info --format '{{.DriverStatus}}' 2>/dev/null | grep -q 'io.containerd.snapshotter'; then
    containerd_store=1
fi

if ((push)); then
    output_args=(--push)
elif ((containerd_store)); then
    output_args=(--load)
elif [[ $platforms == *,* ]]; then
    output_args=(--output "type=cacheonly")
    echo "note: the daemon uses the classic image store, which cannot hold a"
    echo "      multi-platform image, so this build will be discarded. Enable"
    echo "      the containerd image store to keep it, or use --push."
else
    # One platform: the classic store can hold that.
    output_args=(--load)
fi

build_args=()
if [[ -n $toolchain_repo ]]; then
    build_args+=(--build-arg "AURCACHE_TOOLCHAIN_REPO=$toolchain_repo")
fi
if [[ -n $toolchain_repo_name ]]; then
    build_args+=(--build-arg "AURCACHE_TOOLCHAIN_REPO_NAME=$toolchain_repo_name")
fi
if [[ $platforms == *arm/v7* ]] && [[ -z $toolchain_repo ]]; then
    echo "note: building armv7 without --toolchain-repo; the cross toolchain is"
    echo "      built from the AUR and takes roughly an hour the first time."
fi

# The image each Dockerfile publishes as. `aurcache` rather than
# `aurcache-hybrid` because that is the name deployments predating the split
# already pull.
declare -A DOCKERFILES=(
    [server]=docker/Dockerfile
    [worker]=docker/worker.Dockerfile
    [hybrid]=docker/hybrid.Dockerfile
)
declare -A IMAGE_NAMES=(
    [server]=aurcache-server
    [worker]=aurcache-worker
    [hybrid]=aurcache
)

IFS=',' read -r -a selected <<<"$images"
for image in "${selected[@]}"; do
    dockerfile=${DOCKERFILES[$image]:-}
    if [[ -z $dockerfile ]]; then
        echo "error: unknown image '$image' (expected server, worker or hybrid)" >&2
        exit 2
    fi

    ref="$registry/${IMAGE_NAMES[$image]}:$tag"
    echo
    echo "==> $image -> $ref  [$platforms]"
    # The cross-toolchain args belong to the Arch packager stages. The server
    # image is Debian and declares neither, and buildkit warns about a build arg
    # no stage consumes, so it is built without them.
    image_build_args=("${build_args[@]}")
    [[ $image == server ]] && image_build_args=()
    docker buildx build "${builder_args[@]}" \
        --platform "$platforms" \
        --file "$REPO_ROOT/$dockerfile" \
        --tag "$ref" \
        "${image_build_args[@]}" \
        "${output_args[@]}" \
        "$REPO_ROOT"
done

echo
if ((push)); then
    echo "==> pushed $images as :$tag to $registry"
elif ((containerd_store)) || [[ $platforms != *,* ]]; then
    echo "==> built and loaded $images for $platforms"
else
    echo "==> built $images for $platforms (discarded; --push to publish)"
fi
