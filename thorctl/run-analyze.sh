#!/usr/bin/env bash
# run-analyze.sh - start the ThorPal analyze container with persistent volumes
#
# Starts the image built from thorctl/AnalyzeDockerfile and bind-mounts the
# host state ThorPal needs so nothing is lost between containers:
#   $THORIUM_DIR          -> /home/thorpal/.thorium        (rw: config/credentials,
#                            sessions, memories, scripts, logs)
#   $GHIDRA_PROJECTS_DIR  -> /home/thorpal/ghidra-projects (rw: Ghidra analysis
#                            databases + anonymized staging)
#   $SAMPLES_DIR          -> /home/thorpal/samples         (ro: binaries to analyze)
#   $GHIDRA_CACHE_VOLUME  -> /home/thorpal/.config/ghidra  (named volume: Ghidra's
#                            settings, java choice, and system OSGi/felix caches;
#                            prune it with `docker volume rm` when upgrading to a
#                            new Ghidra version so stale version dirs and old
#                            application.log files don't accumulate)
#
# All arguments are passed straight to thorctl inside the container. Reference
# samples by their in-container path /home/thorpal/samples/<file>. Sessions
# record absolute paths, so sessions created in the container resume in later
# containers (the mounts pin stable paths), but sessions created on the host
# will not resume in here and vice versa.
#
# Environment knobs (all optional):
#   THORPAL_IMAGE        image to run           (default thorpal-analyze:latest)
#   THORIUM_DIR          host thorium folder    (default ~/.thorium)
#   GHIDRA_PROJECTS_DIR  host projects folder   (default ~/ghidra-projects)
#   SAMPLES_DIR          host samples folder    (default current directory)
#   GHIDRA_CACHE_VOLUME  docker volume name     (default thorpal-ghidra-cache)
#   THORPAL_YARA_RULES   host YARA file or dir, mounted ro and wired up via
#                        THORCTL__AI__YARA_RULES
#   THORPAL_STRIP_PROXY  set to any non-empty value to NOT forward the proxy
#                        variables (HTTP_PROXY/HTTPS_PROXY/NO_PROXY and their
#                        lowercase forms) into the container - for hosts whose
#                        shell exports a proxy that does not exist (or is
#                        wrong) where the container's traffic egresses; the
#                        image bakes no proxy settings, so with this set the
#                        container runs fully proxy-free
#
# AWS_*/proxy/RUST_LOG/VISUAL/EDITOR and every THORCTL__* variable are passed
# through to the container; ~/.aws is mounted read-only when AWS_PROFILE is
# set. Credentials live only in the mounted config (or AWS env), never in the
# image. Caveat: in-container commands that WRITE the config (thorctl config,
# analyze setup, tokens activate/deactivate) persist any THORCTL__* overrides
# into the mounted config file - prefer flags or a container-dedicated config
# if the same file is also used by host-side thorctl.
#
# Examples:
#   ./run-analyze.sh analyze check
#   SAMPLES_DIR=/srv/samples ./run-analyze.sh \
#     analyze summary /home/thorpal/samples/suspect.exe
#   ./run-analyze.sh --config /home/thorpal/.thorium/thorpal-ai.yml \
#     analyze chat /home/thorpal/samples/suspect.exe --prompt "What does it do?"
#
# Note: the container runs as uid 1000. If your host user is a different uid,
# chown the mounted folders accordingly, or add:
#   --user "$(id -u):$(id -g)" -e HOME=/home/thorpal
# (the -e HOME matters: a passwd-less uid would otherwise get HOME=/ and miss
# every mount).
set -euo pipefail

# show the header comment as help without starting a container
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    awk 'NR == 1 { next } /^set -euo/ { exit } { sub(/^# ?/, ""); print }' "$0"
    exit 0
fi

# resolve the image and the host-side state locations
IMAGE="${THORPAL_IMAGE:-thorpal-analyze:latest}"
THORIUM_DIR="${THORIUM_DIR:-$HOME/.thorium}"
GHIDRA_PROJECTS_DIR="${GHIDRA_PROJECTS_DIR:-$HOME/ghidra-projects}"
SAMPLES_DIR="${SAMPLES_DIR:-$PWD}"
GHIDRA_CACHE_VOLUME="${GHIDRA_CACHE_VOLUME:-thorpal-ghidra-cache}"

# make sure the persistent host folders exist before docker mounts them
# (docker would otherwise create them root-owned)
mkdir -p "$THORIUM_DIR" "$GHIDRA_PROJECTS_DIR"

