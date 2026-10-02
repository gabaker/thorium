ARG IMAGE="ubuntu:24.04"
FROM $IMAGE

# fail a RUN step if any command in a pipeline fails
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# The Trivy release to install and the sha256 of its .deb packages, taken from the release's
# trivy_<version>_checksums.txt (Sigstore-signed by the aquasecurity/trivy release workflow).
# Bump these together, and only to a release whose checksums and signature have been verified.
ARG TRIVY_VERSION=0.74.0
ARG TRIVY_SHA256_AMD64=cf1e32ec8d4d8823e023096a28cadb14f5b5123ce03f201fb633c5b76aa712dd
ARG TRIVY_SHA256_ARM64=14fae2604eab589fd544224ef56b8b8a44266872c43b259bd36cfde81428acc8

# download the pinned Trivy .deb for this architecture, verify its checksum, and install it
RUN apt-get update -y && \
  apt-get install -y ca-certificates wget && \
  case "$(dpkg --print-architecture)" in \
    amd64) TRIVY_ARCH=64bit; TRIVY_SHA256="${TRIVY_SHA256_AMD64}" ;; \
    arm64) TRIVY_ARCH=ARM64; TRIVY_SHA256="${TRIVY_SHA256_ARM64}" ;; \
    *) echo "unsupported architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
  esac && \
  wget -q -O /tmp/trivy.deb "https://github.com/aquasecurity/trivy/releases/download/v${TRIVY_VERSION}/trivy_${TRIVY_VERSION}_Linux-${TRIVY_ARCH}.deb" && \
  echo "${TRIVY_SHA256}  /tmp/trivy.deb" | sha256sum -c - && \
  dpkg -i /tmp/trivy.deb && \
  rm /tmp/trivy.deb && \
  trivy --version | grep -x "Version: ${TRIVY_VERSION}" && \
  rm -rf /var/lib/apt/lists/*

# prefetch and save our DB
RUN trivy fs --scanners vuln,secret,misconfig,license /tmp && \
    mv /root/.cache/* /tmp/cache/ && \
    chmod -R 777 /tmp/cache

# Add our wrapper.sh script
WORKDIR /app
COPY wrapper.sh .

# setup our entrypoint
ENTRYPOINT ["/app/wrapper.sh"]
