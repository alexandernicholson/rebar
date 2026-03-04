"""Core types for the Rebar Python client."""

from dataclasses import dataclass


@dataclass(frozen=True)
class Pid:
    """Identifies a process within a Rebar runtime."""

    node_id: int
    local_id: int

    def __str__(self) -> str:
        return f"<{self.node_id}.{self.local_id}>"
