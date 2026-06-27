ARG IMAGE="python:2.7-slim"
FROM $IMAGE

WORKDIR /app

# python:2.7-slim is built on Debian 10 "buster", which is EOL; its package
# mirrors have moved to archive.debian.org and the release files are no longer
# refreshed, so disable the valid-until check when fetching the index
RUN printf 'deb http://archive.debian.org/debian buster main\ndeb http://archive.debian.org/debian-security buster/updates main\n' > /etc/apt/sources.list && \
    printf 'Acquire::Check-Valid-Until "false";\n' > /etc/apt/apt.conf.d/99no-check-valid && \
    apt-get update && \
    apt-get install -y --no-install-recommends git && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# the base image already ships pip for Python 2.7 (no get-pip bootstrap needed)
RUN pip install balbuzard

# the wrapper runs the analyzer directly from the cloned source tree
RUN git clone https://github.com/decalage2/balbuzard.git

COPY wrapper.sh /app/.

ENTRYPOINT ["bash", "/app/wrapper.sh"]
