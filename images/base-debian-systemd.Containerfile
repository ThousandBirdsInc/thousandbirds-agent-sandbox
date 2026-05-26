FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV container=oci

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        dbus \
        iproute2 \
        kmod \
        procps \
        systemd \
        systemd-sysv \
        tar \
        udhcpc \
        udev \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

STOPSIGNAL SIGRTMIN+3
