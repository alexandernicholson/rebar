# Python Client -- API Reference

The `rebar` package provides an idiomatic Python interface to the Rebar actor runtime. It wraps the `librebar_ffi` C shared library via `ctypes`, translating C-level error codes and opaque pointers into Python classes, abstract base classes, and exceptions.

The package is organized around four concepts: a **Runtime** that manages the actor system, **Pid** values that identify processes, an **Actor** ABC that defines process behavior, and a **Context** that provides messaging capabilities inside actor callbacks.

---

## Types

### Pid

A process identifier composed of two integer fields. Implemented as a frozen dataclass.

```python
@dataclass(frozen=True)
class Pid:
    node_id: int
    local_id: int
```

- `node_id` identifies the runtime node.
- `local_id` identifies the process within that node.

`Pid` is immutable. It is safe to use as a dictionary key, store in sets, and pass between functions.

#### `__str__`

Returns a human-readable representation in the format `<node_id.local_id>`.

```python
def __str__(self) -> str
```

**Returns:** A string like `"<1.42>"`.

**Example:**

```python
pid = Pid(node_id=1, local_id=42)
print(pid)  # <1.42>
```

---

### Actor

The abstract base class that all Rebar actors must implement. It contains a single abstract method, following the Python convention of using ABC and `@abstractmethod` to define required interfaces.

```python
class Actor(ABC):
    @abstractmethod
    def handle_message(self, ctx: Context, msg: bytes | None) -> None: ...
```

- `handle_message` is called when the process starts (with `msg` set to `None` as a lifecycle hook) and each time the process receives a message.
- The `ctx` parameter provides the process's identity and messaging capabilities.
- Implementations should not retain references to `ctx` beyond the lifetime of the call.

**Example:**

```python
class Logger(Actor):
    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            print(f"logger started as {ctx.self_pid()}")
            return
        print(f"log: {msg.decode()}")
```

---

### Context

Passed to `Actor.handle_message` to provide the process's identity and access to the runtime's messaging and registry operations. Context methods mirror the corresponding `Runtime` methods.

```python
class Context:
    def __init__(self, pid: Pid, runtime: Runtime): ...
```

---

#### `self_pid`

Returns this process's PID.

```python
def self_pid(self) -> Pid
```

**Returns:** The `Pid` of the current process.

---

#### `send`

Sends a message to another process by PID.

```python
def send(self, dest: Pid, data: bytes) -> None
```

**Parameters:**

| Name   | Type    | Description                    |
|--------|---------|--------------------------------|
| `dest` | `Pid`   | PID of the destination process |
| `data` | `bytes` | Payload bytes to send          |

**Raises:**
- `SendError` if the destination process does not exist or its mailbox is closed.

---

#### `register`

Associates a name with a PID in the runtime's registry.

```python
def register(self, name: str, pid: Pid) -> None
```

**Parameters:**

| Name   | Type  | Description                    |
|--------|-------|--------------------------------|
| `name` | `str` | Name to register               |
| `pid`  | `Pid` | PID to associate with the name |

**Raises:**
- `InvalidNameError` if the name is not valid UTF-8 (unlikely in Python since `str` is always Unicode, but enforced at the FFI boundary).

**Notes:**
- If the name is already registered, the PID is silently overwritten.

---

#### `whereis`

Looks up a PID by its registered name.

```python
def whereis(self, name: str) -> Pid
```

**Parameters:**

| Name   | Type  | Description    |
|--------|-------|----------------|
| `name` | `str` | Name to look up |

**Returns:** The associated `Pid`.

**Raises:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.

---

#### `send_named`

Sends a message to a process by its registered name. Combines a registry lookup and a send in a single call.

```python
def send_named(self, name: str, data: bytes) -> None
```

**Parameters:**

| Name   | Type    | Description                   |
|--------|---------|-------------------------------|
| `name` | `str`   | Registered name of the target |
| `data` | `bytes` | Payload bytes to send         |

