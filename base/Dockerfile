FROM thorium-registry.sandia.gov/base/ubuntu:latest

# fail a RUN step if any command in a pipeline fails
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# update apt and install curl
RUN apt-get update && apt-get install -y curl
# the rustup release to install and the sha256 of its x86_64 Linux rustup-init from static.rust-lang.org
ARG RUSTUP_VERSION=1.29.1
ARG RUSTUP_INIT_SHA256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
# install rust
RUN curl --proto '=https' --tlsv1.2 -sSf -o rustup-init "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" && \
      echo "${RUSTUP_INIT_SHA256}  rustup-init" | sha256sum -c - && \
      chmod +x rustup-init && \
      ./rustup-init --default-toolchain nightly -y && \
      rm rustup-init
# install build dependencies
RUN apt-get update && \
      apt-get install -y \
            build-essential \
            libssl-dev \
            pkg-config \
            libclang-dev \
            clang-15 \
            musl-tools \
            cmake \
            libvirt-dev \
            jq \
            python3.10-dev \
            python3.10-venv
ENV PATH="/root/.local/bin:$PATH"
RUN python3 -m pip install --no-cache-dir --user pipx && \
      python3 -m pipx ensurepath --global && \
      pipx install maturin && \
      pipx install patchelf
# add cargo to PATH and install rust nightly and musl builds
ENV PATH=${PATH}:/root/.cargo/bin
RUN rustup target add x86_64-unknown-linux-musl
# download and install redis for integration testing (sha256 from github.com/redis/redis-hashes)
RUN apt-get install -y wget && \
      curl -fL --output redis-6.2.4.tar.gz https://download.redis.io/releases/redis-6.2.4.tar.gz && \
      echo "ba32c406a10fc2c09426e2be2787d74ff204eb3a2e496d87cff76a476b6ae16e  redis-6.2.4.tar.gz" | sha256sum -c - && \
      tar xzf redis-6.2.4.tar.gz && \
      cd redis-6.2.4 && \
      make && \
      make install
# download and install scylla for integration testing
RUN wget https://s3.amazonaws.com/downloads.scylladb.com/deb/scylladb.gpg.pubkey && \
      gpg --homedir /tmp --no-default-keyring --keyring /etc/apt/keyrings/scylladb.gpg --keyserver hkp://keyserver.ubuntu.com:80 --keyserver-options http-proxy=http://thorium:pass@proxy.sandia.gov --recv-keys a43e06657bac99e3 && \
      wget -O /etc/apt/sources.list.d/scylla.list http://downloads.scylladb.com/deb/debian/scylla-6.2.list && \
      apt-get update && \
      apt-get install -y scylla && \
      rm scylladb.gpg.pubkey
# set the locales and setup sculla
ENV LC_ALL="en_US.UTF-8"
RUN apt-get install -y locales && \
      locale-gen en_US.UTF-8 && \
      dpkg-reconfigure locales && \
      scylla_dev_mode_setup --developer-mode 1
# the crane release to install and the sha256 of its Linux x86_64 archive from the release's checksums.txt
ARG CRANE_VERSION=v0.22.1
ARG CRANE_SHA256=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0
# download and verify crane
RUN mkdir app && \
      cd app && \
      curl -fsSL -o go-containerregistry.tar.gz "https://github.com/google/go-containerregistry/releases/download/${CRANE_VERSION}/go-containerregistry_Linux_x86_64.tar.gz" && \
      echo "${CRANE_SHA256}  go-containerregistry.tar.gz" | sha256sum -c - && \
      tar xzf go-containerregistry.tar.gz crane && \
      rm go-containerregistry.tar.gz && \
      cd ..
# grab the Thorium binaries
COPY ./ thorium
# fetch our dependencies
RUN cd thorium && \
      cargo fetch
