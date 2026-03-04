package rebar

// #include "rebar_ffi.h"
import "C"
import "unsafe"

// Runtime manages a Rebar actor runtime. Create with NewRuntime, close with Close.
type Runtime struct {
	ptr *C.rebar_runtime_t
}

// NewRuntime creates a new Rebar runtime for the given node ID.
func NewRuntime(nodeID uint64) (*Runtime, error) {
	ptr := C.rebar_runtime_new(C.uint64_t(nodeID))
	if ptr == nil {
		return nil, &RebarError{Code: -1, Message: "failed to create runtime"}
	}
	return &Runtime{ptr: ptr}, nil
}

// Close frees the runtime and stops all spawned processes.
// Safe to call multiple times.
func (r *Runtime) Close() {
	if r.ptr != nil {
		C.rebar_runtime_free(r.ptr)
		r.ptr = nil
	}
}

// Send sends a message to a process by PID.
func (r *Runtime) Send(dest Pid, data []byte) error {
	msg := createCMsg(data)
	defer C.rebar_msg_free(msg)
	rc := C.rebar_send(r.ptr, pidToC(dest), msg)
	return checkError(rc)
}

// Register associates a name with a PID in the runtime's registry.
func (r *Runtime) Register(name string, pid Pid) error {
	nameBytes := []byte(name)
	rc := C.rebar_register(
		r.ptr,
		(*C.uint8_t)(unsafe.Pointer(&nameBytes[0])),
		C.size_t(len(nameBytes)),
		pidToC(pid),
	)
	return checkError(rc)
}

// Whereis looks up a PID by its registered name.
func (r *Runtime) Whereis(name string) (Pid, error) {
	nameBytes := []byte(name)
	var pidOut C.rebar_pid_t
	rc := C.rebar_whereis(
		r.ptr,
		(*C.uint8_t)(unsafe.Pointer(&nameBytes[0])),
		C.size_t(len(nameBytes)),
		&pidOut,
	)
	if err := checkError(rc); err != nil {
		return Pid{}, err
	}
	return pidFromC(pidOut), nil
}

// SendNamed sends a message to a process by its registered name.
func (r *Runtime) SendNamed(name string, data []byte) error {
	nameBytes := []byte(name)
	msg := createCMsg(data)
	defer C.rebar_msg_free(msg)
	rc := C.rebar_send_named(
		r.ptr,
		(*C.uint8_t)(unsafe.Pointer(&nameBytes[0])),
		C.size_t(len(nameBytes)),
		msg,
	)
	return checkError(rc)
}

// SpawnActor spawns a new process backed by the given Actor.
// The actor's HandleMessage is called with a nil message on startup
// (as a lifecycle hook), and the process PID is returned.
func (r *Runtime) SpawnActor(actor Actor) (Pid, error) {
	id := registerActor(actor, r)

	actorMu.Lock()
	activeActorID = id
	actorMu.Unlock()

	var pidOut C.rebar_pid_t
	rc := C.rebar_spawn_go(r.ptr, &pidOut)
	if err := checkError(rc); err != nil {
		return Pid{}, err
	}
	return pidFromC(pidOut), nil
}
