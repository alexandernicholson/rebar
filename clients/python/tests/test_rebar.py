"""Tests for the Rebar Python client."""

import pytest
from rebar import Actor, Context, NotFoundError, Pid, Runtime, SendError


class TestRuntime:
    def test_create_and_close(self):
        rt = Runtime(node_id=1)
        rt.close()

    def test_context_manager(self):
        with Runtime(node_id=1) as rt:
            pass  # should not raise

    def test_close_idempotent(self):
        rt = Runtime(node_id=1)
        rt.close()
        rt.close()  # should not raise


class TestPid:
    def test_fields(self):
        pid = Pid(node_id=7, local_id=42)
        assert pid.node_id == 7
        assert pid.local_id == 42

    def test_str(self):
        pid = Pid(node_id=1, local_id=5)
        assert str(pid) == "<1.5>"

    def test_frozen(self):
        pid = Pid(node_id=1, local_id=1)
        with pytest.raises(AttributeError):
            pid.node_id = 2  # type: ignore


class TestSend:
    def test_send_invalid_pid(self):
        with Runtime(node_id=1) as rt:
            with pytest.raises(SendError):
                rt.send(Pid(node_id=1, local_id=999999), b"nope")


class TestRegistry:
    def test_register_and_whereis(self):
        with Runtime(node_id=1) as rt:
            pid = Pid(node_id=1, local_id=42)
            rt.register("test_service", pid)
            found = rt.whereis("test_service")
            assert found.node_id == 1
            assert found.local_id == 42

    def test_whereis_not_found(self):
        with Runtime(node_id=1) as rt:
            with pytest.raises(NotFoundError):
                rt.whereis("nonexistent")


class EchoActor(Actor):
    def __init__(self):
        self.started = False

    def handle_message(self, ctx: Context, msg: bytes | None) -> None:
        self.started = True


class TestActor:
    def test_spawn_actor(self):
        with Runtime(node_id=1) as rt:
            actor = EchoActor()
            pid = rt.spawn_actor(actor)
            assert pid.node_id == 1
            assert pid.local_id > 0
