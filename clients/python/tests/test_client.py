from __future__ import annotations

import json
from pathlib import Path
import socket
import struct
import tempfile
import threading
import time
import unittest

from mnemosyne_client import Client, DomainError, ProtocolError, TransportError
from mnemosyne_client.import_session import ImportSession, capture_directory


class Peer:
    def __init__(self, root, responder):
        self.path = Path(root) / "peer.sock"
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(self.path))
        self.listener.listen()
        self.requests = []
        self.errors = []

        def run():
            try:
                with self.listener.accept()[0] as stream:
                    stream.settimeout(1)
                    def exact(n):
                        data = bytearray()
                        while len(data) < n:
                            part = stream.recv(n - len(data))
                            if not part:
                                raise EOFError()
                            data.extend(part)
                        return bytes(data)
                    length = struct.unpack("!I", exact(4))[0]
                    self.requests.append(json.loads(exact(length)))
                    responder(stream)
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception as exc:
                self.errors.append(exc)
            finally:
                self.listener.close()
        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def join(self):
        self.thread.join(2)
        if self.thread.is_alive() or self.errors:
            raise AssertionError(f"fake peer failed: {self.errors}")


def send(reply):
    body = json.dumps(reply).encode()
    return lambda stream: stream.sendall(struct.pack("!I", len(body)) + body)


class ClientTests(unittest.TestCase):
    def test_fragmented_unicode_typed_result_and_identity_separation(self):
        with tempfile.TemporaryDirectory() as root:
            body = json.dumps({"version": 2, "result": {"content": "café"}}).encode()
            def fragmented(stream):
                for part in [struct.pack("!I", len(body))[:2], struct.pack("!I", len(body))[2:], body[:9], body[9:]]:
                    stream.sendall(part)
                    time.sleep(.005)
            peer = Peer(root, fragmented)
            result = Client(peer.path, "private-token").request({"type": "read", "path": "café.md"}, decode=lambda x: x["content"])
            peer.join()
            self.assertEqual((result.version, result.result), (2, "café"))
            self.assertEqual(peer.requests[0], {"token": "private-token", "request": {"version": 2, "operation": {"type": "read", "path": "café.md"}}})

    def test_domain_errors_preserve_code_and_do_not_retry(self):
        with tempfile.TemporaryDirectory() as root:
            peer = Peer(root, send({"version": 2, "error": {"code": "idempotency_conflict", "message": "Different operation"}}))
            with self.assertRaises(DomainError) as caught:
                Client(peer.path, "token").call({"type": "edit"}, idempotency_key="fixed")
            peer.join()
            self.assertEqual(caught.exception.code, "idempotency_conflict")
            self.assertEqual(len(peer.requests), 1)

    def test_incomplete_response_is_ambiguous_and_not_retried(self):
        with tempfile.TemporaryDirectory() as root:
            peer = Peer(root, lambda s: s.sendall(struct.pack("!I", 100) + b"{}"))
            with self.assertRaises(TransportError) as caught:
                Client(peer.path, "token").call({"type": "edit"}, idempotency_key="fixed")
            peer.join()
            self.assertTrue(caught.exception.may_have_been_sent)
            self.assertEqual(caught.exception.phase, "receive")

    def test_connect_failure_is_not_sent(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(TransportError) as caught:
                Client(Path(root) / "missing", "token").call({"type": "head"})
            self.assertFalse(caught.exception.may_have_been_sent)

    def test_total_deadline_bounds_slow_peer(self):
        with tempfile.TemporaryDirectory() as root:
            def slow(stream):
                for value in struct.pack("!I", 10) + b"0123456789":
                    stream.sendall(bytes([value]))
                    time.sleep(.025)
            peer = Peer(root, slow)
            start = time.monotonic()
            with self.assertRaises(TransportError):
                Client(peer.path, "token", timeout=.08).call({"type": "head"})
            self.assertLess(time.monotonic() - start, .3)
            peer.join()

    def test_oversized_and_malformed_responses(self):
        variants = [struct.pack("!I", 0), struct.pack("!I", 999)]
        invalid = [b'{"version":2,"result":null,"error":{}}', b'{"version":2,"result":1,"result":2}',
                   b'{"version":2,"result":NaN}', b'{"version":2,"result":1e999}', b'{"version":true,"result":1}', b'\xff']
        variants += [struct.pack("!I", len(body)) + body for body in invalid]
        for wire in variants:
            with self.subTest(wire=wire), tempfile.TemporaryDirectory() as root:
                peer = Peer(root, lambda s: s.sendall(wire))
                with self.assertRaises(ProtocolError):
                    Client(peer.path, "token", max_response_bytes=100).call({"type": "head"})
                peer.join()

    def test_local_bounds_and_nonfinite_data_never_connect(self):
        client = Client("/missing", "token", max_request_bytes=20)
        for operation in [{"type": "edit", "content": "x" * 100}, {"type": "edit", "n": float("nan")}]:
            with self.assertRaises(ValueError):
                client.call(operation)
        for value in [0, -1, float("inf"), float("nan")]:
            with self.assertRaises(ValueError):
                Client("/missing", "token", timeout=value)


class SessionTests(unittest.TestCase):
    def test_session_cannot_capture_itself_or_modify_source(self):
        with tempfile.TemporaryDirectory() as root:
            source = Path(root).resolve(); source.chmod(0o700)
            (source / "note.md").write_text("source stays unchanged")
            nested = source / "nested"; nested.mkdir(mode=0o700)
            before = sorted(str(p.relative_to(source)) for p in source.rglob("*"))
            for path in [source / "state", nested / "state"]:
                with self.assertRaises(ValueError):
                    ImportSession.create(path, source=source, base_revision="r0")
            self.assertEqual(before, sorted(str(p.relative_to(source)) for p in source.rglob("*")))

    def test_capture_multiple_blobs_and_detect_tampering(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root).resolve(); path.chmod(0o700)
            source = path / "source"; source.mkdir()
            (source / "a.md").write_text("alpha")
            (source / "b.md").write_text("beta")
            session = ImportSession.create(path / "state", source=source, base_revision="r0", batch_size=1)
            files, cursor = session._batch()
            self.assertEqual((files[0]["path"], cursor), ("a.md", 1))
            state = json.loads((path / "state").read_text())
            capture = path / state["capture_directory"]
            (capture / state["files"][0]["blob"]).write_bytes(b"tampered")
            with self.assertRaises(ValueError): session._batch()

    def test_capture_binary_and_reject_symlinks(self):
        with tempfile.TemporaryDirectory() as root:
            source = Path(root).resolve() / "source"
            source.mkdir()
            (source / "empty").mkdir()
            (source / "raw.bin").write_bytes(b"\x00\xff\x80")
            self.assertEqual(capture_directory(source), [{"path": "empty", "directory": True, "bytes": []},
                {"path": "raw.bin", "directory": False, "bytes": [0, 255, 128]}])
            (source / "link").symlink_to(source / "raw.bin")
            with self.assertRaises(ValueError):
                capture_directory(source)


if __name__ == "__main__":
    unittest.main()
