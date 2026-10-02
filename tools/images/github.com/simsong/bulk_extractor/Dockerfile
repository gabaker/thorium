ARG IMAGE="ubuntu:24.04"
FROM $IMAGE

WORKDIR /app

# install bulk_extractor deps
RUN apt-get update && \
    apt-get install -y git python3 autoconf make build-essential flex libexpat1-dev \
    libxml2-utils libewf-dev libssl-dev libtool make pkg-config zlib1g-dev libre2-dev && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# the bulk_extractor release to build (v2.1.1) and its commit
ARG BULK_EXTRACTOR_COMMIT="b2279ce0c7e85cf5a68cbc1d130ffc4ac322f263"

# fetch the pinned commit, verify it, and fetch the submodule commits it records
RUN git init -q /app/bulk_extractor && \
    git -C /app/bulk_extractor fetch -q --depth 1 https://github.com/simsong/bulk_extractor.git "$BULK_EXTRACTOR_COMMIT" && \
    git -C /app/bulk_extractor checkout -q --detach FETCH_HEAD && \
    test "$(git -C /app/bulk_extractor rev-parse HEAD)" = "$BULK_EXTRACTOR_COMMIT" && \
    git -C /app/bulk_extractor submodule update -q --init --recursive --depth 1

# configure and install bulk_extractor
RUN cd /app/bulk_extractor && \
    sh ./bootstrap.sh && \
    ./configure && \
    make -j32 && \
    make install

COPY wrapper.sh /app/.

ENTRYPOINT ["bash", "/app/wrapper.sh"]