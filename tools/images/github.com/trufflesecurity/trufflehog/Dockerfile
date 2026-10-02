ARG IMAGE="ubuntu:24.04"
FROM $IMAGE

# fail a RUN step if any command in a pipeline fails
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

WORKDIR "/app"
COPY . /app

# Add trufflehog binary
ADD https://github.com/trufflesecurity/trufflehog/releases/download/v3.87.1/trufflehog_3.87.1_linux_amd64.tar.gz /app
# Verify the archive against the sha256 in the release's trufflehog_3.87.1_checksums.txt and extract it
RUN echo "49acfd8584d7db0904e03867be3de8ff91fc3125b604744bed87e88048da569b  trufflehog_3.87.1_linux_amd64.tar.gz" | sha256sum -c - && \
    tar -xvf trufflehog_3.87.1_linux_amd64.tar.gz && \
    rm trufflehog_3.87.1_linux_amd64.tar.gz

ENTRYPOINT ["/bin/bash", "/app/wrapper.sh"]