**Raises:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.
- `SendError` if the resolved process is unreachable.

---

## Runtime

The central class that manages the Rebar actor system. A `Runtime` wraps an opaque C pointer to the `rebar_runtime_t` structure, which contains a Tokio async runtime, the Rebar process scheduler, and a local name registry.

`Runtime` implements the context manager protocol (`__enter__` / `__exit__`), making it safe to use with Python's `with` statement for automatic resource cleanup.

```python
class Runtime:
    def __init__(self, node_id: int = 1): ...
```

---

### Constructor

Creates a new Rebar runtime for the given node ID.

```python
Runtime(node_id: int = 1)
```

**Parameters:**

| Name      | Type  | Default | Description                              |
|-----------|-------|---------|------------------------------------------|
| `node_id` | `int` | `1`     | Unique identifier for this runtime node  |

**Raises:** `RuntimeError` if the internal Tokio runtime fails to initialize.

**Notes:**
- The caller must call `close()` when the runtime is no longer needed, or use the `with` statement.
- Multiple runtimes can coexist with different `node_id` values.
- Each runtime maintains its own independent name registry.

**Example:**

```python
with Runtime(node_id=1) as rt:
    # use the runtime
    pass
# runtime is automatically freed here
```

---

### `close`

Frees the runtime and stops all spawned processes. Safe to call multiple times -- subsequent calls are no-ops.

```python
def close(self) -> None
```

**Notes:**
- All spawned processes are stopped when the runtime is freed.
- The name registry is destroyed along with the runtime.
- Also called automatically by `__exit__` and `__del__`.

---

### `__enter__` / `__exit__`

Context manager protocol. `__enter__` returns the runtime itself. `__exit__` calls `close()`.

```python
def __enter__(self) -> Runtime
def __exit__(self, *args: object) -> None
```

**Example:**

```python
with Runtime(node_id=1) as rt:
    pid = rt.spawn_actor(MyActor())
    rt.send(pid, b"hello")
# rt.close() is called automatically
```

---

### `send`

Sends a message to a process identified by PID.

```python
def send(self, dest: Pid, data: bytes) -> None
```

**Parameters:**

| Name   | Type    | Description                    |
|--------|---------|--------------------------------|
| `dest` | `Pid`   | PID of the destination process |
| `data` | `bytes` | Payload bytes to send          |

**Raises:**
- `SendError` if the destination process does not exist or its mailbox is closed.

**Notes:**
- The data is copied into Rust-owned memory. The caller retains ownership of the bytes object.
- The underlying C message is created and freed within the same call.

**Example:**

```python
try:
    rt.send(target_pid, b"ping")
except SendError:
    print("process unreachable")
```

---

### `register`

Associates a name with a PID in the runtime's local name registry.

```python
def register(self, name: str, pid: Pid) -> None
```

**Parameters:**

| Name   | Type  | Description                    |
|--------|-------|--------------------------------|
| `name` | `str` | Name to register               |
| `pid`  | `Pid` | PID to associate with the name |

**Raises:**
- `InvalidNameError` if the name bytes are not valid UTF-8.

**Notes:**
- If the name is already registered, the PID is silently overwritten.
- The name string is encoded to UTF-8 bytes for the FFI call.

**Example:**

```python
pid = rt.spawn_actor(MyActor())
rt.register("my_service", pid)
```

---

### `whereis`

Looks up a PID by its registered name.

```python
def whereis(self, name: str) -> Pid
```

**Parameters:**

| Name   | Type  | Description    |
|--------|-------|----------------|
| `name` | `str` | Name to look up |

**Returns:** The associated `Pid`.

**Raises:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.

**Example:**

```python
try:
    pid = rt.whereis("my_service")
    print(f"service is at {pid}")
except NotFoundError:
    print("service not registered yet")
```

---

### `send_named`

Sends a message to a process by its registered name. Combines a registry lookup and a send in a single call.

