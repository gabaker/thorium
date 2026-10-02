ARG IMAGE="python:3.11-slim"
FROM $IMAGE

# install volatility3
RUN pip3 install volatility3

# make sure our symbols directory is writable by all users
RUN chmod -R 777 /usr/local/lib/python3.*/dist-packages/volatility3 || true
RUN chmod -R 777 /usr/lib/python3.*/dist-packages/volatility3 || true

RUN apt update -y && apt install zip -y

# add our wrapper
COPY wrapper.sh /app/.

ENTRYPOINT ["bash", "/app/wrapper.sh"]
