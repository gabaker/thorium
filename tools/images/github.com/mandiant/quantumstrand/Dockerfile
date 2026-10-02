ARG IMAGE="ubuntu:24.04"
FROM $IMAGE AS clone 

# fail a RUN step if any command in a pipeline fails
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

WORKDIR "/tmp"

RUN apt-get update && apt-get install -y unzip

ADD https://github.com/mandiant/flare-floss/releases/download/quantumstrand-beta1/quantumstrand-quantumstrand-beta1-linux.zip /tmp/

# Verify the quantumstrand archive against its pinned sha256 and unzip it
RUN echo "f4c5b9c738fef4add4d8eb18f4306466546cdc0429b39e60dd5282b25117c565  /tmp/quantumstrand-quantumstrand-beta1-linux.zip" | sha256sum -c - && \
    unzip /tmp/quantumstrand-quantumstrand-beta1-linux.zip

FROM $IMAGE

WORKDIR "/app"

COPY --from=clone /tmp/quantumstrand /app/quantumstrand
COPY wrapper.sh /app/.

ENTRYPOINT ["bash", "/app/wrapper.sh"]