FROM ubuntu:22.04

RUN apt update \
    && apt install -y binutils \
    && apt clean \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# set command
ENTRYPOINT ["/app/wrapper.sh"]