# rebar-ffi — C-ABI Reference

`rebar-ffi` is a shared library (`librebar_ffi.so` / `librebar_ffi.dylib` / `rebar_ffi.dll`) that exposes the Rebar actor runtime through a stable C ABI. It allows polyglot developers to spawn lightweight processes, send messages, and manage a name registry from any language with C FFI support — Go, Python, TypeScript, Ruby, C#, and others.

---

## Types

### RebarPid

A C-compatible process identifier composed of two 64-bit unsigned integers.

**Rust definition:**

```rust
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RebarPid {
    pub node_id: u64,
    pub local_id: u64,
}
```

**C equivalent:**

```c
typedef struct {
    uint64_t node_id;
    uint64_t local_id;
} rebar_pid_t;
```

- `node_id` identifies the runtime node.
- `local_id` identifies the process within that node.

`RebarPid` is passed by value across the FFI boundary and is safe to copy.

---

### RebarMsg

An opaque type wrapping a `Vec<u8>` byte buffer. Messages are created and freed exclusively through the Message API functions. Callers must never inspect or modify the struct internals directly.

**C forward declaration:**

```c
typedef struct rebar_msg_t rebar_msg_t;
```

---

### RebarRuntime

An opaque type containing:

- A Tokio async runtime (drives async process execution)
- A Rebar `Runtime` (the actor scheduler)
- A local name registry (`Mutex<HashMap<String, ProcessId>>`)

**C forward declaration:**

```c
typedef struct rebar_runtime_t rebar_runtime_t;
```

---

## Error Codes

All functions that return `i32` use the following error codes:

| Code | Constant                | Meaning                          |
|------|-------------------------|----------------------------------|
| `0`  | `REBAR_OK`              | Success                          |
| `-1` | `REBAR_ERR_NULL_PTR`    | A required pointer was null      |
| `-2` | `REBAR_ERR_SEND_FAILED` | Failed to deliver message        |
| `-3` | `REBAR_ERR_NOT_FOUND`   | Name not found in registry       |
| `-4` | `REBAR_ERR_INVALID_NAME`| Name bytes are not valid UTF-8   |

---

## Functions

### Message API

#### `rebar_msg_create`

Create a new message from a raw byte buffer.

```c
rebar_msg_t *rebar_msg_create(const uint8_t *data, size_t len);
```

**Parameters:**

| Name   | Type             | Description                       |
|--------|------------------|-----------------------------------|
| `data` | `const uint8_t*` | Pointer to the source byte buffer |
| `len`  | `size_t`         | Number of bytes to copy           |

**Returns:** A heap-allocated `rebar_msg_t*`, or `NULL` if `data` is null and `len` is non-zero.

**Notes:**
- The bytes are copied into Rust-owned memory. The caller retains ownership of the source buffer.
- An empty message (`len == 0`) is permitted even with a null `data` pointer.
- The caller owns the returned pointer and must free it with `rebar_msg_free`.

**Example:**

```c
const char *payload = "hello";
rebar_msg_t *msg = rebar_msg_create((const uint8_t *)payload, 5);
// ... use msg ...
rebar_msg_free(msg);
```

---

#### `rebar_msg_data`

Get a pointer to the message's internal byte buffer.

```c
const uint8_t *rebar_msg_data(const rebar_msg_t *msg);
```

**Parameters:**

| Name  | Type                   | Description       |
|-------|------------------------|-------------------|
| `msg` | `const rebar_msg_t*`   | Message to inspect|

**Returns:** A pointer to the internal data, or `NULL` if `msg` is null.

**Notes:**
- The returned pointer is valid only as long as the message has not been freed.
- Do not write through this pointer; the data is read-only.
- Consecutive calls return the same pointer (the address is stable).

---

#### `rebar_msg_len`

Get the length of the message's data buffer.

```c
size_t rebar_msg_len(const rebar_msg_t *msg);
```

**Parameters:**

| Name  | Type                   | Description       |
|-------|------------------------|-------------------|
| `msg` | `const rebar_msg_t*`   | Message to inspect|

**Returns:** The byte length, or `0` if `msg` is null.

---

#### `rebar_msg_free`

Free a message previously created with `rebar_msg_create`.

```c
void rebar_msg_free(rebar_msg_t *msg);
```

**Parameters:**

