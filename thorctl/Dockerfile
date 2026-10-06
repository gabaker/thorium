FROM ubuntu:22.04

WORKDIR /bin
COPY ./target/x86_64-unknown-linux-musl/release/thorctl .

ENTRYPOINT ["./thorctl", "--admin", "/conf/thorium.yml", "--keys", "/keys/keys.yml"]
