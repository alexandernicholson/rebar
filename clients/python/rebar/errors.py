"""Error types for the Rebar Python client."""


class RebarError(Exception):
    """Base error for all Rebar operations."""

    def __init__(self, code: int, message: str):
        self.code = code
        super().__init__(f"rebar error {code}: {message}")


class SendError(RebarError):
    """Raised when a message cannot be delivered."""

    def __init__(self):
        super().__init__(-2, "failed to deliver message")


class NotFoundError(RebarError):
    """Raised when a name is not found in the registry."""

    def __init__(self):
        super().__init__(-3, "name not found in registry")


class InvalidNameError(RebarError):
    """Raised when a name is not valid UTF-8."""

    def __init__(self):
        super().__init__(-4, "name is not valid UTF-8")


def check_error(rc: int) -> None:
    """Raise an appropriate exception for non-zero FFI return codes."""
    if rc == 0:
        return
    if rc == -1:
        raise RuntimeError("rebar: internal error — null pointer passed to FFI")
    if rc == -2:
        raise SendError()
    if rc == -3:
        raise NotFoundError()
    if rc == -4:
        raise InvalidNameError()
    raise RebarError(rc, "unknown error")
