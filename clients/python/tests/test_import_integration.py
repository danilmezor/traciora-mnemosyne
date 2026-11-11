"""Import journeys against the real content process and its durable store."""
from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "clients/python"))
from mnemosyne_client import Client, TransportError
from mnemosyne_client.import_session import ImportSession


def clean_env():
    return {key: value for key, value in os.environ.items()
            if not key.startswith(("MNEMOSYNE_CONTENT_", "MNEMOSYNE_INDEX_", "MNEMOSYNE_EMBEDDING_"))}


class Host:
    """Tiny synthetic application: authenticated sessions map to host-derived grants."""

    def __init__(self, binary, root):
        self.binary, self.root = binary, root
        self.store, self.socket = root / "authority", root / "service.sock"
        self.credentials = root / "credentials.json"
        self.process = None
        # Roles live in this host fixture, never in Mnemosyne policy code.
        users = [("owner", "owner"), ("reviewer", "owner"), ("assistant", "assistant")]
        roles = {"owner": ["read", "edit", "propose", "approve", "admin"],
                 "assistant": ["read", "edit", "propose", "approve"]}
        credentials, grants = [], []
        for token, role in users:
            identity = {"tenant_id": "work", "actor_id": token,
                        "kind": "agent" if token == "assistant" else "human"}
            if token == "assistant":
                identity["on_behalf_of"] = "owner"
            credentials.append({"token": token, "identity": identity})
            grants += [{"tenant_id": "work", "actor_id": token, "permission": permission, "scope": ""}
                       for permission in roles[role]]
        self.credentials.write_text(json.dumps({"credentials": credentials}))
        self.credentials.chmod(0o600)
        policy = root / "policy.json"
        policy.write_text(json.dumps({"grants": grants}))
        env = clean_env()
        env["MNEMOSYNE_CONTENT_TOKEN"] = "owner"
        result = subprocess.run([str(binary), "content-init", "--store", str(self.store), "--policy", str(policy)],
                                env=env, text=True, capture_output=True, timeout=120)
        assert result.returncode == 0, result.stdout + result.stderr

    def call(self, operation, key=None, token="owner"):
        return Client(self.socket, token, timeout=120).call(operation, idempotency_key=key)

    def head(self):
        return self.call({"type": "head"})

    def edit(self, changes, key):
        return self.call({"type": "edit", "base_revision": self.head(), "changes": changes}, key)

    def start(self):
        self.log = (self.root / "service.log").open("ab")
        self.process = subprocess.Popen([str(self.binary), "content-serve", "--store", str(self.store),
            "--credentials", str(self.credentials), "--socket", str(self.socket)], env=clean_env(),
            stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            assert self.process.poll() is None, (self.root / "service.log").read_text()
            try:
                self.head()
                return
            except TransportError:
                time.sleep(.02)
        raise AssertionError("service startup timed out")

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                status = self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
                raise AssertionError("service graceful stop timed out")
            assert status == 0 and not self.socket.exists(), "service failed graceful cleanup"
        if hasattr(self, "log"):
            self.log.close()


class ObservedClient(Client):
    """Real requests, with optional response loss after the engine performed its operation."""

    def __init__(self, host, path, *, token="owner", lose=None):
        super().__init__(host.socket, token, timeout=10)
        self.path, self.lose, self.sent = path, lose, []

    def request(self, operation, **kwargs):
        if operation["type"] == "import_commit":
            pending = json.loads(self.path.read_text())["pending"]
            assert pending["key"] == kwargs["idempotency_key"], "commit key was not durable before send"
            assert pending["preview"] == operation["preview"], "commit differs from the durable preview"
        self.sent.append((json.loads(json.dumps(operation)), kwargs.get("idempotency_key")))
        result = super().request(operation, **kwargs)
        if operation["type"] == self.lose:
            self.lose = None
            raise TransportError("receive", True)
        return result


class ImportIntegrationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="import-journey-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        binary = Path(os.environ.get("MNEMOSYNE_TEST_BINARY", ROOT / "target/release/mnemosyne"))
        if not binary.is_file():
            raise RuntimeError("MNEMOSYNE_TEST_BINARY must name the built real content engine")
        self.host = Host(binary, self.root)
        self.addCleanup(self.host.stop)
        self.host.start()
        self.source = self.root / "source"
        self.source.mkdir()
        (self.source / "note.md").write_text("Captured import words")
        self.path = self.root / "session.json"

    def test_batches_reopen_and_complete_without_redundant_head_requests(self):
        for index in range(64):
            (self.source / f"page-{index:02}.md").write_text(f"Page {index}")
        session = ImportSession.create(self.path, source=self.source, base_revision=self.host.head())
        client = ObservedClient(self.host, self.path)
        revisions = []
        while not session.complete:
            result = session.advance(client, expected_head=session.base_revision)
            revisions.append(result["id"])
            session = ImportSession.open(self.path)
        self.assertEqual(len(set(revisions)), 3)
        self.assertEqual(session.file_paths, frozenset(path.name for path in self.source.iterdir()))
        self.assertEqual(len(session.receipts), 3)
        self.assertEqual(len(self.host.call({"type": "list", "scope": ""})), 65)
        self.assertEqual([op["type"] for op, _ in client.sent], ["import_preview", "import_commit"] * 3)
        self.assertEqual(json.loads(self.path.read_text())["format"], 1)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        sent = len(client.sent)
        self.assertIsNone(session.advance(client, expected_head=session.base_revision))
        self.assertEqual(len(client.sent), sent)

    def test_lost_commit_reply_replays_exact_request_after_restart_and_another_edit(self):
        base = self.host.head()
        session = ImportSession.create(self.path, source=self.source, base_revision=base)
        client = ObservedClient(self.host, self.path, lose="import_commit")
        with self.assertRaises(TransportError):
            session.advance(client, expected_head=base)
        published, original = self.host.head(), client.sent[-1]
        self.assertNotEqual(published, base)
        self.host.stop()
        self.host.start()
        later = self.host.edit([{"type": "put", "path": "other.md", "content": "Another person's edit"}], "other-edit")
        resumed = ImportSession.open(self.path)
        result = resumed.advance(client, expected_head=base)
        self.assertEqual(result["id"], published)
        self.assertEqual(client.sent[-1], original)
        self.assertEqual(self.host.head(), later["id"])
        self.assertEqual(len(resumed.receipts), 1)
        self.assertTrue(resumed.complete)
        self.assertEqual(self.host.call({"type": "read", "path": "note.md"})["content"], "Captured import words")

    def test_lost_preview_reply_reopens_without_a_write_or_lost_source(self):
        base = self.host.head()
        session = ImportSession.create(self.path, source=self.source, base_revision=base)
        client = ObservedClient(self.host, self.path, lose="import_preview")
        with self.assertRaises(TransportError):
            session.advance(client, expected_head=base)
        self.assertEqual(self.host.head(), base)
        (self.source / "note.md").write_text("Changed after capture")
        resumed = ImportSession.open(self.path)
        resumed.advance(client, expected_head=base)
        self.assertTrue(resumed.complete)
        self.assertEqual(self.host.call({"type": "read", "path": "note.md"})["content"], "Captured import words")

    def test_changed_head_refuses_before_import_and_preserves_the_other_edit(self):
        base = self.host.head()
        session = ImportSession.create(self.path, source=self.source, base_revision=base)
        edited = self.host.edit([{"type": "put", "path": "other.md", "content": "Keep this edit"}], "moved-head")
        client = ObservedClient(self.host, self.path)
        with self.assertRaisesRegex(ValueError, "head changed"):
            session.advance(client, expected_head=base)
        self.assertEqual([op["type"] for op, _ in client.sent], ["import_preview"])
        self.assertEqual(self.host.head(), edited["id"])
        self.assertEqual(self.host.call({"type": "list", "scope": ""}), ["other.md"])

    def test_unpersisted_preview_blocks_commit_until_the_durable_session_is_reopened(self):
        base = self.host.head()
        session = ImportSession.create(self.path, source=self.source, base_revision=base)
        durable = self.root / "last-durable.json"
        self.path.rename(durable)
        self.path.mkdir()
        client = ObservedClient(self.host, self.path)
        with self.assertRaises(OSError):
            session.advance(client, expected_head=base)
        self.path.rmdir()
        durable.rename(self.path)
        with self.assertRaisesRegex(ValueError, "reopen durable state"):
            session.advance(client, expected_head=base)
        self.assertEqual(self.host.head(), base)
        self.assertEqual([op["type"] for op, _ in client.sent], ["import_preview"])
        resumed = ImportSession.open(self.path)
        resumed.advance(client, expected_head=base)
        self.assertTrue(resumed.complete)
        self.assertEqual(self.host.call({"type": "read", "path": "note.md"})["content"], "Captured import words")

    def test_proposal_reply_loss_waits_for_real_human_approval_and_application(self):
        base = self.host.head()
        session = ImportSession.create(self.path, source=self.source, base_revision=base, mode="propose")
        client = ObservedClient(self.host, self.path, token="assistant", lose="import_commit")
        with self.assertRaises(TransportError):
            session.advance(client, expected_head=base)
        original = client.sent[-1]
        session = ImportSession.open(self.path)
        proposed = session.advance(client, expected_head=base)
        self.assertEqual(client.sent[-1], original)
        self.assertEqual(self.host.head(), base)
        sent = len(client.sent)
        self.assertEqual(session.advance(client, expected_head=base)["id"], proposed["id"])
        self.assertEqual(len(client.sent), sent)
        with self.assertRaisesRegex(ValueError, "not applied"):
            session.acknowledge_applied(client, expected_revision=base)
        self.host.stop()
        self.host.start()
        self.host.call({"type": "approve", "proposal_id": proposed["id"], "digest": proposed["digest"]}, "approve", token="reviewer")
        applied = self.host.call({"type": "apply", "proposal_id": proposed["id"], "digest": proposed["digest"]}, "apply", token="assistant")
        resumed = ImportSession.open(self.path)
        with self.assertRaisesRegex(ValueError, "not applied"):
            resumed.acknowledge_applied(client, expected_revision=base)
        resumed.acknowledge_applied(client, expected_revision=applied["id"])
        self.assertTrue(resumed.complete)
        self.assertEqual(resumed.base_revision, applied["id"])
        self.assertEqual(len(resumed.receipts), 1)

    def test_message_and_origin_survive_reopen_and_reach_the_real_engine(self):
        for index, options in enumerate(({}, {"message": "Imported notes"},
                                         {"message": "Imported notes", "origin": {"source": "assistant", "model": "example"}})):
            with self.subTest(options=options):
                path = self.root / f"session-{index}.json"
                (self.source / "note.md").write_text(f"Version {index}")
                session = ImportSession.create(path, source=self.source, base_revision=self.host.head(),
                                               conflict_policy="overwrite", **options)
                client = ObservedClient(self.host, path)
                reopened = ImportSession.open(path)
                result = reopened.advance(client, expected_head=session.base_revision)
                committed = client.sent[-1][0]
                self.assertEqual({key: committed[key] for key in options}, options)
                self.assertEqual("message" in committed, "message" in options)
                self.assertTrue(reopened.complete)
                self.assertEqual(self.host.head(), result["id"])
                self.assertEqual(self.host.call({"type": "read", "path": "note.md"})["content"], f"Version {index}")

        for options in ({"message": 5}, {"origin": "assistant"}):
            with self.assertRaises(ValueError):
                ImportSession.create(self.root / "invalid.json", source=self.source,
                                     base_revision=self.host.head(), **options)


if __name__ == "__main__":
    unittest.main()
