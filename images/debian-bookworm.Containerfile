FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV container=oci

# Reject images whose system trust store predates the October 2026 eMudhra fix.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        build-essential \
        ca-certificates \
        clang \
        curl \
        dbus \
        fd-find \
        git \
        gnupg \
        iproute2 \
        jq \
        kmod \
        less \
        lld \
        nodejs \
        npm \
        openssh-client \
        pkg-config \
        procps \
        python3 \
        python3-pip \
        python3-venv \
        ripgrep \
        sudo \
        systemd \
        systemd-sysv \
        tar \
        udhcpc \
        udev \
        unzip \
        vim-tiny \
        wget \
        xz-utils \
    && dpkg --compare-versions "$(dpkg-query -W -f='${Version}' ca-certificates)" ge 20250419~deb12u2 \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash agent \
    && mkdir -p /workspace \
    && chown -R agent:agent /workspace \
    && printf 'agent ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/agent \
    && chmod 0440 /etc/sudoers.d/agent \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd

COPY guest/agent-sandbox-login-tty.c /tmp/agent-sandbox-login-tty.c
RUN cc -O2 -Wall -Wextra -o /usr/local/bin/agent-sandbox-login-tty /tmp/agent-sandbox-login-tty.c \
    && chmod 0755 /usr/local/bin/agent-sandbox-login-tty \
    && rm /tmp/agent-sandbox-login-tty.c

USER agent
ENV NPM_CONFIG_PREFIX=/home/agent/.npm-global
ENV PATH=/home/agent/.npm-global/bin:$PATH
RUN mkdir -p "$NPM_CONFIG_PREFIX" \
    && npm install -g @anthropic-ai/claude-code @openai/codex

USER root
ENV PATH=/home/agent/.npm-global/bin:$PATH

RUN cat > /etc/profile.d/agent-sandbox.sh <<'EOF'
export PATH=/home/agent/.npm-global/bin:$PATH
EOF

STOPSIGNAL SIGRTMIN+3
