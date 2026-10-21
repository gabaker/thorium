#!/usr/bin/env bash
# Build the ThorPal harness image from this repo and tag it for a registry
#
#   REGISTRY=myreg:5000/thorium ./build-harness-image.sh [--push] [--tag X] [--toolbox] [--dry-run]
#   ./build-harness-image.sh --from-source [...]
#   ./build-harness-image.sh --stage <DIR>
#
# The image is <REGISTRY>/thorpal-harness:<TAG>. TAG defaults to the harness tool's version in
# harness/manifest.toml, which is also the tag `thorctl toolbox build` derives for the harness tool's
# spec, so the image and the toolbox agree unless --tag says otherwise. REGISTRY defaults to the
# registry in this toolbox's config.toml. Run with --help for every option.
set -euo pipefail

# the toolbox directory (this script's directory) and the repo root above it
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)

# print the usage
usage() {
  cat <<'EOF'
Usage: build-harness-image.sh [OPTIONS]

By default, builds thorpal-harness on this host (cargo build --release -p thorpal --bin
thorpal-harness), stages it with harness/Dockerfile in a temporary build context, builds
<REGISTRY>/thorpal-harness:<TAG>, and prints the image reference.

With --from-source the binary is compiled inside the image build instead (harness/Dockerfile.source,
for a host without a Rust toolchain): the build context is staged from git, this repo's committed HEAD with no
.git directory, and cargo fetches the git dependencies at the commits Cargo.lock pins.
--stage <DIR> stages that context into DIR and stops, to build it elsewhere.

Options:
  --registry <REGISTRY>       Registry (and path) to tag for (env REGISTRY; default: config.toml's registry)
  --tag <TAG>                 Image tag (env TAG; default: the harness manifest's version)
  --push                      Push the image after building it
  --push-registry <REGISTRY>  Push to this registry instead of REGISTRY, e.g. a port-forward
                              (localhost:5000) of an in-cluster registry that pods pull from as
                              REGISTRY (env PUSH_REGISTRY)
  --toolbox                   Also write toolbox.local.json here with REGISTRY as its registry, so the
                              harness tool's spec names the image just built (needs thorctl; env THORCTL)
  --base-image <IMAGE>        Base image (build arg IMAGE; default: the Dockerfile's ubuntu:24.04)
  --ca-cert <FILE>            A CA certificate (PEM) the build trusts while it downloads, such as a
                              TLS-intercepting proxy's (build arg BUILD_CA_CERT; env BUILD_CA_CERT_FILE)
  --binary <PATH>             Use this thorpal-harness binary instead of building it
  --runtime <docker|podman>   Container runtime (env CONTAINER_RUNTIME; default: docker, else podman)
  --pull                      Pull the base image even when a copy is cached
  --no-cache                  Build without the layer cache
  --label <KEY=VALUE>         Add a label to the image (repeatable)
  --dry-run                   Print the commands and the image reference; build and push nothing
  -h, --help                  Show this help

Source build options:
  --from-source               Compile the harness in the image build (harness/Dockerfile.source)
  --stage <DIR>               Only stage the source build context into DIR (new or empty) and print it
  --jobs <N>                  The most compile jobs cargo runs in the image build (build arg
                              BUILD_JOBS; env CARGO_BUILD_JOBS; default: every CPU)

Env:
  CARGO                       The cargo command (default: cargo); CARGO_TARGET_DIR is honored
  http_proxy, https_proxy, no_proxy (and upper case)
                              Passed to the build as build args when set
EOF
}

# read a top-level string key from a TOML file (enough for this toolbox's own files)
#
# Arguments:
#   $1 - the TOML file
#   $2 - the key
toml_value() {
  sed -n "s/^$2[[:space:]]*=[[:space:]]*\"\(.*\)\"[[:space:]]*$/\1/p" "$1" | head -n 1
}

# print a command, then run it unless this is a dry run
#
# Arguments:
#   $@ - the command
run() {
  local arg name value scheme
  printf '+'
  # name the CA file rather than printing its PEM text, and hide the user info of proxy urls
  for arg in "$@"; do
    case "$arg" in
      BUILD_CA_CERT=*) printf ' %q' "BUILD_CA_CERT=<$ca_cert>" ;;
      no_proxy=* | NO_PROXY=*) printf ' %q' "$arg" ;;
      *_proxy=*@* | *_PROXY=*@*)
        name=${arg%%=*}
        value=${arg#*=}
        scheme=
        if [[ "$value" == *://* ]]; then
          scheme="${value%%://*}://"
          value=${value#*://}
        fi
        printf ' %q' "$name=${scheme}<redacted>@${value##*@}"
        ;;
      *) printf ' %q' "$arg" ;;
    esac
  done
  printf '\n'
  if [ "$dry_run" = 0 ]; then
    "$@"
  fi
}

# stage the source build context: this repo's committed HEAD in thorium/
#
# Arguments:
#   $1 - the directory to stage into (new or empty)
stage_context() {
  local dest=$1 head
  if [ -e "$dest" ] && [ -n "$(ls -A "$dest")" ]; then
    echo "the staging directory $dest is not empty" >&2
    exit 2
  fi
  mkdir -p "$dest/thorium"
  head=$(git -C "$repo" rev-parse HEAD)
  if [ -n "$(git -C "$repo" status --porcelain --untracked-files=no)" ]; then
    echo "WARNING: $repo has uncommitted changes; the source build uses the committed HEAD $head" >&2
  fi
  # the repo's committed tree, without the directories no Rust crate builds from (the user docs,
  # UI, deployment, tools, and CI files), which keeps the context small
  git -C "$repo" archive --format=tar HEAD -- . ':(exclude)api/docs' ':(exclude)ui' \
    ':(exclude)deploy' ':(exclude)megathor' ':(exclude)minithor' \
    ':(exclude)tools' ':(exclude)base' ':(exclude)skills' ':(exclude).github' ':(exclude).gitlab' |
    tar -x -C "$dest/thorium"
  echo "thorium: staged $head"
}

# the options and their env defaults
registry=${REGISTRY:-}
push_registry=${PUSH_REGISTRY:-}
tag=${TAG:-}
push=0
toolbox=0
dry_run=0
base_image=""
ca_cert=${BUILD_CA_CERT_FILE:-}
binary=""
runtime=${CONTAINER_RUNTIME:-}
pull=0
no_cache=0
labels=()
from_source=0
stage_dir=""
jobs=${CARGO_BUILD_JOBS:-}
# parse the command line
while [ $# -gt 0 ]; do
  case "$1" in
    --registry) registry=${2:?--registry needs a value}; shift 2 ;;
    --tag) tag=${2:?--tag needs a value}; shift 2 ;;
    --push) push=1; shift ;;
    --push-registry) push_registry=${2:?--push-registry needs a value}; shift 2 ;;
    --toolbox) toolbox=1; shift ;;
    --base-image) base_image=${2:?--base-image needs a value}; shift 2 ;;
    --ca-cert) ca_cert=${2:?--ca-cert needs a value}; shift 2 ;;
    --binary) binary=${2:?--binary needs a value}; shift 2 ;;
    --runtime) runtime=${2:?--runtime needs a value}; shift 2 ;;
    --pull) pull=1; shift ;;
    --no-cache) no_cache=1; shift ;;
    --label) labels+=(--label "${2:?--label needs a value}"); shift 2 ;;
    --from-source) from_source=1; shift ;;
    --stage) stage_dir=${2:?--stage needs a value}; from_source=1; shift 2 ;;
    --jobs) jobs=${2:?--jobs needs a value}; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