```python
def send_named(self, name: str, data: bytes) -> None
```

**Parameters:**

| Name   | Type    | Description                   |
|--------|---------|-------------------------------|
| `name` | `str`   | Registered name of the target |
| `data` | `bytes` | Payload bytes to send         |

**Raises:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.
- `SendError` if the resolved process is unreachable.

**Notes:**
- The registry lock is held only for the lookup; the send happens after the lock is released.
- The caller retains ownership of `data`.

**Example:**

```python
try:
    rt.send_named("worker", b"work-item")
except (NotFoundError, SendError) as e:
    print(f"failed to send: {e}")
```

---

### `spawn_actor`

Spawns a new process backed by the given `Actor`. The actor's `handle_message` method is called with `msg=None` on startup as a lifecycle hook, and the process PID is returned.

```python
def spawn_actor(self, actor: Actor) -> Pid
```

**Parameters:**

| Name    | Type    | Description                          |
|---------|---------|--------------------------------------|
| `actor` | `Actor` | Actor implementation for the process |

**Returns:** The `Pid` of the newly spawned process.

**Notes:**
- The runtime keeps a reference to the callback to prevent garbage collection.
- The startup call (`msg is None`) executes synchronously during spawn.

**Example:**

```python
class Counter(Actor):
    def __init__(self):
        self.count = 0

    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            ctx.register("counter", ctx.self_pid())
            return
        self.count += 1
        print(f"count: {self.count}")

pid = rt.spawn_actor(Counter())
print(f"spawned counter at {pid}")
```

---

## Errors

All errors raised by the Python client are exception classes that inherit from `RebarError`, which itself inherits from Python's built-in `Exception`. They can be caught individually or as a group via the base class.

### RebarError

The base exception for all Rebar FFI errors.

```python
class RebarError(Exception):
    def __init__(self, code: int, message: str): ...
```

- `code` is the integer error code from the FFI layer.
- The string representation includes both the code and a human-readable message.

---

### SendError

Raised when a message cannot be delivered to a process. Inherits from `RebarError`.

```python
class SendError(RebarError):
    def __init__(self): ...
```

- Raised by `send`, `send_named`, `Context.send`, and `Context.send_named`.
- Typically means the destination process does not exist or its mailbox is closed.
- FFI error code: `-2` (`REBAR_ERR_SEND_FAILED`).

**Example:**

```python
try:
    rt.send(pid, b"hello")
except SendError:
    print("message delivery failed")
```

---

### NotFoundError

Raised when a name is not found in the registry. Inherits from `RebarError`.

```python
class NotFoundError(RebarError):
    def __init__(self): ...
```

- Raised by `whereis`, `send_named`, `Context.whereis`, and `Context.send_named`.
- FFI error code: `-3` (`REBAR_ERR_NOT_FOUND`).

**Example:**

```python
try:
    rt.whereis("nonexistent")
except NotFoundError:
    print("name not registered")
```

---

### InvalidNameError

Raised when a name passed to a registry function is not valid UTF-8. Inherits from `RebarError`.

```python
class InvalidNameError(RebarError):
    def __init__(self): ...
```

- Raised by `register`, `whereis`, `send_named`, and their `Context` equivalents.
- In practice this is rare in Python since `str` is always Unicode, but the FFI layer enforces the constraint.
- FFI error code: `-4` (`REBAR_ERR_INVALID_NAME`).

---

### `check_error`

Internal helper that translates FFI return codes into Python exceptions. Not part of the public API, but documented here for completeness.

```python
def check_error(rc: int) -> None
```

| Return Code | Exception          | Meaning                            |
|-------------|--------------------|------------------------------------|
| `0`         | *(none)*           | Success                            |
| `-1`        | `RuntimeError`     | Null pointer (internal bug)        |
| `-2`        | `SendError`        | Message delivery failed            |
| `-3`        | `NotFoundError`    | Name not in registry               |
| `-4`        | `InvalidNameError` | Name is not valid UTF-8            |
| other       | `RebarError`       | Unknown error                      |

