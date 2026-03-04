# TypeScript Client -- API Reference

The `@rebar/client` package provides an idiomatic TypeScript interface to the Rebar actor runtime. It wraps the `librebar_ffi` C shared library via Deno's FFI API (`Deno.dlopen`), translating C-level error codes and opaque pointers into TypeScript classes, abstract classes, and thrown exceptions.

The package is organized around four concepts: a **Runtime** that manages the actor system, **Pid** values that identify processes, an **Actor** abstract class that defines process behavior, and a **Context** that provides messaging capabilities inside actor callbacks.

---

## Types

### Pid

A process identifier composed of two `bigint` fields. Deno's FFI layer requires `bigint` for 64-bit unsigned integers, so both fields use this type natively.

```typescript
class Pid {
  readonly nodeId: bigint;
  readonly localId: bigint;
  constructor(nodeId: bigint, localId: bigint);
}
```

- `nodeId` identifies the runtime node.
- `localId` identifies the process within that node.

`Pid` is immutable (both fields are `readonly`). It is safe to pass between functions and store in collections.

#### `toString`

Returns a human-readable representation in the format `<nodeId.localId>`.

```typescript
toString(): string
```

**Returns:** A string like `"<1.42>"`.

**Example:**

```typescript
const pid = new Pid(1n, 42n);
console.log(pid.toString());  // <1.42>
console.log(`${pid}`);        // <1.42>
```

---

### Actor

The abstract class that all Rebar actors must extend. It contains a single abstract method, following TypeScript's convention of using the `abstract` keyword to define required interfaces.

```typescript
abstract class Actor {
  abstract handleMessage(ctx: Context, msg: Uint8Array | null): void;
}
```

- `handleMessage` is called when the process starts (with `msg` set to `null` as a lifecycle hook) and each time the process receives a message.
- The `ctx` parameter provides the process's identity and messaging capabilities.
- Implementations should not retain references to `ctx` beyond the lifetime of the call.

**Example:**

```typescript
class Logger extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      console.log(`logger started as ${ctx.self()}`);
      return;
    }
    console.log(`log: ${new TextDecoder().decode(msg)}`);
  }
}
```

---

### Context

Passed to `Actor.handleMessage` to provide the process's identity and access to the runtime's messaging and registry operations. Context methods mirror the corresponding `Runtime` methods.

```typescript
class Context {
  constructor(pid: Pid, runtime: Runtime);
}
```

---

#### `self`

Returns this process's PID.

```typescript
self(): Pid
```

**Returns:** The `Pid` of the current process.

---

#### `send`

Sends a message to another process by PID.

```typescript
send(dest: Pid, data: Uint8Array): void
```

**Parameters:**

| Name   | Type         | Description                    |
|--------|--------------|--------------------------------|
| `dest` | `Pid`        | PID of the destination process |
| `data` | `Uint8Array` | Payload bytes to send          |

**Throws:**
- `SendError` if the destination process does not exist or its mailbox is closed.

---

#### `register`

Associates a name with a PID in the runtime's registry.

```typescript
register(name: string, pid: Pid): void
```

**Parameters:**

| Name   | Type     | Description                    |
|--------|----------|--------------------------------|
| `name` | `string` | Name to register               |
| `pid`  | `Pid`    | PID to associate with the name |

**Throws:**
- `InvalidNameError` if the name is not valid UTF-8 (unlikely in TypeScript since `string` is always Unicode, but enforced at the FFI boundary).

**Notes:**
- If the name is already registered, the PID is silently overwritten.

---

#### `whereis`

Looks up a PID by its registered name.

```typescript
whereis(name: string): Pid
```

**Parameters:**

| Name   | Type     | Description     |
|--------|----------|-----------------|
| `name` | `string` | Name to look up |

**Returns:** The associated `Pid`.

**Throws:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.

---

#### `sendNamed`

Sends a message to a process by its registered name. Combines a registry lookup and a send in a single call.

```typescript
sendNamed(name: string, data: Uint8Array): void
```

**Parameters:**

| Name   | Type         | Description                   |
|--------|--------------|-------------------------------|
| `name` | `string`     | Registered name of the target |
| `data` | `Uint8Array` | Payload bytes to send         |

**Throws:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.
- `SendError` if the resolved process is unreachable.

---

