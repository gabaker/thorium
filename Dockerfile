# The core Thorium image, for linux/amd64 and linux/arm64. Each architecture is built as its own
# single-platform image (`docker build --platform linux/<arch>`), and CI joins the two into one
# multi-arch image. Every RUN step runs in the prep stage on the build machine's own architecture,
# and the arm64 image's stages only copy files, so building it needs no emulation.

# the server binaries the image runs, for each architecture: the x86_64 glibc build (cargo's
# default target directory on an x86_64 machine) and the aarch64 glibc cross build. Only the
# target architecture's stage is built, so an amd64 build needs no arm64 binaries
FROM scratch AS server-amd64
COPY ./target/release/thorium-api /thorium-api
COPY ./target/release/thorium-operator /thorium-operator
COPY ./target/release/thoradm /thoradm
COPY ./target/release/thorium-scaler /thorium-scaler
COPY ./target/release/thorium-search-streamer /thorium-search-streamer
COPY ./target/release/thorium-event-handler /thorium-event-handler

FROM scratch AS server-arm64
COPY ./target/aarch64-unknown-linux-gnu/release/thorium-api /thorium-api
COPY ./target/aarch64-unknown-linux-gnu/release/thorium-operator /thorium-operator
COPY ./target/aarch64-unknown-linux-gnu/release/thoradm /thoradm
COPY ./target/aarch64-unknown-linux-gnu/release/thorium-scaler /thorium-scaler
COPY ./target/aarch64-unknown-linux-gnu/release/thorium-search-streamer /thorium-search-streamer
COPY ./target/aarch64-unknown-linux-gnu/release/thorium-event-handler /thorium-event-handler

# the stage is one of the two above, chosen by the target architecture, not an image to tag
# hadolint ignore=DL3006
FROM server-${TARGETARCH} AS server

# the operating system the image runs on; nothing is run in it until the amd64 image installs its
# utilities
FROM ubuntu:24.04 AS os

# prepares every file of /app and the CA certificates on the build machine
FROM --platform=$BUILDPLATFORM ubuntu:24.04 AS prep

# fail a RUN step if any command in a pipeline fails
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# curl downloads crane, verified against the CA certificates the image is given too
RUN apt-get update -y && \
      apt-get install -y --no-install-recommends ca-certificates curl && \
      rm -rf /var/lib/apt/lists/*

# copy the app in
WORKDIR /staging/app
COPY --from=server / ./
# Add UI bundle to root path
COPY ./ui/dist ui
# copy their user and developer docs in
COPY ./api/docs/book docs/user
COPY ./target/doc docs/dev
# copy our binaries in
COPY ./target/x86_64-unknown-linux-musl/release/thorctl binaries/linux/x86-64/thorctl
COPY ./target/x86_64-unknown-linux-musl/release/thorium-agent binaries/linux/x86-64/thorium-agent
COPY ./target/x86_64-unknown-linux-musl/release/thorium-reactor binaries/linux/x86-64/thorium-reactor
COPY ./target/x86_64-unknown-linux-musl/release/thoradm binaries/linux/x86-64/thoradm
COPY ./target/x86_64-unknown-linux-musl/release/thorium-operator binaries/linux/x86-64/thorium-operator
# copy the Linux arm64 (aarch64 musl) binaries in
COPY ./target/aarch64-unknown-linux-musl/release/thorctl binaries/linux/arm64/thorctl
COPY ./target/aarch64-unknown-linux-musl/release/thorium-agent binaries/linux/arm64/thorium-agent
COPY ./target/aarch64-unknown-linux-musl/release/thorium-reactor binaries/linux/arm64/thorium-reactor
COPY ./target/aarch64-unknown-linux-musl/release/thoradm binaries/linux/arm64/thoradm
COPY ./target/aarch64-unknown-linux-musl/release/thorium-operator binaries/linux/arm64/thorium-operator
# copy windows binaries to target paths
COPY ./target/x86_64-pc-windows-gnu/release/thorctl.exe binaries/windows/x86-64/thorctl.exe
COPY ./target/x86_64-pc-windows-gnu/release/thorium-agent.exe binaries/windows/x86-64/thorium-agent.exe
COPY ./target/x86_64-pc-windows-gnu/release/thorium-reactor.exe binaries/windows/x86-64/thorium-reactor.exe
# copy macos binaries to target paths
COPY ./target/x86_64-apple-darwin/release/thorctl binaries/darwin/x86-64/thorctl
COPY ./target/x86_64-apple-darwin/release/thoradm binaries/darwin/x86-64/thoradm
COPY ./target/aarch64-apple-darwin/release/thorctl binaries/darwin/arm64/thorctl
COPY ./target/aarch64-apple-darwin/release/thoradm binaries/darwin/arm64/thoradm
# copy the thorctl install script to the right path
COPY ./api/docs/src/scripts/install-thorctl.sh binaries/install-thorctl.sh
# add banner to default path
COPY ./ui/src/assets/banner.txt banner.txt

# make glibc compiled and cross compiled binaries executable, and serve the Linux arm64 binaries
# under aarch64 too (the arch `uname -m` and the self-updaters name on Linux)
RUN chmod +x thorium-api \
      thoradm \
      thorium-operator \
      thorium-scaler \
      thorium-search-streamer \
      thorium-event-handler && \
    chmod -R +x binaries && \
    ln -s arm64 binaries/linux/aarch64

# the crane release to install and the sha256 of its Linux x86_64 and arm64 archives from the
# release's checksums.txt; a new crane version needs both checksums changed with it
# renovate: datasource=github-releases depName=google/go-containerregistry
ARG CRANE_VERSION=v0.22.1
ARG CRANE_SHA256_AMD64=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0
ARG CRANE_SHA256_ARM64=898c0cff975f898a33e8c4580bdafb0e7c02c7faa33374e946762f97c4ab7110
# the architecture of the image being built, which may not be the build machine's
ARG TARGETARCH

# download and verify the target architecture's crane
RUN case "$TARGETARCH" in \
      amd64) archive=Linux_x86_64; sum="$CRANE_SHA256_AMD64" ;; \
      arm64) archive=Linux_arm64; sum="$CRANE_SHA256_ARM64" ;; \
      *) echo "no crane archive is pinned for ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    curl -fsSL -o go-containerregistry.tar.gz "https://github.com/google/go-containerregistry/releases/download/${CRANE_VERSION}/go-containerregistry_${archive}.tar.gz" && \
    echo "${sum}  go-containerregistry.tar.gz" | sha256sum -c - && \
    tar xzf go-containerregistry.tar.gz crane && \
    rm go-containerregistry.tar.gz

# the files both architectures' images share; copies only, so no step runs on the target
FROM os AS base
COPY --from=prep /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
WORKDIR /app
COPY --from=prep /staging/app/ ./

# the amd64 image keeps the utilities developers debug the containers with
FROM base AS final-amd64
RUN apt-get update -y && \
      apt-get install -y --no-install-recommends lsof curl jq ca-certificates && \
      rm -rf /var/lib/apt/lists/*

# the arm64 image runs nothing on arm64 while it is built, so it has no utilities beyond the
# operating system's; debug its containers with `kubectl debug` and an image that has them
FROM base AS final-arm64

# the target architecture's final stage above; the image runs as root, as this release's
# operator expects
# hadolint ignore=DL3006
FROM final-${TARGETARCH}

ENTRYPOINT ["./thorium-operator", "operate"]
