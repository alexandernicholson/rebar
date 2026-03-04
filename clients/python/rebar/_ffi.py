"""Private FFI bindings for librebar_ffi. Do not use directly."""

import ctypes
import ctypes.util
import os
import sys
from pathlib import Path


class RebarPid(ctypes.Structure):
    _fields_ = [("node_id", ctypes.c_uint64), ("local_id", ctypes.c_uint64)]


# Callback type: void (*)(rebar_pid_t)
PROCESS_CALLBACK = ctypes.CFUNCTYPE(None, RebarPid)


def _find_library() -> str:
    """Find librebar_ffi, checking common locations."""
    # Check environment variable first
    env_path = os.environ.get("REBAR_LIB_PATH")
    if env_path and os.path.isfile(env_path):
        return env_path

    # Platform-specific names
    if sys.platform == "darwin":
        name = "librebar_ffi.dylib"
    elif sys.platform == "win32":
        name = "rebar_ffi.dll"
    else:
        name = "librebar_ffi.so"

    # Check LD_LIBRARY_PATH / standard paths
    found = ctypes.util.find_library("rebar_ffi")
    if found:
        return found

    # Check relative to this file (for development)
    repo_root = Path(__file__).resolve().parent.parent.parent.parent
    release_path = repo_root / "target" / "release" / name
    if release_path.is_file():
        return str(release_path)

    raise OSError(
        f"Cannot find {name}. Build it with: cargo build --release -p rebar-ffi\n"
        f"Then set LD_LIBRARY_PATH or REBAR_LIB_PATH."
    )


_lib = ctypes.CDLL(_find_library())

# --- Message API ---
_lib.rebar_msg_create.argtypes = [ctypes.c_char_p, ctypes.c_size_t]
_lib.rebar_msg_create.restype = ctypes.c_void_p

_lib.rebar_msg_data.argtypes = [ctypes.c_void_p]
_lib.rebar_msg_data.restype = ctypes.POINTER(ctypes.c_uint8)

_lib.rebar_msg_len.argtypes = [ctypes.c_void_p]
_lib.rebar_msg_len.restype = ctypes.c_size_t

_lib.rebar_msg_free.argtypes = [ctypes.c_void_p]
_lib.rebar_msg_free.restype = None

# --- Runtime API ---
_lib.rebar_runtime_new.argtypes = [ctypes.c_uint64]
_lib.rebar_runtime_new.restype = ctypes.c_void_p

_lib.rebar_runtime_free.argtypes = [ctypes.c_void_p]
_lib.rebar_runtime_free.restype = None

# --- Process API ---
_lib.rebar_spawn.argtypes = [ctypes.c_void_p, PROCESS_CALLBACK, ctypes.POINTER(RebarPid)]
_lib.rebar_spawn.restype = ctypes.c_int32

_lib.rebar_send.argtypes = [ctypes.c_void_p, RebarPid, ctypes.c_void_p]
_lib.rebar_send.restype = ctypes.c_int32

# --- Registry API ---
_lib.rebar_register.argtypes = [
    ctypes.c_void_p, ctypes.c_char_p, ctypes.c_size_t, RebarPid
]
_lib.rebar_register.restype = ctypes.c_int32

_lib.rebar_whereis.argtypes = [
    ctypes.c_void_p, ctypes.c_char_p, ctypes.c_size_t, ctypes.POINTER(RebarPid)
]
_lib.rebar_whereis.restype = ctypes.c_int32

_lib.rebar_send_named.argtypes = [
    ctypes.c_void_p, ctypes.c_char_p, ctypes.c_size_t, ctypes.c_void_p
]
_lib.rebar_send_named.restype = ctypes.c_int32
