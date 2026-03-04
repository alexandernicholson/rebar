"""Runtime and Context for the Rebar Python client."""

from __future__ import annotations

import ctypes
from typing import TYPE_CHECKING

from . import _ffi
from .errors import check_error
from .types import Pid

if TYPE_CHECKING:
    from .actor import Actor


class Context:
    """Passed to Actor.handle_message. Provides messaging capabilities."""

    def __init__(self, pid: Pid, runtime: Runtime):
        self._pid = pid
        self._runtime = runtime

    def self_pid(self) -> Pid:
        """Return this process's PID."""
        return self._pid

    def send(self, dest: Pid, data: bytes) -> None:
        """Send a message to another process."""
        self._runtime.send(dest, data)

    def register(self, name: str, pid: Pid) -> None:
        """Register a name for a PID."""
        self._runtime.register(name, pid)

    def whereis(self, name: str) -> Pid:
        """Look up a PID by name."""
        return self._runtime.whereis(name)

    def send_named(self, name: str, data: bytes) -> None:
        """Send a message to a named process."""
        self._runtime.send_named(name, data)


class Runtime:
    """Manages a Rebar actor runtime. Use as a context manager.

    Example::

        with Runtime(node_id=1) as rt:
            pid = rt.spawn_actor(MyActor())
            rt.send(pid, b"hello")
    """

    def __init__(self, node_id: int = 1):
        self._ptr = _ffi._lib.rebar_runtime_new(node_id)
        if not self._ptr:
            raise RuntimeError("failed to create Rebar runtime")
        # Keep references to callbacks to prevent GC
        self._callbacks: list[_ffi.PROCESS_CALLBACK] = []

    def close(self) -> None:
        """Free the runtime. Safe to call multiple times."""
        if self._ptr:
            _ffi._lib.rebar_runtime_free(self._ptr)
            self._ptr = None
            self._callbacks.clear()

    def __enter__(self) -> Runtime:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()

    def __del__(self) -> None:
        self.close()

    def send(self, dest: Pid, data: bytes) -> None:
        """Send a message to a process by PID."""
        msg = _ffi._lib.rebar_msg_create(data, len(data))
        try:
            rc = _ffi._lib.rebar_send(self._ptr, _ffi.RebarPid(dest.node_id, dest.local_id), msg)
            check_error(rc)
        finally:
            _ffi._lib.rebar_msg_free(msg)

    def register(self, name: str, pid: Pid) -> None:
        """Register a name for a PID."""
        name_bytes = name.encode("utf-8")
        rc = _ffi._lib.rebar_register(
            self._ptr, name_bytes, len(name_bytes),
            _ffi.RebarPid(pid.node_id, pid.local_id),
        )
        check_error(rc)

    def whereis(self, name: str) -> Pid:
        """Look up a PID by name."""
        name_bytes = name.encode("utf-8")
        pid_out = _ffi.RebarPid()
        rc = _ffi._lib.rebar_whereis(
            self._ptr, name_bytes, len(name_bytes), ctypes.byref(pid_out),
        )
        check_error(rc)
        return Pid(node_id=pid_out.node_id, local_id=pid_out.local_id)

    def send_named(self, name: str, data: bytes) -> None:
        """Send a message to a named process."""
        name_bytes = name.encode("utf-8")
        msg = _ffi._lib.rebar_msg_create(data, len(data))
        try:
            rc = _ffi._lib.rebar_send_named(
                self._ptr, name_bytes, len(name_bytes), msg,
            )
            check_error(rc)
        finally:
            _ffi._lib.rebar_msg_free(msg)

    def spawn_actor(self, actor: Actor) -> Pid:
        """Spawn a new process backed by the given Actor.

        The actor's handle_message is called with a None message on startup.
        """
        pid_out = _ffi.RebarPid()
        runtime_ref = self

        @_ffi.PROCESS_CALLBACK
        def callback(ffi_pid: _ffi.RebarPid) -> None:
            pid = Pid(node_id=ffi_pid.node_id, local_id=ffi_pid.local_id)
            ctx = Context(pid, runtime_ref)
            actor.handle_message(ctx, None)

        # Keep reference to prevent GC of the callback
        self._callbacks.append(callback)

        rc = _ffi._lib.rebar_spawn(self._ptr, callback, ctypes.byref(pid_out))
        check_error(rc)
        return Pid(node_id=pid_out.node_id, local_id=pid_out.local_id)
