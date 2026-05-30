"""Adapter for Daytona sandboxes via the `daytona` CLI.

Daytona is an API-backed sandbox platform; the `daytona` CLI talks to a control
plane (Daytona Cloud or a self-hosted server) over an API key. Because Daytona's
CLI surface has shifted across releases and the remote control plane is not
present on this host, this adapter is built on the env-configurable template
provider with Daytona-shaped defaults. Confirm the templates against your
installed `daytona version` before trusting the numbers.

Required to run (set in env, or export DAYTONA_API_KEY and adjust templates):
  DAYTONA_BIN        default: daytona
  DAYTONA_LAUNCH     create a sandbox and print its id
  DAYTONA_EXEC       run {cmd} in sandbox {name}
  DAYTONA_DESTROY    delete sandbox {name}
Optional: DAYTONA_SNAPSHOT, DAYTONA_RESTORE, DAYTONA_RSS, DAYTONA_IMAGE.

Defaults below reflect a plausible `daytona sandbox` surface; they are a
starting point, not a guarantee.
"""

from .template import CommandTemplateProvider

_DEFAULTS = {
    "BIN": "daytona",
    "IMAGE": "daytonaio/sandbox:latest",
    # `--target` / id parsing differ by version; LAUNCH_ID_RE pulls an id-looking
    # token from stdout so persistent exec can address the sandbox.
    "LAUNCH": "daytona sandbox create --image {image}",
    "LAUNCH_ID_RE": r"([0-9a-f]{8,}|sandbox-[\w-]+)",
    "EXEC": "daytona sandbox exec {name} -- {cmd}",
    "DESTROY": "daytona sandbox delete {name} --yes",
    # Daytona supports snapshots via the platform; templates left to the operator
    # because the CLI verb/flags vary by release.
    "SNAPSHOT": None,
    "RESTORE": None,
    "RSS": None,
}


def DaytonaProvider():
    return CommandTemplateProvider("daytona", "DAYTONA", kind="container", defaults=_DEFAULTS)
