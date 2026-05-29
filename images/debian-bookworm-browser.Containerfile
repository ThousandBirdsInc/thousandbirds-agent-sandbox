ARG BASE_IMAGE=tb-agent-sandbox:debian-bookworm
FROM ${BASE_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive

# Browser + fonts are the headless-mode baseline. Xvfb + x11vnc + fluxbox add
# the display stack used by `agent-sandbox-browser start --headed` (and the
# `tb-sandbox browser view` host-side VNC viewer). Fluxbox is the smallest
# window manager that gives Chromium a sane parent so it can render full-
# screen; without one, the Chromium window has no resize/focus handling.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        chromium \
        fonts-liberation \
        fonts-noto-color-emoji \
        xvfb \
        x11vnc \
        fluxbox \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