done
# the staging work directory, removed on exit
work=$(mktemp -d "${TMPDIR:-/tmp}/thorpal-harness-image.XXXXXX")
trap 'rm -rf "$work"' EXIT
# stage the source build context and stop when that is all that was asked
if [ -n "$stage_dir" ]; then
  if [ "$dry_run" = 1 ]; then
    echo "+ stage this repo's committed HEAD in $stage_dir/thorium"
  else
    stage_context "$stage_dir"
  fi
  echo "$stage_dir"
  exit 0
fi
# default the registry to the toolbox's and the tag to the harness tool's version
[ -n "$registry" ] || registry=$(toml_value "$here/config.toml" registry)
[ -n "$tag" ] || tag=$(toml_value "$here/harness/manifest.toml" version)
registry=${registry%/}
push_registry=${push_registry%/}
if [ -z "$registry" ]; then
  echo "no registry: set REGISTRY, pass --registry, or set registry in $here/config.toml" >&2
  exit 2
fi
if [ -z "$tag" ]; then
  echo "no tag: pass --tag or set version in $here/harness/manifest.toml" >&2
  exit 2
fi
# the toolbox derives the harness image from the manifest's version, so a different tag would not match it
version=$(toml_value "$here/harness/manifest.toml" version)
if [ "$toolbox" = 1 ] && [ "$tag" != "$version" ]; then
  echo "--toolbox derives the tag $version from harness/manifest.toml; drop --tag or change the version there" >&2
  exit 2
