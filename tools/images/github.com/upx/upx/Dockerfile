ARG IMAGE="ubuntu:24.04"
FROM $IMAGE AS builder

WORKDIR "/tmp"

RUN apt update && \
    apt install -y git cmake g++ && \
    apt clean && \
    rm -rf /var/lib/apt/lists/*

RUN git clone https://github.com/upx/upx.git

# The result uses different compression algorithms than the official release at https://github.com/upx/upx/releases/
# but seems compatible
RUN cd upx && \
    git submodule update --init && \
    mkdir -p build/release && cd build/release && \
    cmake ../.. && cd ../.. && \
    make

FROM $IMAGE

WORKDIR "/app"

COPY --from=builder /tmp/upx/build/release/upx /usr/local/bin/

COPY wrapper.sh /app/.

ENTRYPOINT ["bash", "/app/wrapper.sh"]