| Name  | Type             | Description      |
|-------|------------------|------------------|
| `msg` | `rebar_msg_t*`   | Message to free  |

**Notes:**
- Passing `NULL` is a safe no-op.
- Each message must be freed exactly once. Double-free is undefined behavior.

---

### Runtime API

#### `rebar_runtime_new`

Create a new Rebar runtime for the given node ID.

```c
rebar_runtime_t *rebar_runtime_new(uint64_t node_id);
```

**Parameters:**

| Name      | Type       | Description                                |
|-----------|------------|--------------------------------------------|
| `node_id` | `uint64_t` | Unique identifier for this runtime node    |

**Returns:** A heap-allocated `rebar_runtime_t*`, or `NULL` if the internal Tokio runtime fails to initialize.

**Notes:**
- The caller owns the returned pointer and must free it with `rebar_runtime_free`.
- Multiple runtimes can coexist with different `node_id` values.
- Each runtime maintains its own independent name registry.

**Example:**

```c
rebar_runtime_t *rt = rebar_runtime_new(1);
if (rt == NULL) {
    fprintf(stderr, "failed to create runtime\n");
    return 1;
}
// ... use runtime ...
rebar_runtime_free(rt);
```

---

#### `rebar_runtime_free`

Free a runtime previously created with `rebar_runtime_new`.

```c
void rebar_runtime_free(rebar_runtime_t *rt);
```

**Parameters:**

| Name | Type               | Description      |
|------|--------------------|------------------|
| `rt` | `rebar_runtime_t*` | Runtime to free  |

**Notes:**
- Passing `NULL` is a safe no-op.
- All spawned processes are stopped when the runtime is dropped.
- The name registry is destroyed along with the runtime.

---

### Process API

#### `rebar_spawn`

Spawn a new lightweight process. The provided callback is invoked in the process context with the process's own PID.

```c
int32_t rebar_spawn(
    rebar_runtime_t *rt,
    void (*callback)(rebar_pid_t),
    rebar_pid_t *pid_out
);
```

**Parameters:**

| Name       | Type                         | Description                                    |
|------------|------------------------------|------------------------------------------------|
| `rt`       | `rebar_runtime_t*`           | Runtime in which to spawn the process          |
| `callback` | `void (*)(rebar_pid_t)`      | Function invoked as the process body           |
| `pid_out`  | `rebar_pid_t*`               | Output: PID of the newly spawned process       |

**Returns:** `REBAR_OK` (0) on success, or a negative error code.

**Errors:**
- `REBAR_ERR_NULL_PTR` if `rt`, `callback`, or `pid_out` is null.

**Notes:**
- This call blocks until the process is spawned (it runs `block_on` internally).
- The callback is invoked asynchronously within the Tokio runtime. It receives the process's own PID as its argument.
- The spawned PID is written to `pid_out` regardless of when the callback executes.

**Example:**

```c
void my_process(rebar_pid_t self) {
    printf("process started: node=%llu local=%llu\n",
           (unsigned long long)self.node_id,
           (unsigned long long)self.local_id);
}

rebar_pid_t pid;
int32_t rc = rebar_spawn(rt, my_process, &pid);
if (rc != 0) {
    fprintf(stderr, "spawn failed: %d\n", rc);
}
```

---

#### `rebar_send`

Send a message to a process identified by PID.

```c
int32_t rebar_send(
    rebar_runtime_t *rt,
    rebar_pid_t dest,
    const rebar_msg_t *msg
);
```

**Parameters:**

| Name   | Type                   | Description                        |
|--------|------------------------|------------------------------------|
| `rt`   | `rebar_runtime_t*`     | Runtime the process belongs to     |
| `dest` | `rebar_pid_t`          | PID of the destination process     |
| `msg`  | `const rebar_msg_t*`   | Message to send                    |

**Returns:** `REBAR_OK` (0) on success, or a negative error code.

**Errors:**
- `REBAR_ERR_NULL_PTR` if `rt` or `msg` is null.
- `REBAR_ERR_SEND_FAILED` if the destination process does not exist or its mailbox is closed.

**Notes:**
- The message data is cloned internally and serialized as a MessagePack binary value (`rmpv::Value::Binary`).
- The caller retains ownership of `msg` and must still free it with `rebar_msg_free`.
- This call blocks until the send completes.

**Example:**

