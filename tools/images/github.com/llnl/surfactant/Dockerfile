ARG IMAGE="python:3.11-slim"
FROM $IMAGE

WORKDIR /app

# git: required for the fuzzyhashes plugin's git+https install
# build-essential: the plugin pulls py-tlsh, which has no wheel and compiles C++
RUN apt-get update && \
    apt-get install -y --no-install-recommends git build-essential && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

### REPO: https://github.com/LLNL/Surfactant

# the surfactant release to install and its commit, which the fuzzyhashes plugin is installed from
ARG SURFACTANT_VERSION="0.0.0rc18"
ARG SURFACTANT_COMMIT="cfbaf38d0ba0352672b4128471a5c8a196ffb2ce"

# install surfactant and plugins; pip checks out the plugin at the pinned commit
RUN pip install --upgrade pip setuptools wheel && \
    pip install "surfactant==${SURFACTANT_VERSION}" && \
    pip install "git+https://github.com/LLNL/Surfactant@${SURFACTANT_COMMIT}#subdirectory=plugins/fuzzyhashes"

COPY wrapper.sh /app/.

ENTRYPOINT ["bash", "/app/wrapper.sh"]
