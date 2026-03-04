package rebar

// #include "rebar_ffi.h"
import "C"
import (
	"sync"
)

// Actor defines the interface that all Rebar actors must implement.
// HandleMessage is called each time the process receives a message.
type Actor interface {
	HandleMessage(ctx *Context, msg *Msg)
}

// Context is passed to Actor.HandleMessage and provides the process's
// identity and messaging capabilities.
type Context struct {
	self    Pid
	runtime *Runtime
}

// Self returns this process's PID.
func (c *Context) Self() Pid {
	return c.self
}

// Send sends a message to another process by PID.
func (c *Context) Send(dest Pid, data []byte) error {
	return c.runtime.Send(dest, data)
}

// Register associates a name with a PID in the registry.
func (c *Context) Register(name string, pid Pid) error {
	return c.runtime.Register(name, pid)
}

// Whereis looks up a PID by name.
func (c *Context) Whereis(name string) (Pid, error) {
	return c.runtime.Whereis(name)
}

// SendNamed sends a message to a named process.
func (c *Context) SendNamed(name string, data []byte) error {
	return c.runtime.SendNamed(name, data)
}

// --- Actor registration for C callback trampoline ---

var (
	actorMu      sync.Mutex
	actorMap     = make(map[uint64]actorEntry)
	actorCounter uint64
)

type actorEntry struct {
	actor   Actor
	runtime *Runtime
}

func registerActor(a Actor, r *Runtime) uint64 {
	actorMu.Lock()
	defer actorMu.Unlock()
	actorCounter++
	id := actorCounter
	actorMap[id] = actorEntry{actor: a, runtime: r}
	return id
}

// The active actor ID is set before spawning so the C callback can find it.
// This is safe because rebar_spawn blocks until the callback is invoked.
var activeActorID uint64

//export goRebarProcessCallback
func goRebarProcessCallback(pid C.rebar_pid_t) {
	actorMu.Lock()
	entry, ok := actorMap[activeActorID]
	actorMu.Unlock()
	if !ok {
		return
	}
	goPid := pidFromC(pid)
	ctx := &Context{self: goPid, runtime: entry.runtime}
	entry.actor.HandleMessage(ctx, nil)
}
