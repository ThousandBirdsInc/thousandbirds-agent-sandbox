ARG BASE_IMAGE=tb-agent-sandbox:debian-bookworm
FROM ${BASE_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        golang-go \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
