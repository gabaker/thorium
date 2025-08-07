FROM ubuntu:22.04

RUN apt update \
    && apt install -y python3 clamav \
    && apt clean \
    && rm -rf /var/lib/apt/lists/*

# Grab definitions
RUN freshclam
 
COPY . /clamav
WORKDIR "/clamav"

ENTRYPOINT ["python3", "/clamav/run_clamav.py"]