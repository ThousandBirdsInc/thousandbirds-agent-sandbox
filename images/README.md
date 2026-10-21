# Agent Sandbox Images

Images are built on demand. The wrapper does not install every language
toolchain into the default sandbox.

Default variant:

```sh
scripts/build-agent-image.sh debian-bookworm
```

Optional variants:

```sh
scripts/build-agent-image.sh debian-bookworm-browser
scripts/build-agent-image.sh debian-bookworm-rust
scripts/build-agent-image.sh debian-bookworm-go
scripts/build-agent-image.sh ubuntu-noble
```

The `debian-bookworm-browser` variant bakes Chromium into the libkrun image so
browser sessions do not reinstall it per sandbox.

The script builds an OCI image with Docker or Podman, overlays the guest shell
scripts from `guest/`, then uses `../thousandbirds-libkrun/examples/os_mode_build_container_rootfs.py`
to produce a libkrun OS-mode bundle under:

```text
target/agent-images/<variant>/
```

Use the resulting bundle with:

```sh
cargo run -- shell --image target/agent-images/debian-bookworm --workspace .
```

## October 2026 eMudhra certificate remediation

The host proxy and guest applications validate TLS separately. Rebuild the host
with the updated `Cargo.lock` (`rustls-webpki` 0.103.16 or newer) so its upstream
TLS verifier rejects the compromised intermediate public keys. This does **not**
protect HTTPS that the proxy only tunnels, or clients inside an existing guest.

The Debian base/agent and Ubuntu image recipes now require a system
`ca-certificates` package containing the distribution's incident fix:

- Debian Bookworm: `20250419~deb12u2` or newer.
- Ubuntu Noble: `20260816ubuntu0.24.04.1` or newer.

The version checks fail the image build if a mirror has not shipped the update.
They also invalidate the old package-install build layer. Do not bypass a
failure by removing the check or disabling TLS verification. See the
[Debian security advisory](https://lists.debian.org/debian-lts-announce/2026/10/msg00039.html)
and [Ubuntu incident advisory](https://ubuntu.com/blog/emudhra-ca-compromise-fixes-available).

Rebuild each image variant you distribute from this revision, pulling its base:

```sh
PULL=1 scripts/build-agent-image.sh debian-bookworm
PULL=1 scripts/build-agent-image.sh debian-bookworm-browser
PULL=1 scripts/build-agent-image.sh ubuntu-noble
```

The Rust/Go variants also rebuild their Debian parent. Refresh any separately
built base bundles with `PULL=1 scripts/build-base-bundle.sh`. Replace deployed
bundles and recreate guests from them: rebuilding an image does not update a
running VM, persistent guest root filesystem, or already exported bundle.

Check the package version inside the rebuilt image with
`dpkg-query -W ca-certificates`. A system-store update is not a universal client
fix: bundled CA stores, Chromium's root/revocation data, Python `certifi`, and
statically linked Rust/Go tools can require their own updates or rebuilds.
Check those clients independently before treating an image as remediated. In
particular, do not assume a Chromium package upgrade alone proves that its
revocation data is current; see the
[Chrome incident response](https://blog.google/security/protecting-chrome-and-android-users-from-compromised-emudhra-intermediate-ca-certificates/).
