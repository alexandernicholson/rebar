import { assertEquals, assertThrows } from "https://deno.land/std/assert/mod.ts";
import { Actor, Context, NotFoundError, Pid, Runtime, SendError } from "../src/mod.ts";

Deno.test("Runtime - create and close", () => {
  const rt = new Runtime(1n);
  rt.close();
});

Deno.test("Runtime - close idempotent", () => {
  const rt = new Runtime(1n);
  rt.close();
  rt.close(); // should not throw
});

Deno.test("Runtime - using disposable", () => {
  using rt = new Runtime(1n);
  // should dispose automatically
});

Deno.test("Pid - fields", () => {
  const pid = new Pid(7n, 42n);
  assertEquals(pid.nodeId, 7n);
  assertEquals(pid.localId, 42n);
});

Deno.test("Pid - toString", () => {
  const pid = new Pid(1n, 5n);
  assertEquals(pid.toString(), "<1.5>");
});

Deno.test("Send - invalid pid", () => {
  using rt = new Runtime(1n);
  assertThrows(
    () => rt.send(new Pid(1n, 999999n), new Uint8Array([1, 2, 3])),
    SendError,
  );
});

Deno.test("Registry - register and whereis", () => {
  using rt = new Runtime(1n);
  const pid = new Pid(1n, 42n);
  rt.register("test_service", pid);
  const found = rt.whereis("test_service");
  assertEquals(found.nodeId, 1n);
  assertEquals(found.localId, 42n);
});

Deno.test("Registry - whereis not found", () => {
  using rt = new Runtime(1n);
  assertThrows(() => rt.whereis("nonexistent"), NotFoundError);
});

class TestActor extends Actor {
  started = false;
  handleMessage(ctx: Context, msg: Uint8Array | null): void {
    this.started = true;
  }
}

Deno.test("Actor - spawn", async () => {
  const rt = new Runtime(1n);
  try {
    const actor = new TestActor();
    const pid = rt.spawnActor(actor);
    assertEquals(pid.nodeId, 1n);
    // Allow the spawned tokio task to call the callback before the runtime
    // is dropped (the tokio runtime blocks on drop waiting for all tasks).
    await new Promise((r) => setTimeout(r, 50));
  } finally {
    rt.close();
  }
});