fi
if [ -n "$ca_cert" ] && [ ! -f "$ca_cert" ]; then
  echo "--ca-cert $ca_cert is not a file" >&2
  exit 2
fi
if [ "$from_source" = 1 ] && [ -n "$binary" ]; then
  echo "--binary and --from-source both pick where the binary comes from; pass one" >&2
  exit 2
fi
# pick the container runtime
if [ -z "$runtime" ]; then
  if command -v docker >/dev/null 2>&1; then runtime=docker; else runtime=podman; fi
fi
image="$registry/thorpal-harness:$tag"
# collect the build args: the base image, the build CA, and the proxy settings from the env
build_args=()
[ -z "$base_image" ] || build_args+=(--build-arg "IMAGE=$base_image")
[ -z "$ca_cert" ] || build_args+=(--build-arg "BUILD_CA_CERT=$(cat "$ca_cert")")
for var in http_proxy https_proxy no_proxy HTTP_PROXY HTTPS_PROXY NO_PROXY; do
  if [ -n "${!var:-}" ]; then
    build_args+=(--build-arg "$var=${!var}")
  fi
done
[ "$pull" = 0 ] || build_args+=(--pull)
[ "$no_cache" = 0 ] || build_args+=(--no-cache)
if [ "$from_source" = 1 ]; then
  # stage the sources and build them in the image
  context="$work/context"
  if [ "$dry_run" = 1 ]; then
    echo "+ stage this repo's committed HEAD in $context/thorium"
  else
    stage_context "$context"
  fi
  [ -z "$jobs" ] || build_args+=(--build-arg "BUILD_JOBS=$jobs")
  # record where the image came from
  labels+=(--label "org.opencontainers.image.revision=$(git -C "$repo" rev-parse HEAD)"
    --label "org.opencontainers.image.version=$tag")
  run "$runtime" build "${build_args[@]}" "${labels[@]}" \
    -f "$context/thorium/thorpal/toolbox/harness/Dockerfile.source" -t "$image" "$context"
else
  # build the harness binary unless one was given
  if [ -z "$binary" ]; then
    cargo_cmd=${CARGO:-cargo}
    ( cd "$repo" && run "$cargo_cmd" build --release --locked -p thorpal --bin thorpal-harness )
    binary=${CARGO_TARGET_DIR:-$repo/target}/release/thorpal-harness
  fi
  if [ "$dry_run" = 0 ] && [ ! -x "$binary" ]; then
    echo "no thorpal-harness binary at $binary" >&2
    exit 1
  fi
  # stage the binary and the Dockerfile in the temporary build context
  context="$work/context"
  mkdir -p "$context"
  cp "$here/harness/Dockerfile" "$context/Dockerfile"
  if [ "$dry_run" = 0 ]; then
    cp "$binary" "$context/thorpal-harness"
  fi
  # build the image
  run "$runtime" build "${build_args[@]}" ${labels[@]+"${labels[@]}"} -t "$image" "$context"
fi
# push it, under the push registry when one is set
if [ "$push" = 1 ]; then
  pushed=$image
  if [ -n "$push_registry" ]; then
    pushed="$push_registry/thorpal-harness:$tag"
    run "$runtime" tag "$image" "$pushed"
  fi
  run "$runtime" push "$pushed"
  if [ -n "$push_registry" ]; then
    run "$runtime" rmi "$pushed"
  fi
fi
# write toolbox.local.json with this registry, from a copy of config.toml that names it
if [ "$toolbox" = 1 ]; then
  thorctl_cmd=${THORCTL:-thorctl}
  sed "s|^registry[[:space:]]*=.*|registry = \"$registry\"|" "$here/config.toml" > "$work/config.toml"
  run "$thorctl_cmd" toolbox build -c "$work/config.toml" --path "$here" -o "$here/toolbox.local.json"
fi
# print the image reference for the caller
echo "$image"
