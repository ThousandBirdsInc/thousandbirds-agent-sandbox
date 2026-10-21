FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV container=oci

# Reject images whose system trust store predates the October 2026 eMudhra fix.
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
    && dpkg --compare-versions "$(dpkg-query -W -f='${Version}' ca-certificates)" ge 20250419~deb12u2 \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

STOPSIGNAL SIGRTMIN+3