## Runtime

The central class that manages the Rebar actor system. A `Runtime` wraps an opaque C pointer to the `rebar_runtime_t` structure, which contains a Tokio async runtime, the Rebar process scheduler, and a local name registry.

`Runtime` implements the `Disposable` interface (`Symbol.dispose`), making it compatible with TypeScript's `using` keyword for automatic resource cleanup.

```typescript
class Runtime implements Disposable {
  constructor(nodeId: number | bigint = 1n);
}
```

---

### Constructor

Creates a new Rebar runtime for the given node ID.

```typescript
new Runtime(nodeId: number | bigint = 1n)
```

**Parameters:**

| Name     | Type                 | Default | Description                              |
|----------|----------------------|---------|------------------------------------------|
| `nodeId` | `number \| bigint`   | `1n`    | Unique identifier for this runtime node  |

**Throws:** `Error` if the internal Tokio runtime fails to initialize.

**Notes:**
- The constructor accepts both `number` and `bigint` for convenience. The value is converted to `bigint` internally since Deno's FFI layer requires `BigInt` for `u64` parameters.
- The caller must call `close()` when the runtime is no longer needed, or use the `using` keyword.
- Multiple runtimes can coexist with different `nodeId` values.
- Each runtime maintains its own independent name registry.

**Example:**

```typescript
{
  using rt = new Runtime(1n);
  // use the runtime
}
// runtime is automatically freed here
```

---

### `close`

Frees the runtime and stops all spawned processes. Safe to call multiple times -- subsequent calls are no-ops.

```typescript
close(): void
```

**Notes:**
- All spawned processes are stopped when the runtime is freed.
- The name registry is destroyed along with the runtime.
- All `Deno.UnsafeCallback` objects created by `spawnActor` are also closed.
- Also called automatically by `[Symbol.dispose]()`.

---

### `[Symbol.dispose]`

Implements the `Disposable` protocol. Calls `close()`.

```typescript
[Symbol.dispose](): void
```

**Example:**

```typescript
{
  using rt = new Runtime(1n);
  const pid = rt.spawnActor(new MyActor());
  rt.send(pid, new TextEncoder().encode("hello"));
}
// rt.close() is called automatically
```

---

### `send`

Sends a message to a process identified by PID.

```typescript
send(dest: Pid, data: Uint8Array): void
```

**Parameters:**

| Name   | Type         | Description                    |
|--------|--------------|--------------------------------|
| `dest` | `Pid`        | PID of the destination process |
| `data` | `Uint8Array` | Payload bytes to send          |

**Throws:**
- `SendError` if the destination process does not exist or its mailbox is closed.

**Notes:**
- The data is copied into Rust-owned memory. The caller retains ownership of the `Uint8Array`.
- The underlying C message is created and freed within the same call (via a `try`/`finally` block).

**Example:**

```typescript
try {
  rt.send(targetPid, new TextEncoder().encode("ping"));
} catch (e) {
  if (e instanceof SendError) {
    console.log("process unreachable");
  }
}
```

---

### `register`

Associates a name with a PID in the runtime's local name registry.

```typescript
register(name: string, pid: Pid): void
```

**Parameters:**

| Name   | Type     | Description                    |
|--------|----------|--------------------------------|
| `name` | `string` | Name to register               |
| `pid`  | `Pid`    | PID to associate with the name |

**Throws:**
- `InvalidNameError` if the name bytes are not valid UTF-8.

**Notes:**
- If the name is already registered, the PID is silently overwritten.
- The name string is encoded to UTF-8 bytes via `TextEncoder` for the FFI call.

**Example:**

```typescript
const pid = rt.spawnActor(new MyActor());
rt.register("my_service", pid);
```

---

### `whereis`

Looks up a PID by its registered name.

```typescript
whereis(name: string): Pid
```

**Parameters:**

| Name   | Type     | Description     |
|--------|----------|-----------------|
| `name` | `string` | Name to look up |

**Returns:** The associated `Pid`.

**Throws:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.

**Example:**

```typescript
try {
  const pid = rt.whereis("my_service");
  console.log(`service is at ${pid}`);
} catch (e) {
  if (e instanceof NotFoundError) {
    console.log("service not registered yet");
  }
}
```

---

### `sendNamed`

