# TypeScript Actor Patterns Guide

This guide explains the design decisions behind the Rebar TypeScript client and demonstrates idiomatic patterns for building actors in TypeScript. Where Erlang/BEAM uses processes, pattern matching, and OTP behaviors, TypeScript achieves the same goals through abstract classes, the `Disposable` protocol, `Uint8Array` payloads, and `bigint` identifiers.

---

## Table of Contents

1. [Why an Abstract Class](#1-why-an-abstract-class)
2. [The `Disposable` Protocol and `using`](#2-the-disposable-protocol-and-using)
3. [`Uint8Array` for Payloads](#3-uint8array-for-payloads)
4. [`bigint` for IDs](#4-bigint-for-ids)
5. [Error Class Hierarchy](#5-error-class-hierarchy)
6. [The Context Pattern](#6-the-context-pattern)
7. [Pattern: Ping-Pong Actors](#7-pattern-ping-pong-actors)
8. [Pattern: Named Services](#8-pattern-named-services)
9. [Pattern: Message Dispatch](#9-pattern-message-dispatch)

---

## 1. Why an Abstract Class

The `Actor` interface is defined as an abstract class with a single abstract method:

```typescript
abstract class Actor {
  abstract handleMessage(ctx: Context, msg: Uint8Array | null): void;
}
```

This follows a core TypeScript design principle: **use the `abstract` keyword to define contracts that subclasses must fulfill**. TypeScript's `abstract` keyword is the language-native way to say "this class cannot be instantiated directly, and subclasses must implement these methods."

### Why `abstract class` and Not `interface`

TypeScript has both `interface` and `abstract class`. The Rebar client uses `abstract class` because:

- **Runtime enforcement.** An `abstract class` prevents direct instantiation at runtime. `new Actor()` throws a `TypeError`. A TypeScript `interface` disappears entirely at runtime -- it provides no guardrails if you accidentally construct a plain object.
- **Class-based OOP is native to TypeScript.** TypeScript extends JavaScript's class syntax with `abstract`, `readonly`, and access modifiers. Using `abstract class` keeps the actor definition in the class hierarchy, making `instanceof Actor` checks work naturally.
- **Consistency with `extends`.** Actors are defined with `extends Actor`, which is the natural way to express "this is a kind of actor" in TypeScript. Using `implements` with an interface would work for the type checker, but would lose the runtime check and the signal that `Actor` is meant to be a base class.

In Erlang/OTP, a `gen_server` module implements multiple callbacks (`init/1`, `handle_call/3`, `handle_cast/2`, `handle_info/2`, `terminate/2`). The TypeScript client collapses this into a single entry point because:

- **Startup is a message.** When a process starts, `handleMessage` is called with `msg` set to `null`. This removes the need for a separate initialization method. The actor can register itself, set up state, or do nothing -- it is the actor's choice.
- **TypeScript has no pattern matching on message shape.** TypeScript uses `instanceof`, string comparisons, or discriminated unions to dispatch on message type. The single `handleMessage` method is the natural place for this dispatch logic.
- **Any class can be an actor.** Extending `Actor` and implementing one method is all it takes. There is no framework registration, no decorators, and no mandatory boilerplate.

```typescript
// Minimal actor -- a class with one method.
class Pinger extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg !== null) {
      console.log(`pinger got: ${new TextDecoder().decode(msg)}`);
    }
  }
}
```

---

## 2. The `Disposable` Protocol and `using`

The `Runtime` class implements the `Disposable` interface via `[Symbol.dispose]()`. This enables the `using` keyword, a modern TypeScript/JavaScript feature for deterministic resource cleanup (TC39 Explicit Resource Management proposal, supported in TypeScript 5.2+ and Deno).

```typescript
{
  using rt = new Runtime(1n);
  const pid = rt.spawnActor(new MyActor());
  rt.send(pid, new TextEncoder().encode("hello"));
}
// rt[Symbol.dispose]() is called automatically, which calls rt.close()
```

### Why This Matters

- **Deterministic cleanup.** When execution leaves the block (whether normally or via an exception), `[Symbol.dispose]()` is called. The runtime is always cleaned up. Without `using`, a forgotten `close()` call after an exception would leak the Tokio runtime and all its threads.
- **No `try`/`finally` boilerplate.** The `using` keyword eliminates the need for manual `try`/`finally` blocks to ensure cleanup. It is the TypeScript equivalent of Go's `defer` and Python's `with`.
- **Forward-looking.** The `Disposable` protocol is part of the TC39 standard track. Using it aligns the Rebar client with the direction of the JavaScript ecosystem.

### Manual Close

If `using` does not fit your control flow (for example, if the runtime lives beyond a single block scope), call `close()` explicitly:

```typescript
const rt = new Runtime(1n);
try {
  const pid = rt.spawnActor(new MyActor());
  rt.send(pid, new TextEncoder().encode("hello"));
} finally {
  rt.close();
}
```

`close()` is idempotent. Calling it twice does nothing. It also closes all `Deno.UnsafeCallback` objects that were created by `spawnActor`.

---

## 3. `Uint8Array` for Payloads

All message payloads in the TypeScript client use `Uint8Array`, JavaScript's native type for binary data:

```typescript
rt.send(pid, new TextEncoder().encode("hello world"));
```

### Why `Uint8Array` and Not `string`

Rebar messages are arbitrary byte sequences at the wire level. The TypeScript client does not impose any encoding or serialization format. Using `Uint8Array` makes this explicit:

- **`Uint8Array` is the truth.** The FFI layer passes raw byte pointers. Wrapping them in `string` would require choosing an encoding and handling decode errors. Using `Uint8Array` avoids this entirely.
- **Encoding is the caller's choice.** To send a string, encode it with `TextEncoder`: `new TextEncoder().encode("hello")`. To send JSON, use `new TextEncoder().encode(JSON.stringify(obj))`. To send binary protocols, construct the `Uint8Array` directly. The actor framework does not care.
- **`Uint8Array` is zero-copy friendly.** Deno's FFI layer can pass `Uint8Array` directly to C functions as a buffer without any intermediate conversion.
- **Consistency with Deno.** Deno's standard library uses `Uint8Array` throughout for binary data -- file I/O, network sockets, and cryptographic functions all use this type.

### Receiving Messages

Inside `handleMessage`, `msg` is `Uint8Array | null`. Decode as needed:

```typescript
class TextActor extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      return;
    }
    const text = new TextDecoder().decode(msg);
    console.log(`received text: ${text}`);
  }
}
```

### The `null` Startup Message

The startup message is `null`, not an empty `Uint8Array`. This is a deliberate choice:

- **`null` is unambiguous.** An empty `Uint8Array` (`new Uint8Array(0)`) is a valid message payload. Using `null` for the startup signal means there is no confusion between "no message" and "empty message."
- **TypeScript's union types make this safe.** The type `Uint8Array | null` forces callers to check for `null` before accessing `Uint8Array` methods. The type checker prevents accidental `msg.byteLength` calls on a startup message.

---

## 4. `bigint` for IDs

Process identifiers (`Pid`) use `bigint` for both `nodeId` and `localId`:

```typescript
const pid = new Pid(1n, 42n);
console.log(pid.nodeId); // 1n (bigint)
```

### Why `bigint` and Not `number`

Deno's FFI layer requires `BigInt` for 64-bit unsigned integers (`u64`). This is not a style choice -- it is a technical requirement:

- **JavaScript `number` is a 64-bit float.** It can represent integers exactly only up to `2^53 - 1` (`Number.MAX_SAFE_INTEGER`). Rebar node IDs and local IDs are `u64` values that can exceed this limit.
- **Deno FFI mandates `BigInt` for `u64`.** When a Deno FFI symbol is declared with `"u64"` parameter or return type, the JavaScript side must use `BigInt`. Passing a `number` would cause a runtime error.
- **Consistency avoids conversion bugs.** If `Pid` used `number` internally and converted to `bigint` at the FFI boundary, values above `2^53` would silently lose precision. Using `bigint` throughout eliminates this class of bugs entirely.

### Convenience in the Constructor

The `Runtime` constructor accepts both `number` and `bigint` for ergonomics:

```typescript
const rt1 = new Runtime(1n);    // bigint literal
const rt2 = new Runtime(1);     // number, converted internally
```

The constructor calls `BigInt(nodeId)` to normalize the value. This is a convenience for the common case where node IDs are small numbers. `Pid` fields are always `bigint`.

---

## 5. Error Class Hierarchy

The TypeScript client uses thrown exceptions for error handling, following JavaScript's convention. All Rebar-specific errors extend a common base class:

```
Error
  RebarError            -- base class, code + message
    SendError           -- message delivery failed (code -2)
    NotFoundError       -- name not in registry (code -3)
    InvalidNameError    -- name not valid UTF-8 (code -4)
```

### Why a Class Hierarchy

- **`instanceof` checks are idiomatic.** JavaScript and TypeScript use `instanceof` for error type checking. The class hierarchy makes `e instanceof SendError` and `e instanceof RebarError` both work naturally.
- **Each error sets its own `name` property.** This makes error messages and stack traces immediately identifiable: `SendError: rebar error -2: failed to deliver message`.
- **The base class carries the FFI error code.** Every `RebarError` has a `code` property with the numeric FFI return code, useful for logging or programmatic inspection.

### Catching Specific Errors

```typescript
try {
  rt.sendNamed("worker", new TextEncoder().encode("task"));
} catch (e) {
  if (e instanceof NotFoundError) {
    console.log("worker not registered");
  } else if (e instanceof SendError) {
    console.log("worker unreachable");
  }
}
```

### Catching All Rebar Errors

```typescript
try {
  rt.sendNamed("worker", new TextEncoder().encode("task"));
} catch (e) {
  if (e instanceof RebarError) {
    console.log(`rebar operation failed: ${e}`);
  }
}
```

### Inside `handleMessage`

Since `handleMessage` returns `void`, actors handle errors inline:

```typescript
class Forwarder extends Actor {
  constructor(private target: Pid) {
    super();
  }

  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      return;
    }
    try {
      ctx.send(this.target, msg);
    } catch (e) {
      // Log, retry, drop -- the actor decides.
      if (e instanceof SendError) {
        console.log(`forward to ${this.target} failed`);
      }
    }
  }
}
```

This is a deliberate design choice. In Erlang, a process that crashes is restarted by its supervisor. In the TypeScript client, the actor is responsible for its own error handling. An unhandled exception inside `handleMessage` would propagate through the `Deno.UnsafeCallback` boundary, which has undefined behavior. Always handle exceptions within `handleMessage`.

---

## 6. The Context Pattern

Every call to `handleMessage` receives a `Context` as its first argument. Context provides the process's identity (`self()`) and access to messaging (`send`, `sendNamed`) and the registry (`register`, `whereis`).

This is **dependency injection, not global state**. The actor never imports a global runtime variable or calls module-level functions to send messages. Everything it needs arrives through the context.

### Why This Matters

- **Testability.** You can construct a `Context` with a mock runtime for unit testing without starting a real Rebar runtime.
- **Isolation.** Each process gets its own context. There is no shared mutable state between actors unless you explicitly introduce it.
- **Clarity.** Reading the `handleMessage` signature tells you exactly what capabilities the actor has access to.

### Context vs. Runtime

The `Context` methods mirror `Runtime` methods exactly. The distinction is about scope:

| Operation    | Via Runtime                        | Via Context                        |
|--------------|------------------------------------|------------------------------------|
| Send         | `rt.send(dest, data)`              | `ctx.send(dest, data)`             |
| Register     | `rt.register(name, pid)`           | `ctx.register(name, pid)`          |
| Whereis      | `rt.whereis(name)`                 | `ctx.whereis(name)`                |
| SendNamed    | `rt.sendNamed(name, data)`         | `ctx.sendNamed(name, data)`        |
| Self         | --                                 | `ctx.self()`                       |

Use `Runtime` methods from top-level or setup code. Use `Context` methods from inside `handleMessage`. Both route to the same underlying FFI calls.

---

## 7. Pattern: Ping-Pong Actors

Two actors exchanging messages through the registry demonstrate named messaging, startup initialization, and cross-actor communication.

```typescript
import { Actor, Context, Runtime } from "./src/mod.ts";

/** Sends a "ping" to the ponger on startup. */
class Pinger extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      // Startup: register ourselves so Ponger can reply.
      ctx.register("pinger", ctx.self());
      // Send the first ping.
      try {
        ctx.sendNamed("ponger", new TextEncoder().encode("ping"));
      } catch (e) {
        console.log(`pinger: failed to send: ${e}`);
      }
      return;
    }
    console.log(`pinger received: ${new TextDecoder().decode(msg)}`);
  }
}

/** Waits for a "ping" and replies with "pong". */
class Ponger extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      // Startup: register ourselves so Pinger can find us.
      ctx.register("ponger", ctx.self());
      return;
    }
    console.log(`ponger received: ${new TextDecoder().decode(msg)}`);
    // Reply to the pinger.
    try {
      ctx.sendNamed("pinger", new TextEncoder().encode("pong"));
    } catch (e) {
      console.log(`ponger: failed to reply: ${e}`);
    }
  }
}

{
  using rt = new Runtime(1n);

  // Spawn ponger first so its name is registered before pinger starts.
  rt.spawnActor(new Ponger());
  rt.spawnActor(new Pinger());

  // Let the async processes run.
  await new Promise((r) => setTimeout(r, 100));
}
```

**Key points:**

- Ponger is spawned first so that its name is registered before Pinger tries to send.
- Both actors register themselves during the startup call (`msg === null`).
- Communication uses `sendNamed` so actors do not need to know each other's PIDs directly.
- Error handling uses `try`/`catch` -- failed sends are caught and logged, not ignored.

---

## 8. Pattern: Named Services

Named services are actors registered under well-known names that other parts of the system can discover via `whereis` or send to via `sendNamed`. This is analogous to Erlang's registered processes.

```typescript
import { Actor, Context, NotFoundError, Runtime } from "./src/mod.ts";

/** A simple in-memory key-value store actor. */
class KVStore extends Actor {
  private data = new Map<string, string>();

  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      ctx.register("kv", ctx.self());
      console.log("kv store ready");
      return;
    }
    const payload = new TextDecoder().decode(msg);
    console.log(`kv store received: ${payload}`);
    // In a real implementation, parse the payload as a command
    // (e.g., "SET key value" or "GET key") and respond accordingly.
  }
}

{
  using rt = new Runtime(1n);

  // Start the service.
  rt.spawnActor(new KVStore());

  // Any part of the system can now discover the service.
  try {
    const pid = rt.whereis("kv");
    console.log(`kv store is at ${pid}`);
  } catch (e) {
    if (e instanceof NotFoundError) {
      console.log("kv not found");
    }
  }

  // Send a command by name -- no PID needed.
  rt.sendNamed("kv", new TextEncoder().encode("SET greeting hello"));

  await new Promise((r) => setTimeout(r, 100));
}
```

**Key points:**

- The service registers itself during startup. Callers use `whereis` or `sendNamed` to interact with it.
- State (the `data` map) lives in the actor instance. It is initialized as a class field, which runs before the constructor body. The startup handler (`msg === null`) is used for registration, not for state initialization. This separates TypeScript-level construction from Rebar-level lifecycle.
- `sendNamed` avoids the need to pass PIDs around, making the system loosely coupled.

---

## 9. Pattern: Message Dispatch

Since TypeScript does not have Erlang-style pattern matching on message shapes, actors that handle multiple message types need an explicit dispatch mechanism. The simplest approach is a prefix byte or tag:

```typescript
class Router extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      ctx.register("router", ctx.self());
      return;
    }
    if (msg.byteLength === 0) {
      return;
    }

    // First byte is the message type tag.
    const tag = msg[0];
    const payload = msg.slice(1);

    switch (tag) {
      case 0x01:
        this.handleCreate(ctx, payload);
        break;
      case 0x02:
        this.handleUpdate(ctx, payload);
        break;
      case 0x03:
        this.handleDelete(ctx, payload);
        break;
      default:
        console.log(`unknown message type: 0x${tag.toString(16).padStart(2, "0")}`);
    }
  }

  private handleCreate(_ctx: Context, data: Uint8Array): void {
    console.log(`create: ${new TextDecoder().decode(data)}`);
  }

  private handleUpdate(_ctx: Context, data: Uint8Array): void {
    console.log(`update: ${new TextDecoder().decode(data)}`);
  }

  private handleDelete(_ctx: Context, data: Uint8Array): void {
    console.log(`delete: ${new TextDecoder().decode(data)}`);
  }
}
```

For richer message formats, consider encoding messages with JSON. The actor's `handleMessage` deserializes the payload and dispatches based on the decoded type:

```typescript
interface Command {
  action: string;
  key: string;
  value?: string;
}

class JSONActor extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      return;
    }
    let cmd: Command;
    try {
      cmd = JSON.parse(new TextDecoder().decode(msg));
    } catch {
      console.log("invalid JSON message");
      return;
    }

    switch (cmd.action) {
      case "get":
        console.log(`get ${cmd.key}`);
        break;
      case "set":
        console.log(`set ${cmd.key} = ${cmd.value}`);
        break;
      default:
        console.log(`unknown action: ${cmd.action}`);
    }
  }
}
```

For structured binary protocols, the `DataView` API provides fine-grained control over byte layout:

```typescript
class BinaryActor extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    if (msg === null) {
      return;
    }
    if (msg.byteLength < 4) {
      return;
    }
    // First 4 bytes: big-endian unsigned int (message type)
    const view = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    const msgType = view.getUint32(0, false); // false = big-endian
    const payload = msg.slice(4);

    switch (msgType) {
      case 1:
        console.log(`type 1: ${new TextDecoder().decode(payload)}`);
        break;
      case 2:
        console.log(`type 2: ${payload}`);
        break;
    }
  }
}
```

This explicit dispatch is more verbose than Erlang's pattern matching, but it is straightforward, debuggable, and uses familiar TypeScript patterns.

---

## See Also

- [TypeScript Client API Reference](../api/client-typescript.md) -- complete class and method documentation
- [rebar-ffi C-ABI Reference](../api/rebar-ffi.md) -- the underlying C API
- [Extending Rebar](../extending.md) -- building FFI bindings for new languages
