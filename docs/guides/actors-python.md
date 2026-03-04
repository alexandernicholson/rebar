# Python Actor Patterns Guide

This guide explains the design decisions behind the Rebar Python client and demonstrates idiomatic patterns for building actors in Python. Where Erlang/BEAM uses processes, pattern matching, and OTP behaviors, Python achieves the same goals through abstract base classes, context managers, type hints, and structured exception handling.

---

## Table of Contents

1. [Why an Abstract Base Class](#1-why-an-abstract-base-class)
2. [Context Managers for Resource Cleanup](#2-context-managers-for-resource-cleanup)
3. [Bytes for Payloads](#3-bytes-for-payloads)
4. [Exception Hierarchy](#4-exception-hierarchy)
5. [Type Hints Throughout](#5-type-hints-throughout)
6. [Pattern: Ping-Pong Actors](#6-pattern-ping-pong-actors)
7. [Pattern: Named Services](#7-pattern-named-services)
8. [Pattern: Message Dispatch](#8-pattern-message-dispatch)

---

## 1. Why an Abstract Base Class

The `Actor` interface is defined as an ABC with a single abstract method:

```python
from abc import ABC, abstractmethod

class Actor(ABC):
    @abstractmethod
    def handle_message(self, ctx: Context, msg: bytes | None) -> None: ...
```

This follows a core Python design principle: **explicit is better than implicit**. Python's `abc` module is the standard way to define required interfaces. Using `ABC` and `@abstractmethod` means:

- **You cannot instantiate an Actor directly.** Python raises `TypeError` at construction time if `handle_message` is not implemented. This catches mistakes immediately, not when the first message arrives.
- **Any class can be an actor.** There is no metaclass magic, no registration step, and no framework inheritance chain. Subclass `Actor`, implement the one method, and you have a working actor.
- **IDE support is automatic.** Type checkers (mypy, pyright) and editors understand `@abstractmethod` natively. You get autocomplete for `ctx` methods and linting for missing implementations without any plugins.

In Erlang/OTP, a `gen_server` module implements multiple callbacks (`init/1`, `handle_call/3`, `handle_cast/2`, `handle_info/2`, `terminate/2`). The Python client collapses this into a single entry point because:

- **Startup is a message.** When a process starts, `handle_message` is called with `msg` set to `None`. This removes the need for a separate `__init__`-style lifecycle hook in the actor framework. The actor can register itself, initialize state, or do nothing -- it is the actor's choice.
- **Python has no pattern matching on message shape.** Python uses `isinstance`, string comparisons, or structured deserialization to dispatch on message type. The single `handle_message` method is the natural place for this dispatch logic.
- **Inheritance is Pythonic.** Go uses interfaces (structural typing). Rust uses traits. Python uses inheritance. ABC-based inheritance is the standard Python mechanism for defining a contract that subclasses must fulfill.

```python
# Minimal actor -- a class with one method.
class Pinger(Actor):
    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is not None:
            print(f"pinger got: {msg.decode()}")
```

---

## 2. Context Managers for Resource Cleanup

The `Runtime` class implements Python's context manager protocol (`__enter__` and `__exit__`). This is the Pythonic way to manage resources that require explicit cleanup -- files, database connections, network sockets, and in this case, a Rust-backed actor runtime holding a Tokio thread pool.

```python
with Runtime(node_id=1) as rt:
    pid = rt.spawn_actor(MyActor())
    rt.send(pid, b"hello")
# rt.close() is called automatically, even if an exception was raised
```

### Why This Matters

- **Exception safety.** If code inside the `with` block raises an exception, `__exit__` still runs. The runtime is always cleaned up. Without the context manager, a forgotten `close()` call after an exception would leak the Tokio runtime and all its threads.
- **No `defer` in Python.** Go has `defer` for cleanup. Python has `with`. They solve the same problem: ensuring cleanup code runs regardless of how control flow exits the block.
- **`__del__` is a safety net, not a strategy.** The `Runtime` class also implements `__del__`, which calls `close()` when the object is garbage collected. However, Python's garbage collector makes no guarantees about when `__del__` runs -- or whether it runs at all during interpreter shutdown. The `with` statement provides deterministic cleanup.

### Manual Close

If a context manager does not fit your control flow, call `close()` explicitly:

```python
rt = Runtime(node_id=1)
try:
    pid = rt.spawn_actor(MyActor())
    rt.send(pid, b"hello")
finally:
    rt.close()
```

`close()` is idempotent. Calling it twice does nothing.

---

## 3. Bytes for Payloads

All message payloads in the Python client use `bytes`, Python's native type for binary data:

```python
rt.send(pid, b"hello world")
```

### Why `bytes` and Not `str`

Rebar messages are arbitrary byte sequences at the wire level. The Python client does not impose any encoding or serialization format. Using `bytes` makes this explicit:

- **`bytes` is the truth.** The FFI layer passes raw byte pointers. Wrapping them in `str` would require choosing an encoding (UTF-8? Latin-1?) and handling decode errors. Using `bytes` avoids this entirely.
- **Encoding is the caller's choice.** To send a string, encode it: `b"hello"` or `"hello".encode("utf-8")`. To send JSON, use `json.dumps(obj).encode()`. To send Protocol Buffers, use `msg.SerializeToString()`. The actor framework does not care.
- **`bytes` is zero-copy friendly.** The `ctypes` layer can pass `bytes` directly to C functions as a `c_char_p` without any intermediate conversion.

### Receiving Messages

Inside `handle_message`, `msg` is `bytes | None`. Decode as needed:

```python
class TextActor(Actor):
    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            return
        text = msg.decode("utf-8")
        print(f"received text: {text}")
```

---

## 4. Exception Hierarchy

The Python client uses exceptions for error handling, following Python's convention of "ask forgiveness, not permission" (EAFP). All Rebar-specific exceptions inherit from a common base class:

```
Exception
  RebarError            -- base class, code + message
    SendError           -- message delivery failed (code -2)
    NotFoundError       -- name not in registry (code -3)
    InvalidNameError    -- name not valid UTF-8 (code -4)
```

### Catching Specific Errors

```python
try:
    rt.send_named("worker", b"task")
except NotFoundError:
    print("worker not registered")
except SendError:
    print("worker unreachable")
```

### Catching All Rebar Errors

```python
try:
    rt.send_named("worker", b"task")
except RebarError as e:
    print(f"rebar operation failed: {e}")
```

### Catching Multiple Error Types

```python
try:
    rt.send_named("worker", b"task")
except (NotFoundError, SendError) as e:
    print(f"could not reach worker: {e}")
```

### Why Exceptions and Not Return Codes

Go returns `(value, error)` tuples because Go does not have exceptions. Python does have exceptions, and the community convention is to use them:

- **`try`/`except` is idiomatic Python.** Every Python developer knows this pattern. Returning error codes would force callers to check return values manually, which is error-prone and unidiomatic.
- **Exceptions propagate automatically.** If an actor does not handle a `SendError`, it propagates up to the caller of `spawn_actor` or to the runtime. There is no silent swallowing of errors.
- **The error code is still accessible.** Every `RebarError` carries the numeric FFI error code in its `code` attribute, if you need it for logging or programmatic inspection.

### Inside `handle_message`

Since `handle_message` returns `None`, actors handle errors inline:

```python
class Forwarder(Actor):
    def __init__(self, target: Pid):
        self.target = target

    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            return
        try:
            ctx.send(self.target, msg)
        except SendError:
            # Log, retry, drop -- the actor decides.
            print(f"forward to {self.target} failed")
```

This is a deliberate design choice. In Erlang, a process that crashes is restarted by its supervisor. In the Python client, the actor is responsible for its own error handling. An unhandled exception inside `handle_message` would propagate through the `ctypes` callback boundary, which is undefined behavior. Always handle exceptions within `handle_message`.

---

## 5. Type Hints Throughout

The Python client uses type hints on every public method and class. This is a modern Python best practice (PEP 484, PEP 526) that provides significant benefits without any runtime cost:

```python
def send(self, dest: Pid, data: bytes) -> None: ...
def whereis(self, name: str) -> Pid: ...
def spawn_actor(self, actor: Actor) -> Pid: ...
def handle_message(self, ctx: Context, msg: bytes | None) -> None: ...
```

### What the Type Hints Tell You

- `msg: bytes | None` -- the startup message is `None`, all other messages are `bytes`. The union type makes this explicit in the signature.
- `-> Pid` -- `whereis` always returns a `Pid` on success. If the name is not found, it raises `NotFoundError` rather than returning `None`. This avoids the "billion-dollar mistake" of returning null.
- `actor: Actor` -- the parameter is typed as the abstract base class. Type checkers will reject a plain object that does not implement `handle_message`.

### `from __future__ import annotations`

The client uses `from __future__ import annotations` to enable postponed evaluation of annotations (PEP 563). This allows forward references (e.g., `Runtime` referencing itself in `__enter__`) without quoting the type names.

### `TYPE_CHECKING` Guard

Imports used only for type hints are guarded behind `if TYPE_CHECKING:` to avoid circular imports at runtime:

```python
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .runtime import Context
```

This is a standard Python pattern for breaking circular dependencies between modules that need to reference each other's types.

---

## 6. Pattern: Ping-Pong Actors

Two actors exchanging messages through the registry demonstrate named messaging, startup initialization, and cross-actor communication.

```python
import time

from rebar import Actor, Context, Runtime, SendError


class Pinger(Actor):
    """Sends a 'ping' to the ponger on startup."""

    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            # Startup: register ourselves so Ponger can reply.
            ctx.register("pinger", ctx.self_pid())
            # Send the first ping.
            try:
                ctx.send_named("ponger", b"ping")
            except Exception as e:
                print(f"pinger: failed to send: {e}")
            return
        print(f"pinger received: {msg.decode()}")


class Ponger(Actor):
    """Waits for a 'ping' and replies with 'pong'."""

    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            # Startup: register ourselves so Pinger can find us.
            ctx.register("ponger", ctx.self_pid())
            return
        print(f"ponger received: {msg.decode()}")
        # Reply to the pinger.
        try:
            ctx.send_named("pinger", b"pong")
        except Exception as e:
            print(f"ponger: failed to reply: {e}")


def main():
    with Runtime(node_id=1) as rt:
        # Spawn ponger first so its name is registered before pinger starts.
        rt.spawn_actor(Ponger())
        rt.spawn_actor(Pinger())

        # Let the async processes run.
        time.sleep(0.1)


if __name__ == "__main__":
    main()
```

**Key points:**

- Ponger is spawned first so that its name is registered before Pinger tries to send.
- Both actors register themselves during the startup call (`msg is None`).
- Communication uses `send_named` so actors do not need to know each other's PIDs directly.
- Error handling uses `try`/`except` -- failed sends are caught and logged, not ignored.

---

## 7. Pattern: Named Services

Named services are actors registered under well-known names that other parts of the system can discover via `whereis` or send to via `send_named`. This is analogous to Erlang's registered processes.

```python
import time

from rebar import Actor, Context, NotFoundError, Runtime


class KVStore(Actor):
    """A simple in-memory key-value store actor."""

    def __init__(self):
        self.data: dict[str, str] = {}

    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            ctx.register("kv", ctx.self_pid())
            print("kv store ready")
            return
        payload = msg.decode()
        print(f"kv store received: {payload}")
        # In a real implementation, parse the payload as a command
        # (e.g., "SET key value" or "GET key") and respond accordingly.


def main():
    with Runtime(node_id=1) as rt:
        # Start the service.
        rt.spawn_actor(KVStore())

        # Any part of the system can now discover the service.
        try:
            pid = rt.whereis("kv")
            print(f"kv store is at {pid}")
        except NotFoundError:
            print("kv not found")
            return

        # Send a command by name -- no PID needed.
        rt.send_named("kv", b"SET greeting hello")

        time.sleep(0.1)


if __name__ == "__main__":
    main()
```

**Key points:**

- The service registers itself during startup. Callers use `whereis` or `send_named` to interact with it.
- State (the `data` dictionary) lives in the actor instance. It is initialized in `__init__`, which runs before the actor is spawned. The startup handler (`msg is None`) is used for registration, not for state initialization. This separates Python-level construction from Rebar-level lifecycle.
- `send_named` avoids the need to pass PIDs around, making the system loosely coupled.

---

## 8. Pattern: Message Dispatch

Since Python does not have Erlang-style pattern matching on message shapes, actors that handle multiple message types need an explicit dispatch mechanism. The simplest approach is a prefix byte or tag:

```python
class Router(Actor):
    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            ctx.register("router", ctx.self_pid())
            return
        if len(msg) == 0:
            return

        # First byte is the message type tag.
        tag = msg[0]
        payload = msg[1:]

        if tag == 0x01:
            self._handle_create(ctx, payload)
        elif tag == 0x02:
            self._handle_update(ctx, payload)
        elif tag == 0x03:
            self._handle_delete(ctx, payload)
        else:
            print(f"unknown message type: 0x{tag:02x}")

    def _handle_create(self, ctx: Context, data: bytes) -> None:
        print(f"create: {data.decode()}")

    def _handle_update(self, ctx: Context, data: bytes) -> None:
        print(f"update: {data.decode()}")

    def _handle_delete(self, ctx: Context, data: bytes) -> None:
        print(f"delete: {data.decode()}")
```

For richer message formats, consider encoding messages with JSON. The actor's `handle_message` deserializes the payload and dispatches based on the decoded type:

```python
import json


class Command:
    def __init__(self, action: str, key: str, value: str = ""):
        self.action = action
        self.key = key
        self.value = value


class JSONActor(Actor):
    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            return
        try:
            data = json.loads(msg)
        except json.JSONDecodeError:
            print(f"invalid JSON: {msg!r}")
            return

        action = data.get("action")
        key = data.get("key", "")
        value = data.get("value", "")

        if action == "get":
            print(f"get {key}")
        elif action == "set":
            print(f"set {key} = {value}")
        else:
            print(f"unknown action: {action}")
```

For structured binary protocols, Python's `struct` module or libraries like `msgpack` or `protobuf` work well:

```python
import struct


class BinaryActor(Actor):
    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            return
        if len(msg) < 4:
            return
        # First 4 bytes: big-endian unsigned int (message type)
        msg_type = struct.unpack(">I", msg[:4])[0]
        payload = msg[4:]

        if msg_type == 1:
            print(f"type 1: {payload.decode()}")
        elif msg_type == 2:
            print(f"type 2: {payload!r}")
```

This explicit dispatch is more verbose than Erlang's pattern matching, but it is straightforward, debuggable, and uses familiar Python patterns.

---

## See Also

- [Python Client API Reference](../api/client-python.md) -- complete class and method documentation
- [rebar-ffi C-ABI Reference](../api/rebar-ffi.md) -- the underlying C API
- [Extending Rebar](../extending.md) -- building FFI bindings for new languages