Sends a message to a process by its registered name. Combines a registry lookup and a send in a single call.

```typescript
sendNamed(name: string, data: Uint8Array): void
```

**Parameters:**

| Name   | Type         | Description                   |
|--------|--------------|-------------------------------|
| `name` | `string`     | Registered name of the target |
| `data` | `Uint8Array` | Payload bytes to send         |

**Throws:**
- `NotFoundError` if no PID is registered under this name.
- `InvalidNameError` if the name is not valid UTF-8.
- `SendError` if the resolved process is unreachable.

**Notes:**
- The registry lock is held only for the lookup; the send happens after the lock is released.
- The caller retains ownership of `data`.

**Example:**

```typescript
try {
  rt.sendNamed("worker", new TextEncoder().encode("work-item"));
} catch (e) {
  console.log(`failed to send: ${e}`);
}
```

---

### `spawnActor`

Spawns a new process backed by the given `Actor`. The actor's `handleMessage` method is called with `msg` set to `null` on startup as a lifecycle hook, and the process PID is returned.

```typescript
spawnActor(actor: Actor): Pid
```

**Parameters:**

| Name    | Type    | Description                          |
|---------|---------|--------------------------------------|
| `actor` | `Actor` | Actor implementation for the process |

**Returns:** The `Pid` of the newly spawned process.

**Notes:**
- The runtime keeps a reference to the `Deno.UnsafeCallback` to prevent garbage collection. All callbacks are closed when the runtime is closed.
- The startup call (`msg === null`) executes synchronously during spawn.

**Example:**

```typescript
class Counter extends Actor {
  private count = 0;

  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      ctx.register("counter", ctx.self());
      return;
    }
    this.count++;
    console.log(`count: ${this.count}`);
  }
}

const pid = rt.spawnActor(new Counter());
console.log(`spawned counter at ${pid}`);
```

---

## Errors

All errors thrown by the TypeScript client are class instances that extend `RebarError`, which itself extends JavaScript's built-in `Error`. They can be caught individually with `instanceof` or as a group via the base class.

### RebarError

The base error class for all Rebar FFI errors.

```typescript
class RebarError extends Error {
  readonly code: number;
  constructor(code: number, message: string);
}
```

- `code` is the integer error code from the FFI layer.
- The `name` property is set to `"RebarError"`.
- The error message includes both the code and a human-readable description.

---

### SendError

Thrown when a message cannot be delivered to a process. Extends `RebarError`.

```typescript
class SendError extends RebarError {
  constructor();
}
```

- Thrown by `send`, `sendNamed`, `Context.send`, and `Context.sendNamed`.
- Typically means the destination process does not exist or its mailbox is closed.
- FFI error code: `-2` (`REBAR_ERR_SEND_FAILED`).

**Example:**

```typescript
try {
  rt.send(pid, new TextEncoder().encode("hello"));
} catch (e) {
  if (e instanceof SendError) {
    console.log("message delivery failed");
  }
}
```

---

### NotFoundError

Thrown when a name is not found in the registry. Extends `RebarError`.

```typescript
class NotFoundError extends RebarError {
  constructor();
}
```

- Thrown by `whereis`, `sendNamed`, `Context.whereis`, and `Context.sendNamed`.
- FFI error code: `-3` (`REBAR_ERR_NOT_FOUND`).

**Example:**

```typescript
try {
  rt.whereis("nonexistent");
} catch (e) {
  if (e instanceof NotFoundError) {
    console.log("name not registered");
  }
}
```

---

### InvalidNameError

Thrown when a name passed to a registry function is not valid UTF-8. Extends `RebarError`.

```typescript
class InvalidNameError extends RebarError {
  constructor();
}
```

- Thrown by `register`, `whereis`, `sendNamed`, and their `Context` equivalents.
- In practice this is rare in TypeScript since `string` is always Unicode, but the FFI layer enforces the constraint.
- FFI error code: `-4` (`REBAR_ERR_INVALID_NAME`).

---

### `checkError`

Internal helper that translates FFI return codes into thrown exceptions. Not part of the public API, but documented here for completeness.

```typescript
function checkError(rc: number): void
```

