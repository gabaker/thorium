# The ThorPal harness for Thorium workflow runs, from a prebuilt binary
#
# The build context holds a thorpal-harness binary built from this repo with
# `cargo build --release -p thorpal --bin thorpal-harness`; ../build-harness-image.sh
# builds it, stages it here, and builds this image. This is the image CI publishes:
# CI builds the binary on Ubuntu 22.04 and checks it with ../check-harness-binary.sh
# first. ./Dockerfile.source compiles the binary in the image build instead and has
# the same runtime as this one. The harness's agents use no Ghidra tools, so the
# image carries neither Ghidra nor a JDK.
#
# Override the base with `--build-arg IMAGE=<image>`. The binary links glibc,
# libssl.so.3, and libcrypto.so.3 dynamically, so the base needs a glibc at
# least as new as the build host's (2.35 for an Ubuntu 22.04 build), OpenSSL 3,
# and apt (Ubuntu 22.04 or newer, or Debian 12). The default is Ubuntu 22.04
# pinned by digest, as in ./Dockerfile.source; keep the two in step, and on the
# Ubuntu release of the CI base image (base/Dockerfile) that builds the binary.
ARG IMAGE="docker.io/library/ubuntu:22.04@sha256:076e7bf82319619f28dd0aa9c02ab34b055ad37100395b4f49a2ffba04fb5e11"
FROM ${IMAGE}

# the PEM text of a CA that apt should trust while this image builds, such as a
# TLS-intercepting proxy's; it is used for the package install only and is not
# added to the image's trust store, but like every build arg it is recorded in
# the image's history
ARG BUILD_CA_CERT=""

# fail a RUN step if any command in a pipeline fails
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# install the CA bundle and the OpenSSL 3 shared libraries the harness links,
# trusting BUILD_CA_CERT for this step only when it is set
RUN apt_opts=() && \
    if [ -n "$BUILD_CA_CERT" ]; then \
      printf '%s\n' "$BUILD_CA_CERT" > /tmp/build-ca.crt && \
      apt_opts=(-o Acquire::https::CAInfo=/tmp/build-ca.crt); \
    fi && \
    apt-get "${apt_opts[@]}" update && \
    apt-get "${apt_opts[@]}" install -y --no-install-recommends ca-certificates libssl3 && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/* /tmp/build-ca.crt

# run as an unprivileged user with a fixed uid/gid when the pod names no other (Thorium runs a
# job as its user's uid when that user has one)
RUN groupadd --system --gid 10001 thorpal && \
    useradd --system --uid 10001 --gid thorpal --create-home --home-dir /home/thorpal \
      --shell /usr/sbin/nologin thorpal

# the run timeout in seconds; a run still going when it expires fails
ENV THORPAL_HARNESS_TIMEOUT_SECS=600

# the harness reads the run's files from the standard Thorium paths
COPY thorpal-harness /usr/local/bin/thorpal-harness
RUN chmod +x /usr/local/bin/thorpal-harness

USER 10001:10001

ENTRYPOINT ["/usr/local/bin/thorpal-harness"]
# the harness takes no default arguments, so drop the base image's default command (bash)
CMD []
