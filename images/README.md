# Agent Sandbox Images

Images are built on demand. The wrapper does not install every language
toolchain into the default sandbox.

Default variant:

```sh
scripts/build-agent-image.sh debian-bookworm
```

Optional variants:

```sh
scripts/build-agent-image.sh debian-bookworm-rust
scripts/build-agent-image.sh debian-bookworm-go
scripts/build-agent-image.sh ubuntu-noble
```

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