| Return Code | Exception          | Meaning                            |
|-------------|--------------------|------------------------------------|
| `0`         | *(none)*           | Success                            |
| `-1`        | `Error`            | Null pointer (internal bug)        |
| `-2`        | `SendError`        | Message delivery failed            |
| `-3`        | `NotFoundError`    | Name not in registry               |
| `-4`        | `InvalidNameError` | Name is not valid UTF-8            |
| other       | `RebarError`       | Unknown error                      |

**Notes:**
- A return code of `-1` (null pointer) throws a plain `Error` rather than a `RebarError`, since it indicates a bug in the bindings rather than a user error.

---

## Library Discovery

The TypeScript client uses `Deno.dlopen` to load the `librebar_ffi` shared library. The search order is:

1. **`REBAR_LIB_PATH` environment variable.** If set, this path is used directly as the argument to `Deno.dlopen`.
2. **System library search.** If `REBAR_LIB_PATH` is not set, the platform-specific library name is passed to `Deno.dlopen`, which searches the system library path (`LD_LIBRARY_PATH` on Linux, `DYLD_LIBRARY_PATH` on macOS, `PATH` on Windows).

### Platform-Specific Library Names

| Platform | Library File          |
|----------|-----------------------|
| Linux    | `librebar_ffi.so`     |
| macOS    | `librebar_ffi.dylib`  |
| Windows  | `rebar_ffi.dll`       |

---

## Memory Management

### Runtime Lifecycle

The caller who creates a `Runtime` owns it and must free it with `close()`. The idiomatic pattern uses the `using` keyword:

```typescript
{
  using rt = new Runtime(1n);
  const pid = rt.spawnActor(new MyActor());
  rt.send(pid, new TextEncoder().encode("hello"));
}
// runtime is freed here
```

`close` is idempotent -- calling it multiple times is safe. The runtime also cleans up all `Deno.UnsafeCallback` objects created by `spawnActor`.

### Messages

Messages are created from `Uint8Array` values. The FFI layer copies the data into a C-side message, sends it, and frees the C message within the same method call (via a `try`/`finally` block). The caller's `Uint8Array` is never modified.

### Actors

When you call `spawnActor`, a `Deno.UnsafeCallback` is created and stored in the runtime's internal `callbacks` array. This prevents the callback from being garbage collected while the runtime is alive. The actor object itself is captured by the callback closure. All callbacks are closed when the runtime is closed.

### Thread Safety

The underlying C runtime is internally synchronized -- it uses a Tokio runtime (thread-safe) and the name registry is protected by a mutex. Deno is single-threaded by default, so concurrent access is not a concern for typical usage. If using workers, each worker should create its own `Runtime` instance.

---

## Complete Example

```typescript
import {
  Actor,
  Context,
  NotFoundError,
  Pid,
  Runtime,
  SendError,
} from "./src/mod.ts";

/** An actor that prints a greeting when it receives a message. */
class Greeter extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      // Startup: register ourselves under a well-known name.
      ctx.register("greeter", ctx.self());
      console.log(`greeter started at ${ctx.self()}`);
      return;
    }
    console.log(`greeter received: ${new TextDecoder().decode(msg)}`);
  }
}

// Create a runtime on node 1.
{
  using rt = new Runtime(1n);

  // Spawn the greeter actor.
  const pid = rt.spawnActor(new Greeter());

  // Send a message by PID.
  try {
    rt.send(pid, new TextEncoder().encode("hello by PID"));
  } catch (e) {
    if (e instanceof SendError) {
      console.log(`send failed: ${e}`);
    }
  }

  // Send a message by name.
  try {
    rt.sendNamed("greeter", new TextEncoder().encode("hello by name"));
  } catch (e) {
    console.log(`send named failed: ${e}`);
  }

  // Look up by name.
  try {
    const found = rt.whereis("greeter");
    console.log(`greeter is at ${found}`);
  } catch (e) {
    if (e instanceof NotFoundError) {
      console.log("greeter not registered");
    }
  }

  // Give async processes a moment to execute.
  await new Promise((r) => setTimeout(r, 100));
}
```

---

## See Also

- [rebar-ffi C-ABI Reference](rebar-ffi.md) -- the underlying C API that this package wraps
- [TypeScript Actor Patterns Guide](../guides/actors-typescript.md) -- idiomatic patterns for building actors in TypeScript
- [Architecture](../architecture.md) -- how the FFI layer fits into the overall crate structure