# absolutize every bind source: docker silently treats a relative -v source as
# a NAMED VOLUME, which would mount an empty dir where state was expected
THORIUM_DIR="$(realpath "$THORIUM_DIR")"
GHIDRA_PROJECTS_DIR="$(realpath "$GHIDRA_PROJECTS_DIR")"
if [[ ! -d "$SAMPLES_DIR" ]]; then
    echo "error: SAMPLES_DIR '$SAMPLES_DIR' does not exist" >&2
    exit 1
fi
SAMPLES_DIR="$(realpath "$SAMPLES_DIR")"

# the persistent state mounts plus the read-only sample mount
VOLUME_ARGS=(
    -v "$THORIUM_DIR:/home/thorpal/.thorium"
    -v "$GHIDRA_PROJECTS_DIR:/home/thorpal/ghidra-projects"
    -v "$SAMPLES_DIR:/home/thorpal/samples:ro"
    -v "$GHIDRA_CACHE_VOLUME:/home/thorpal/.config/ghidra"
)

# mount the AWS config read-only when a profile is in use (bedrock auth chain)
if [[ -n "${AWS_PROFILE:-}" && -d "$HOME/.aws" ]]; then
    VOLUME_ARGS+=(-v "$HOME/.aws:/home/thorpal/.aws:ro")
fi

# forward provider/proxy/logging/editor settings that are set in this shell;
# note plain (non-mantle) Bedrock rides the AWS SDK's own HTTP stack, whose
# proxy support differs from the reqwest-based providers
FORWARD_VARS=(
    AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN
    AWS_REGION AWS_DEFAULT_REGION AWS_PROFILE
    RUST_LOG VISUAL EDITOR
)
# the proxy group forwards only when THORPAL_STRIP_PROXY is unset: on an
# unproxied network a host shell's stale proxy exports would otherwise ride
# into the container and break every outbound provider call (the image bakes
# no proxy settings, so skipping the forward is a complete removal)
if [[ -z "${THORPAL_STRIP_PROXY:-}" ]]; then
    FORWARD_VARS+=(HTTP_PROXY HTTPS_PROXY NO_PROXY http_proxy https_proxy no_proxy)
fi
ENV_ARGS=()
for var in "${FORWARD_VARS[@]}"; do
    if [[ -n "${!var:-}" ]]; then
        ENV_ARGS+=(-e "$var")
    fi
done

# forward every THORCTL__* config override (see the write-back caveat in the
# header) - except a projects override pointing outside the bind mount, which
# would silently send analyses into the ephemeral container layer
while IFS= read -r var; do
    if [[ "$var" == "THORCTL__AI__GHIDRA_PROJECTS" && "${!var}" != /home/thorpal/* ]]; then
        echo "warning: ignoring THORCTL__AI__GHIDRA_PROJECTS='${!var}'" \
             "(not under /home/thorpal, projects would not land in the bind mount)" >&2
        continue
    fi
    ENV_ARGS+=(-e "$var")
done < <(compgen -e | grep '^THORCTL__' || true)

# mount an operator YARA pack and point ai.yara_rules at it when requested
if [[ -n "${THORPAL_YARA_RULES:-}" ]]; then
    if [[ ! -e "$THORPAL_YARA_RULES" ]]; then
        echo "error: THORPAL_YARA_RULES '$THORPAL_YARA_RULES' does not exist" >&2
        exit 1
    fi
    THORPAL_YARA_RULES="$(realpath "$THORPAL_YARA_RULES")"
    VOLUME_ARGS+=(-v "$THORPAL_YARA_RULES:/home/thorpal/yara-rules:ro")
    ENV_ARGS+=(-e "THORCTL__AI__YARA_RULES=/home/thorpal/yara-rules")
fi

# keep stdin open always; allocate a tty only when we actually have one so
# both the interactive TUI and piped headless runs behave
TTY_ARGS=(-i)
if [[ -t 0 && -t 1 ]]; then
    TTY_ARGS+=(-t)
fi

# run the container:
# - --init reaps the Ghidra JVM cleanly and forwards signals to thorctl
#   (in tty mode the docker client ignores SIGTERM - use Ctrl-C or docker stop)
# - --stop-timeout 90 gives thorctl time to snapshot the session and flush
#   Ghidra on SIGTERM before docker escalates to SIGKILL
exec docker run --rm "${TTY_ARGS[@]}" --init --stop-timeout 90 \
    "${VOLUME_ARGS[@]}" \
    "${ENV_ARGS[@]}" \
    "$IMAGE" "$@"