```c
const char *data = "ping";
rebar_msg_t *msg = rebar_msg_create((const uint8_t *)data, 4);

int32_t rc = rebar_send(rt, target_pid, msg);
if (rc == REBAR_ERR_SEND_FAILED) {
    fprintf(stderr, "process unreachable\n");
}

rebar_msg_free(msg);
```

---

### Registry API

#### `rebar_register`

Register a name for a PID in the runtime's local name registry.

```c
int32_t rebar_register(
    rebar_runtime_t *rt,
    const uint8_t *name,
    size_t name_len,
    rebar_pid_t pid
);
```

**Parameters:**

| Name       | Type                | Description                              |
|------------|---------------------|------------------------------------------|
| `rt`       | `rebar_runtime_t*`  | Runtime owning the registry              |
| `name`     | `const uint8_t*`    | Name bytes (must be valid UTF-8)         |
| `name_len` | `size_t`            | Length of the name in bytes              |
| `pid`      | `rebar_pid_t`       | PID to associate with the name           |

**Returns:** `REBAR_OK` (0) on success, or a negative error code.

**Errors:**
- `REBAR_ERR_NULL_PTR` if `rt` or `name` is null.
- `REBAR_ERR_INVALID_NAME` if the name bytes are not valid UTF-8.

**Notes:**
- If the name is already registered, the PID is silently overwritten.
- The name bytes are borrowed for the duration of the call; the caller retains ownership.

**Example:**

```c
rebar_pid_t pid;
rebar_spawn(rt, my_handler, &pid);

const char *name = "worker";
int32_t rc = rebar_register(rt, (const uint8_t *)name, 6, pid);
```

---

#### `rebar_whereis`

Look up a PID by its registered name.

```c
int32_t rebar_whereis(
    rebar_runtime_t *rt,
    const uint8_t *name,
    size_t name_len,
    rebar_pid_t *pid_out
);
```

**Parameters:**

| Name       | Type                | Description                              |
|------------|---------------------|------------------------------------------|
| `rt`       | `rebar_runtime_t*`  | Runtime owning the registry              |
| `name`     | `const uint8_t*`    | Name bytes (must be valid UTF-8)         |
| `name_len` | `size_t`            | Length of the name in bytes              |
| `pid_out`  | `rebar_pid_t*`      | Output: PID associated with the name     |

**Returns:** `REBAR_OK` (0) on success, or a negative error code.

**Errors:**
- `REBAR_ERR_NULL_PTR` if `rt`, `name`, or `pid_out` is null.
- `REBAR_ERR_INVALID_NAME` if the name bytes are not valid UTF-8.
- `REBAR_ERR_NOT_FOUND` if no PID is registered under this name.

**Example:**

```c
rebar_pid_t found;
int32_t rc = rebar_whereis(rt, (const uint8_t *)"worker", 6, &found);
if (rc == REBAR_OK) {
    printf("found: node=%llu local=%llu\n",
           (unsigned long long)found.node_id,
           (unsigned long long)found.local_id);
} else if (rc == REBAR_ERR_NOT_FOUND) {
    printf("name not registered\n");
}
```

---

#### `rebar_send_named`

Send a message to a process by its registered name. This combines a registry lookup and a send in a single call.

```c
int32_t rebar_send_named(
    rebar_runtime_t *rt,
    const uint8_t *name,
    size_t name_len,
    const rebar_msg_t *msg
);
```

**Parameters:**

| Name       | Type                | Description                              |
|------------|---------------------|------------------------------------------|
| `rt`       | `rebar_runtime_t*`  | Runtime owning the registry              |
| `name`     | `const uint8_t*`    | Name bytes (must be valid UTF-8)         |
| `name_len` | `size_t`            | Length of the name in bytes              |
| `msg`      | `const rebar_msg_t*`| Message to send                          |

**Returns:** `REBAR_OK` (0) on success, or a negative error code.

**Errors:**
- `REBAR_ERR_NULL_PTR` if `rt`, `name`, or `msg` is null.
- `REBAR_ERR_INVALID_NAME` if the name bytes are not valid UTF-8.
- `REBAR_ERR_NOT_FOUND` if no PID is registered under this name.
- `REBAR_ERR_SEND_FAILED` if the resolved process is unreachable.

**Notes:**
- The registry lock is held only for the lookup; the send happens after the lock is released.
- The caller retains ownership of `msg`.

**Example:**

