# Client Libraries Design: Go, Python, TypeScript

**Date:** 2026-03-04
**Scope:** Create idiomatic client libraries for Go, Python, and TypeScript wrapping the `rebar-ffi` C-ABI shared library, with rich actor abstractions and comprehensive documentation.

## Approach

Rich SDK wrappers (Approach B) — each library provides both low-level FFI bindings and higher-level actor abstractions idiomatic to the language. Users build from source (`cargo build --release -p rebar-ffi`) and load the shared library at runtime.

## Repository Layout

```
clients/
├── go/
│   ├── rebar/          # Go package "rebar"
│   │   ├── rebar.go    # Runtime, Pid, Msg types + FFI bindings
│   │   ├── actor.go    # Actor interface + base actor
│   │   ├── registry.go # Named process helpers
│   │   └── rebar_test.go
│   ├── go.mod
│   └── README.md
├── python/
│   ├── rebar/          # Python package "rebar"
│   │   ├── __init__.py
│   │   ├── _ffi.py     # ctypes bindings (private)
│   │   ├── runtime.py  # Runtime context manager
│   │   ├── actor.py    # Actor base class
│   │   ├── registry.py # Named process helpers
│   │   └── types.py    # Pid, Msg types
│   ├── tests/
│   ├── pyproject.toml
│   └── README.md
├── typescript/
│   ├── src/
│   │   ├── ffi.ts      # Deno FFI bindings (private)
│   │   ├── runtime.ts  # Runtime class with Disposable
│   │   ├── actor.ts    # Actor abstract class
│   │   ├── registry.ts # Named process helpers
│   │   ├── types.ts    # Pid, Msg types
│   │   └── mod.ts      # Re-exports
│   ├── tests/
│   ├── deno.json
│   └── README.md
└── README.md           # Overview + build instructions for librebar_ffi
```

## Shared Library Distribution

Build from source. The top-level `clients/README.md` provides extremely clear step-by-step instructions:
1. Prerequisites (Rust toolchain)
2. `cargo build --release -p rebar-ffi`
3. Platform-specific output paths (`target/release/librebar_ffi.{so,dylib,dll}`)
4. Setting `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH`
5. Verification command

Each language README repeats the build steps relevant to that language's loading mechanism.

## FFI Surface Wrapped

All 11 functions, 3 types, 5 error codes from `rebar-ffi`:

**Types:** `RebarRuntime`, `RebarPid`, `RebarMsg`

**Functions:**
- Runtime: `rebar_runtime_new`, `rebar_runtime_free`
- Messages: `rebar_msg_create`, `rebar_msg_data`, `rebar_msg_len`, `rebar_msg_free`
- Processes: `rebar_spawn`, `rebar_send`
- Registry: `rebar_register`, `rebar_whereis`, `rebar_send_named`

**Error Codes:** `REBAR_OK` (0), `REBAR_ERR_NULL_PTR` (-1), `REBAR_ERR_SEND_FAILED` (-2), `REBAR_ERR_NOT_FOUND` (-3), `REBAR_ERR_INVALID_NAME` (-4)

## Actor Abstractions

### Go — Interface + Context

```go
type Actor interface {
    HandleMessage(ctx *Context, msg *Msg)
}

type Context struct { ... }
// ctx.Self() Pid
// ctx.Send(pid Pid, data []byte) error
// ctx.Register(name string, pid Pid) error
// ctx.Whereis(name string) (Pid, error)
// ctx.SendNamed(name string, data []byte) error

pid, err := runtime.SpawnActor(myActor)
```

Go idioms: single-method interface, `error` return values, `[]byte` for payloads.

### Python — ABC + Context Manager

```python
class MyActor(rebar.Actor):
    def handle_message(self, ctx: rebar.Context, msg: bytes):
        ...

with rebar.Runtime(node_id=1) as rt:
    pid = rt.spawn_actor(MyActor())
    rt.send(pid, b"hello")
```

Python idioms: abstract base class, context managers for cleanup, `bytes` for payloads, type hints, exceptions for errors.

### TypeScript — Abstract Class + Disposable

```typescript
class MyActor extends rebar.Actor {
  handleMessage(ctx: rebar.Context, msg: Uint8Array): void { ... }
}

using runtime = new rebar.Runtime(1);
const pid = runtime.spawnActor(new MyActor());
await runtime.send(pid, new TextEncoder().encode("hello"));
```

TypeScript idioms: abstract class, `Disposable` protocol (`using`), `Uint8Array` for binary data, async methods, Deno FFI.

## Error Handling

Each language maps the 5 FFI error codes idiomatically:

### Go
```go
type RebarError struct {
    Code    int
    Message string
}
func (e *RebarError) Error() string { return e.Message }
```
Returns `error` values. `REBAR_ERR_NULL_PTR` is an internal panic (programming bug).

### Python
```python
class RebarError(Exception): ...
class SendError(RebarError): ...      # REBAR_ERR_SEND_FAILED
class NotFoundError(RebarError): ...   # REBAR_ERR_NOT_FOUND
class InvalidNameError(RebarError): ... # REBAR_ERR_INVALID_NAME
```
Raises exceptions. `REBAR_ERR_NULL_PTR` raises `RuntimeError` (internal bug).

### TypeScript
```typescript
class RebarError extends Error { code: number; }
class SendError extends RebarError { }
class NotFoundError extends RebarError { }
class InvalidNameError extends RebarError { }
```
Throws typed errors. `REBAR_ERR_NULL_PTR` throws generic `Error` (internal bug).

## Memory Management

- **Go:** `runtime.Finalize()` for explicit cleanup; `runtime.Close()` pattern. Messages freed automatically after callback returns.
- **Python:** `Runtime` is a context manager (`with` block). `Msg` objects freed in `__del__`.
- **TypeScript:** `Runtime` implements `Disposable` (`using` keyword). `Msg` objects freed in `[Symbol.dispose]()`.

All three: users never call `_free` functions directly.

## Documentation

### Per-language docs:
1. **`clients/<lang>/README.md`** — Getting started: prerequisites, build `librebar_ffi`, install package, hello world example
2. **`docs/api/client-go.md`** / `client-python.md` / `client-typescript.md`** — Full API reference following existing `docs/api/` pattern
3. **`docs/guides/actors-go.md`** / `actors-python.md` / `actors-typescript.md` — Pattern guide explaining idiomatic choices

### Top-level:
- **`clients/README.md`** — Build instructions for the shared library (extremely clear, step-by-step, per-platform)

## Conventions

- Messages are raw bytes — users choose their own serialization
- All three libraries expose identical logical API surfaces
- Documentation uses real API signatures from the implementation
- Tests require the shared library to be built first
- No third-party dependencies beyond the language's standard library + FFI mechanism
