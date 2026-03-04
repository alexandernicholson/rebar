# Rebar TypeScript Client

Idiomatic TypeScript module for the Rebar actor runtime. Wraps the `librebar_ffi` C shared library via Deno's FFI API.

## Prerequisites

- Deno 1.38+ (tested with Deno 2.7)
- Rust toolchain (to build `librebar_ffi`)

## Quick Start

### 1. Build the shared library

```bash
cd /path/to/rebar
cargo build --release -p rebar-ffi
```

### 2. Set library path

The TypeScript client supports the `REBAR_LIB_PATH` environment variable to specify the exact path to the shared library:

```bash
export REBAR_LIB_PATH="/path/to/rebar/target/release/librebar_ffi.so"
```

Alternatively, add the library directory to your system library path:

```bash
# Linux
export LD_LIBRARY_PATH="/path/to/rebar/target/release:$LD_LIBRARY_PATH"

# macOS
export DYLD_LIBRARY_PATH="/path/to/rebar/target/release:$DYLD_LIBRARY_PATH"
```

### 3. Hello World

```typescript
import { Actor, Context, Runtime } from "./src/mod.ts";

class Greeter extends Actor {
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    console.log(`Hello from process ${ctx.self()}`);
  }
}

{
  using runtime = new Runtime(1n);
  const pid = runtime.spawnActor(new Greeter());
  console.log(`Spawned: ${pid}`);
}
```

### 4. Run with Deno FFI flags

Deno's FFI API requires explicit permission flags:

```bash
deno run --unstable-ffi --allow-ffi --allow-env hello.ts
```

- `--unstable-ffi` -- enables the unstable FFI API
- `--allow-ffi` -- grants permission to load native libraries
- `--allow-env` -- grants permission to read `REBAR_LIB_PATH`

### 5. Running Tests

```bash
cd /path/to/rebar/clients/typescript
deno task test
```

Or directly:

```bash
deno test --unstable-ffi --allow-ffi --allow-env tests/
```

## API Reference

See [TypeScript Client API Reference](../../docs/api/client-typescript.md).

## Actor Patterns

See [TypeScript Actor Patterns Guide](../../docs/guides/actors-typescript.md).