```c
rebar_msg_t *msg = rebar_msg_create((const uint8_t *)"work-item", 9);
int32_t rc = rebar_send_named(rt, (const uint8_t *)"worker", 6, msg);
rebar_msg_free(msg);
```

---

## Memory Ownership Rules

### Messages

The caller who calls `rebar_msg_create` owns the returned pointer. It must be freed exactly once with `rebar_msg_free`. Passing a message to `rebar_send` or `rebar_send_named` does **not** transfer ownership -- the data is cloned internally and the caller must still free the message.

### Runtime

The caller who calls `rebar_runtime_new` owns the returned pointer. It must be freed exactly once with `rebar_runtime_free`. Freeing the runtime stops all spawned processes and destroys the name registry.

### Strings (names)

Name pointers (`const uint8_t*`) passed to registry functions are **borrowed** for the duration of the call. Rust copies the bytes internally. The caller retains ownership and may free or reuse the buffer immediately after the call returns.

### Thread Safety

`RebarRuntime` is internally synchronized:

- The Tokio runtime is thread-safe.
- The name registry is protected by a `Mutex`.
- It is safe to call any `rebar_*` function from multiple threads concurrently on the same runtime pointer.

`RebarMsg` is **not** thread-safe. Do not share a single message pointer across threads without external synchronization. In practice, create separate messages per thread or protect access with a lock.

---

## Language Integration Examples

### C Header

A complete header file for C/C++ integration:

```c
/* rebar.h — C bindings for rebar-ffi */
#ifndef REBAR_H
#define REBAR_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Error codes */
#define REBAR_OK              0
#define REBAR_ERR_NULL_PTR   -1
#define REBAR_ERR_SEND_FAILED -2
#define REBAR_ERR_NOT_FOUND  -3
#define REBAR_ERR_INVALID_NAME -4

/* Types */
typedef struct {
    uint64_t node_id;
    uint64_t local_id;
} rebar_pid_t;

typedef struct rebar_msg_t rebar_msg_t;
typedef struct rebar_runtime_t rebar_runtime_t;

/* Callback type for spawned processes */
typedef void (*rebar_process_fn)(rebar_pid_t);

/* Message API */
rebar_msg_t *rebar_msg_create(const uint8_t *data, size_t len);
const uint8_t *rebar_msg_data(const rebar_msg_t *msg);
size_t rebar_msg_len(const rebar_msg_t *msg);
void rebar_msg_free(rebar_msg_t *msg);

/* Runtime API */
rebar_runtime_t *rebar_runtime_new(uint64_t node_id);
void rebar_runtime_free(rebar_runtime_t *rt);

/* Process API */
int32_t rebar_spawn(rebar_runtime_t *rt, rebar_process_fn callback,
                    rebar_pid_t *pid_out);
int32_t rebar_send(rebar_runtime_t *rt, rebar_pid_t dest,
                   const rebar_msg_t *msg);

/* Registry API */
int32_t rebar_register(rebar_runtime_t *rt, const uint8_t *name,
                       size_t name_len, rebar_pid_t pid);
int32_t rebar_whereis(rebar_runtime_t *rt, const uint8_t *name,
                      size_t name_len, rebar_pid_t *pid_out);
int32_t rebar_send_named(rebar_runtime_t *rt, const uint8_t *name,
                         size_t name_len, const rebar_msg_t *msg);

#ifdef __cplusplus
}
#endif

#endif /* REBAR_H */
```

---

### Go via cgo

