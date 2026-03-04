from abc import ABC, abstractmethod
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from .runtime import Context


class Actor(ABC):
    """Base class for Rebar actors. Subclass and implement handle_message."""

    @abstractmethod
    def handle_message(self, ctx: "Context", msg: Optional[bytes]) -> None:
        """Called when this actor receives a message.

        Args:
            ctx: The process context, providing self_pid(), send(), etc.
            msg: The message payload as bytes, or None on startup.
        """
        ...
