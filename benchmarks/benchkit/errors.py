"""Exceptions used to signal provider capability and availability."""


class BenchError(Exception):
    """Base class for harness errors."""


class NotAvailable(BenchError):
    """The provider's tool is not installed or not usable on this host.

    Raised by ``Provider.available`` consumers; a provider that is not
    available is skipped entirely rather than reported as a failure.
    """


class Unsupported(BenchError):
    """The provider does not implement this operation (e.g. no snapshots).

    A scenario that hits an ``Unsupported`` operation records the cell as
    "n/a" rather than as an error.
    """


class OperationFailed(BenchError):
    """An operation the provider claims to support failed at runtime.

    Carries the command (if any) and captured output so failures are
    diagnosable from the JSON report.
    """

    def __init__(self, message, *, cmd=None, returncode=None, output=None):
        super().__init__(message)
        self.cmd = cmd
        self.returncode = returncode
        self.output = output