```go
package main

/*
#cgo LDFLAGS: -lrebar_ffi
#include "rebar.h"
#include <stdlib.h>

// Go cannot pass Go function pointers directly to C.
// Use a C trampoline that calls back into Go via a registered ID.
extern void goProcessCallback(rebar_pid_t pid);
*/
import "C"
import (
	"fmt"
	"unsafe"
)

//export goProcessCallback
func goProcessCallback(pid C.rebar_pid_t) {
	fmt.Printf("process started: node=%d local=%d\n", pid.node_id, pid.local_id)
}

func main() {
	// Create a runtime on node 1
	rt := C.rebar_runtime_new(1)
	if rt == nil {
		panic("failed to create runtime")
	}
	defer C.rebar_runtime_free(rt)

	// Spawn a process
	var pid C.rebar_pid_t
	rc := C.rebar_spawn(rt, C.rebar_process_fn(C.goProcessCallback), &pid)
	if rc != C.REBAR_OK {
		panic(fmt.Sprintf("spawn failed: %d", rc))
	}
	fmt.Printf("spawned pid: node=%d local=%d\n", pid.node_id, pid.local_id)

	// Send a message
	data := []byte("hello from Go")
	msg := C.rebar_msg_create((*C.uint8_t)(unsafe.Pointer(&data[0])), C.size_t(len(data)))
	defer C.rebar_msg_free(msg)

	rc = C.rebar_send(rt, pid, msg)
	if rc != C.REBAR_OK {
		fmt.Printf("send returned: %d\n", rc)
	}

	// Register and look up by name
	name := []byte("greeter")
	rc = C.rebar_register(rt, (*C.uint8_t)(unsafe.Pointer(&name[0])),
		C.size_t(len(name)), pid)
	if rc != C.REBAR_OK {
		panic(fmt.Sprintf("register failed: %d", rc))
	}

	var found C.rebar_pid_t
	rc = C.rebar_whereis(rt, (*C.uint8_t)(unsafe.Pointer(&name[0])),
		C.size_t(len(name)), &found)
	if rc == C.REBAR_OK {
		fmt.Printf("found: node=%d local=%d\n", found.node_id, found.local_id)
	}
}
```

---

### Python via ctypes

```python
import ctypes
import os

# Load the shared library
lib = ctypes.CDLL("librebar_ffi.so")

# --- Type definitions ---

class RebarPid(ctypes.Structure):
    _fields_ = [
        ("node_id", ctypes.c_uint64),
        ("local_id", ctypes.c_uint64),
    ]

# Opaque pointer types
class RebarMsg(ctypes.Structure):
    pass

class RebarRuntime(ctypes.Structure):
    pass

RebarMsgPtr = ctypes.POINTER(RebarMsg)
RebarRuntimePtr = ctypes.POINTER(RebarRuntime)

# Callback type: void (*)(rebar_pid_t)
PROCESS_CALLBACK = ctypes.CFUNCTYPE(None, RebarPid)

# --- Function signatures ---

# Message API
lib.rebar_msg_create.argtypes = [ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t]
lib.rebar_msg_create.restype = RebarMsgPtr

lib.rebar_msg_data.argtypes = [RebarMsgPtr]
lib.rebar_msg_data.restype = ctypes.POINTER(ctypes.c_uint8)

lib.rebar_msg_len.argtypes = [RebarMsgPtr]
lib.rebar_msg_len.restype = ctypes.c_size_t

lib.rebar_msg_free.argtypes = [RebarMsgPtr]
lib.rebar_msg_free.restype = None

# Runtime API
lib.rebar_runtime_new.argtypes = [ctypes.c_uint64]
lib.rebar_runtime_new.restype = RebarRuntimePtr

lib.rebar_runtime_free.argtypes = [RebarRuntimePtr]
lib.rebar_runtime_free.restype = None

# Process API
lib.rebar_spawn.argtypes = [RebarRuntimePtr, PROCESS_CALLBACK,
                            ctypes.POINTER(RebarPid)]
lib.rebar_spawn.restype = ctypes.c_int32

lib.rebar_send.argtypes = [RebarRuntimePtr, RebarPid, RebarMsgPtr]
lib.rebar_send.restype = ctypes.c_int32

# Registry API
lib.rebar_register.argtypes = [RebarRuntimePtr, ctypes.POINTER(ctypes.c_uint8),
                               ctypes.c_size_t, RebarPid]
lib.rebar_register.restype = ctypes.c_int32

lib.rebar_whereis.argtypes = [RebarRuntimePtr, ctypes.POINTER(ctypes.c_uint8),
                              ctypes.c_size_t, ctypes.POINTER(RebarPid)]
lib.rebar_whereis.restype = ctypes.c_int32

lib.rebar_send_named.argtypes = [RebarRuntimePtr, ctypes.POINTER(ctypes.c_uint8),
                                 ctypes.c_size_t, RebarMsgPtr]
lib.rebar_send_named.restype = ctypes.c_int32

# --- Usage ---

REBAR_OK = 0

def main():
    # Create runtime
    rt = lib.rebar_runtime_new(1)
    assert rt, "failed to create runtime"

    try:
        # Define a process callback
        @PROCESS_CALLBACK
        def my_process(pid):
            print(f"process started: node={pid.node_id} local={pid.local_id}")

        # Spawn a process
        pid = RebarPid()
        rc = lib.rebar_spawn(rt, my_process, ctypes.byref(pid))
        assert rc == REBAR_OK, f"spawn failed: {rc}"
        print(f"spawned: node={pid.node_id} local={pid.local_id}")

        # Send a message
        payload = b"hello from Python"
        buf = (ctypes.c_uint8 * len(payload))(*payload)
        msg = lib.rebar_msg_create(buf, len(payload))
        try:
            rc = lib.rebar_send(rt, pid, msg)
            print(f"send returned: {rc}")
        finally:
            lib.rebar_msg_free(msg)

        # Register and look up
        name = b"worker"
        name_buf = (ctypes.c_uint8 * len(name))(*name)
        lib.rebar_register(rt, name_buf, len(name), pid)

        found = RebarPid()
        rc = lib.rebar_whereis(rt, name_buf, len(name), ctypes.byref(found))
        if rc == REBAR_OK:
            print(f"found: node={found.node_id} local={found.local_id}")

    finally:
        lib.rebar_runtime_free(rt)

if __name__ == "__main__":
    main()
```

