# Rebar Python Client

Idiomatic Python package for the Rebar actor runtime. Wraps the `librebar_ffi` C shared library via `ctypes`.

## Prerequisites

- Python 3.10+
- Rust toolchain (to build `librebar_ffi`)

## Quick Start

### 1. Build the shared library

```bash
cd /path/to/rebar
cargo build --release -p rebar-ffi
```

### 2. Set library path

**Linux:**

```bash
export LD_LIBRARY_PATH="/path/to/rebar/target/release:$LD_LIBRARY_PATH"
```

**macOS:**

```bash
export DYLD_LIBRARY_PATH="/path/to/rebar/target/release:$DYLD_LIBRARY_PATH"
```

**Windows (PowerShell):**

```powershell
$env:PATH = "C:\path\to\rebar\target\release;$env:PATH"
```

Alternatively, set `REBAR_LIB_PATH` to the exact path of the shared library file:

```bash
export REBAR_LIB_PATH="/path/to/rebar/target/release/librebar_ffi.so"
```

### 3. Install the package

From the repository root:

```bash
pip install -e clients/python
```

Or, without installing, add the client directory to your Python path:

```bash
export PYTHONPATH="/path/to/rebar/clients/python:$PYTHONPATH"
```

### 4. Hello World

```python
from rebar import Actor, Context, Runtime


class Greeter(Actor):
    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        pid = ctx.self_pid()
        print(f"Hello from process {pid}")


with Runtime(node_id=1) as rt:
    pid = rt.spawn_actor(Greeter())
    print(f"Spawned: {pid}")
```

### 5. Run

```bash
python hello.py
```

If the library is in the default build location (`target/release/`), the client will find it automatically when run from within the repository. Otherwise, set `LD_LIBRARY_PATH` or `REBAR_LIB_PATH` as shown above.

## Running Tests

```bash
cd /path/to/rebar
pip install pytest
pytest clients/python/tests/
```

## API Reference

See [Python Client API Reference](../../docs/api/client-python.md).

## Actor Patterns

See [Python Actor Patterns Guide](../../docs/guides/actors-python.md).