**Notes:**
- A return code of `-1` (null pointer) raises a plain `RuntimeError` rather than a `RebarError`, since it indicates a bug in the bindings rather than a user error.

---

## Library Discovery

The Python client uses `ctypes.CDLL` to load the `librebar_ffi` shared library. The search order is:

1. **`REBAR_LIB_PATH` environment variable.** If set and the file exists, this path is used directly.
2. **System library search.** `ctypes.util.find_library("rebar_ffi")` checks `LD_LIBRARY_PATH` (Linux), `DYLD_LIBRARY_PATH` (macOS), and standard system library directories.
3. **Repository-relative fallback.** The client looks for `target/release/librebar_ffi.so` (or `.dylib` / `.dll`) relative to the repository root. This works when running from within the Rebar source tree.

If none of these succeed, an `OSError` is raised with instructions for building the library.

### Platform-Specific Library Names

| Platform | Library File          |
|----------|-----------------------|
| Linux    | `librebar_ffi.so`     |
| macOS    | `librebar_ffi.dylib`  |
| Windows  | `rebar_ffi.dll`       |

---

## Memory Management

### Runtime Lifecycle

The caller who creates a `Runtime` owns it and must free it with `close()`. The idiomatic pattern uses a context manager:

```python
with Runtime(node_id=1) as rt:
    pid = rt.spawn_actor(MyActor())
    rt.send(pid, b"hello")
# runtime is freed here
```

`close` is idempotent -- calling it multiple times is safe. The runtime also implements `__del__` as a safety net, but relying on `__del__` for cleanup is not recommended.

### Messages

Messages are created from Python `bytes` objects. The `ctypes` layer copies the data into a C-side message, sends it, and frees the C message within the same method call (via a `try`/`finally` block). The caller's `bytes` object is never modified.

### Actors

When you call `spawn_actor`, the runtime stores a reference to the `ctypes` callback wrapper in an internal list. This prevents the garbage collector from collecting the callback while the runtime is alive. The actor object itself is captured by the callback closure and lives as long as the callback does.

### Thread Safety

The underlying C runtime is internally synchronized -- it uses a Tokio runtime (thread-safe) and the name registry is protected by a mutex. However, the Python `ctypes` bindings themselves are subject to the GIL. In practice, all calls to the FFI layer are serialized by the GIL, which prevents data races on the Python side.

---

## Complete Example

```python
import time

from rebar import Actor, Context, NotFoundError, Pid, Runtime, SendError


class Greeter(Actor):
    """An actor that prints a greeting when it receives a message."""

    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        if msg is None:
            # Startup: register ourselves under a well-known name.
            ctx.register("greeter", ctx.self_pid())
            print(f"greeter started at {ctx.self_pid()}")
            return
        print(f"greeter received: {msg.decode()}")


def main():
    # Create a runtime on node 1.
    with Runtime(node_id=1) as rt:
        # Spawn the greeter actor.
        pid = rt.spawn_actor(Greeter())

        # Send a message by PID.
        try:
            rt.send(pid, b"hello by PID")
        except SendError as e:
            print(f"send failed: {e}")

        # Send a message by name.
        try:
            rt.send_named("greeter", b"hello by name")
        except (NotFoundError, SendError) as e:
            print(f"send named failed: {e}")

        # Look up by name.
        try:
            found = rt.whereis("greeter")
            print(f"greeter is at {found}")
        except NotFoundError:
            print("greeter not registered")

        # Give async processes a moment to execute.
        time.sleep(0.1)


if __name__ == "__main__":
    main()
```

---

## See Also

- [rebar-ffi C-ABI Reference](rebar-ffi.md) -- the underlying C API that this package wraps
- [Python Actor Patterns Guide](../guides/actors-python.md) -- idiomatic patterns for building actors in Python
- [Architecture](../architecture.md) -- how the FFI layer fits into the overall crate structure