---

### Ruby via FFI

```ruby
require 'ffi'

module Rebar
  extend FFI::Library
  ffi_lib 'rebar_ffi'

  class Pid < FFI::Struct
    layout :node_id, :uint64,
           :local_id, :uint64
  end

  # Error codes
  OK              = 0
  ERR_NULL_PTR    = -1
  ERR_SEND_FAILED = -2
  ERR_NOT_FOUND   = -3
  ERR_INVALID_NAME = -4

  # Message API
  attach_function :rebar_msg_create, [:pointer, :size_t], :pointer
  attach_function :rebar_msg_data,   [:pointer], :pointer
  attach_function :rebar_msg_len,    [:pointer], :size_t
  attach_function :rebar_msg_free,   [:pointer], :void

  # Runtime API
  attach_function :rebar_runtime_new,  [:uint64], :pointer
  attach_function :rebar_runtime_free, [:pointer], :void

  # Process API
  callback :process_fn, [Pid.by_value], :void
  attach_function :rebar_spawn, [:pointer, :process_fn, :pointer], :int32
  attach_function :rebar_send,  [:pointer, Pid.by_value, :pointer], :int32

  # Registry API
  attach_function :rebar_register,   [:pointer, :pointer, :size_t, Pid.by_value], :int32
  attach_function :rebar_whereis,    [:pointer, :pointer, :size_t, :pointer], :int32
  attach_function :rebar_send_named, [:pointer, :pointer, :size_t, :pointer], :int32
end

# Usage
rt = Rebar.rebar_runtime_new(1)
raise "failed to create runtime" if rt.null?

begin
  callback = Proc.new do |pid|
    puts "process started: node=#{pid[:node_id]} local=#{pid[:local_id]}"
  end

  pid = Rebar::Pid.new
  rc = Rebar.rebar_spawn(rt, callback, pid)
  puts "spawned: node=#{pid[:node_id]} local=#{pid[:local_id]}" if rc == Rebar::OK
ensure
  Rebar.rebar_runtime_free(rt)
end
```

---

### TypeScript via Deno FFI

```typescript
const lib = Deno.dlopen("librebar_ffi.so", {
  rebar_runtime_new: { parameters: ["u64"], result: "pointer" },
  rebar_runtime_free: { parameters: ["pointer"], result: "void" },
  rebar_msg_create: { parameters: ["pointer", "usize"], result: "pointer" },
  rebar_msg_free: { parameters: ["pointer"], result: "void" },
  rebar_spawn: {
    parameters: ["pointer", "function", "pointer"],
    result: "i32",
  },
  rebar_send: { parameters: ["pointer", { struct: ["u64", "u64"] }, "pointer"], result: "i32" },
  rebar_register: {
    parameters: ["pointer", "pointer", "usize", { struct: ["u64", "u64"] }],
    result: "i32",
  },
  rebar_whereis: {
    parameters: ["pointer", "pointer", "usize", "pointer"],
    result: "i32",
  },
});

// Create a runtime
const rt = lib.symbols.rebar_runtime_new(1n);

// Clean up on exit
using _ = { [Symbol.dispose]: () => lib.symbols.rebar_runtime_free(rt) };
```
