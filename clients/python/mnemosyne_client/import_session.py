"""Explicit, durable local import cursor. State contains source bytes: keep private."""
from __future__ import annotations

import hashlib
from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile
from typing import Any, cast
import unicodedata
import uuid

from . import Client, DomainError

MAX_SOURCE_BYTES = 16 * 1024 * 1024
MAX_FILE_BYTES = MAX_SOURCE_BYTES
MAX_STATE_BYTES = 128 * 1024 * 1024


@dataclass(frozen=True)
class _Ready:
    pass


@dataclass(frozen=True)
class _PendingCommit:
    preview: dict[str, Any]
    key: str
    next_cursor: int | None


@dataclass(frozen=True)
class _AwaitingApplication:
    receipt: dict[str, Any]


@dataclass(frozen=True)
class _Complete:
    pass


@dataclass(frozen=True)
class _PersistenceFailed:
    pass


_Phase = _Ready | _PendingCommit | _AwaitingApplication | _Complete | _PersistenceFailed


def capture_directory(source: str | os.PathLike[str], *, _capture_to: Path | None = None) -> list[dict[str, Any]]:
    """Capture a small local corpus; reject symlinks/special files and path races.

    The trusted host owns the source tree during capture. Files are read through
    directory descriptors with O_NOFOLLOW, including every ancestor under root.
    """
    files: list[dict[str, Any]] = []
    total = 0
    if ".." in Path(source).parts:
        raise ValueError("source must not contain parent traversal")
    absolute = Path(os.path.abspath(source))
    for ancestor in [*reversed(absolute.parents), absolute]:
        if ancestor.is_symlink():
            raise ValueError("source ancestors must not be symlinks")
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK

    def visit(descriptor: int, prefix: str) -> None:
        nonlocal total
        for name in sorted(os.listdir(descriptor)):
            path = prefix + name
            if (not name or name in (".", "..") or "\\" in name
                    or any(unicodedata.category(c) == "Cc" for c in name)
                    or len(path.encode("utf-8")) > 1024):
                raise ValueError("invalid import path")
            metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
            if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
                raise ValueError("source contains symlink or special file")
            child = os.open(name, flags, dir_fd=descriptor)
            try:
                opened = os.fstat(child)
                if (metadata.st_dev, metadata.st_ino, metadata.st_mode) != (opened.st_dev, opened.st_ino, opened.st_mode):
                    raise ValueError("source changed during capture")
                directory = stat.S_ISDIR(opened.st_mode)
                entry: dict[str, Any] = {"path": path, "bytes": [], "directory": directory}
                files.append(entry)
                if directory:
                    visit(child, path + "/")
                else:
                    body = bytearray()
                    while True:
                        block = os.read(child, min(65536, MAX_FILE_BYTES - len(body) + 1))
                        if not block:
                            break
                        total += len(block)
                        if len(body) + len(block) > MAX_FILE_BYTES or (_capture_to is None and total > MAX_SOURCE_BYTES):
                            raise ValueError("source byte limit exceeded")
                        body.extend(block)
                    after = os.fstat(child)
                    if (opened.st_size, opened.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
                        raise ValueError("source changed during capture")
                    if _capture_to is None:
                        entry["bytes"] = list(body)
                    else:
                        digest = hashlib.sha256(body).hexdigest()
                        destination = _capture_to / digest
                        if not destination.exists():
                            blob_descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                            with os.fdopen(blob_descriptor, "wb") as output:
                                output.write(body)
                                output.flush()
                                os.fsync(output.fileno())
                        del entry["bytes"]
                        entry.update(blob=digest, size=len(body))
            finally:
                os.close(child)

    descriptor = os.open(source, flags | os.O_DIRECTORY)
    try:
        visit(descriptor, "")
    finally:
        os.close(descriptor)
    return files


class ImportSession:
    """One persisted captured source and explicit cursor, without automatic rebase.

    create()/open() require a private host-owned state directory. advance() makes
    one batch. For agent mode, acknowledge_applied() is required between batches.
    Do not concurrently drive one state file from multiple processes.
    An optional message (and origin) is saved with the session and sent with every
    import_commit: the import revision's message, or a proposal's reason.
    """

    def __init__(self, path: Path, state: dict[str, Any]):
        self.path, self._data = path, dict(state)
        self._phase: _Phase
        match (self._data.pop("pending"), self._data.pop("awaiting_application"), self._data.pop("complete")):
            case (None, None, False):
                self._phase = _Ready()
            case (dict() as pending, None, False):
                self._phase = _PendingCommit(**pending)
            case (None, dict() as receipt, False):
                self._phase = _AwaitingApplication(receipt)
            case (None, None, True):
                self._phase = _Complete()
            case _:
                raise ValueError("inconsistent import session phase")

    @property
    def complete(self) -> bool:
        return isinstance(self._phase, _Complete)

    @property
    def base_revision(self) -> str:
        return self._data["base_revision"]

    @property
    def file_paths(self) -> frozenset[str]:
        return frozenset(entry["path"] for entry in self._data["files"] if not entry["directory"])

    @property
    def receipts(self) -> tuple[dict[str, Any], ...]:
        return tuple(self._data["receipts"])

    @staticmethod
    def _private(path: Path) -> None:
        parent = path.parent.lstat()
        if not stat.S_ISDIR(parent.st_mode) or parent.st_mode & 0o077 or parent.st_uid != os.getuid():
            raise ValueError("session parent must be a private host-owned directory")
        if path.is_symlink():
            raise ValueError("session must not be a symlink")

    @classmethod
    def create(cls, path: str | os.PathLike[str], *, source: str | os.PathLike[str],
               base_revision: str, mode: str = "edit", batch_size: int = 32,
               conflict_policy: str = "error", message: str | None = None,
               origin: dict[str, Any] | None = None) -> "ImportSession":
        target = Path(path)
        cls._private(target)
        resolved_source, state_parent = Path(source).resolve(), target.parent.resolve()
        if state_parent == resolved_source or resolved_source in state_parent.parents:
            raise ValueError("session state and capture must be outside the source tree")
        if target.exists():
            raise FileExistsError(target)
        if (not base_revision or mode not in ("edit", "propose")
                or type(batch_size) is not int or not 1 <= batch_size <= 32
                or conflict_policy not in ("error", "skip", "overwrite")
                or not isinstance(message, (str, type(None)))
                or not isinstance(origin, (dict, type(None)))):
            raise ValueError("invalid import session options")
        session_id = uuid.uuid4().hex
        capture = target.parent / (target.name + ".capture-" + session_id)
        capture.mkdir(mode=0o700)
        try:
            files = capture_directory(source, _capture_to=capture)
            capture_descriptor = os.open(capture, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(capture_descriptor)
            finally:
                os.close(capture_descriptor)
        except BaseException:
            shutil.rmtree(capture)
            raise
        state = {"format": 1, "session_id": session_id, "capture_directory": capture.name, "files": files,
                 "base_revision": base_revision, "offset": 0, "batch_size": batch_size,
                 "conflict_policy": conflict_policy, "mode": mode, "pending": None,
                 "awaiting_application": None, "receipts": [], "complete": False}
        # Absent unless given, so a session without words keeps the format-1 request.
        if message is not None:
            state["message"] = message
        if origin is not None:
            state["origin"] = origin
        session = cls(target, state)
        # Claim the initial name exclusively before the first atomic save.
        descriptor = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        os.close(descriptor)
        session._save()
        return session

    @classmethod
    def open(cls, path: str | os.PathLike[str]) -> "ImportSession":
        target = Path(path)
        cls._private(target)
        descriptor = os.open(target, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(descriptor, "rb") as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_uid != os.getuid():
                raise ValueError("session must be a private host-owned regular file")
            data = stream.read(MAX_STATE_BYTES + 1)
        if len(data) > MAX_STATE_BYTES:
            raise ValueError("session state exceeds bound")
        state = json.loads(data)
        if not isinstance(state, dict) or state.get("format") != 1:
            raise ValueError("unsupported session format")
        return cls(target, state)

    def _require_writable(self) -> None:
        if isinstance(self._phase, _PersistenceFailed):
            raise ValueError("session persistence failed; reopen durable state before continuing")

    def _save(self) -> None:
        try:
            self._write_state()
        except BaseException:
            self._phase = _PersistenceFailed()
            raise

    def _write_state(self) -> None:
        self._private(self.path)
        state = {**self._data,
                 "pending": vars(self._phase) if isinstance(self._phase, _PendingCommit) else None,
                 "awaiting_application": self._phase.receipt if isinstance(self._phase, _AwaitingApplication) else None,
                 "complete": self.complete}
        data = json.dumps(state, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode()
        if len(data) > MAX_STATE_BYTES:
            raise ValueError("session state exceeds bound")
        descriptor, name = tempfile.mkstemp(prefix=".import-", dir=self.path.parent)
        try:
            with os.fdopen(descriptor, "wb") as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(name, self.path)
            parent = os.open(self.path.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(parent)
            finally:
                os.close(parent)
        finally:
            if os.path.exists(name):
                os.unlink(name)

    def advance(self, client: Client, *, expected_head: str) -> Any:
        """Publish one human edit / agent proposal, or retry a saved exact commit.

        expected_head must equal the saved base. With an unresolved saved commit,
        replay its exact request/key before making any head-based inference.
        """
        self._require_writable()
        state = self._data
        if expected_head != self.base_revision:
            raise ValueError("expected head differs from saved session base")
        match self._phase:
            case _Complete():
                return None
            case _AwaitingApplication(receipt):
                return receipt["result"]
            case _Ready():
                files, next_cursor = self._batch()
                try:
                    preview = client.call({"type": "import_preview", "base_revision": expected_head,
                        "files": files, "offset": state["offset"], "next_offset": next_cursor,
                        "conflict_policy": state["conflict_policy"]})
                except DomainError as failure:
                    if failure.code != "stale_base":
                        raise
                    raise ValueError("head changed; inspect conflict and start an explicit new session") from failure
                self._phase = _PendingCommit(preview, f"import-{state['session_id']}-{state['offset']}", next_cursor)
                self._save()
        pending = cast(_PendingCommit, self._phase)
        commit = {"type": "import_commit", "preview": pending.preview, "mode": state["mode"]}
        for words in ("message", "origin"):
            if state.get(words) is not None:
                commit[words] = state[words]
        result = client.call(commit, idempotency_key=pending.key)
        receipt = {"key": pending.key, "next_cursor": pending.next_cursor,
                   "preview": {k: v for k, v in pending.preview.items() if k != "changes"},
                   "result": {k: v for k, v in result.items() if k in ("id", "digest", "state", "revision_id", "next_offset", "skipped")}}
        if result.get("skipped"):
            self._finish(receipt, result["revision_id"])
        elif state["mode"] == "propose":
            self._phase = _AwaitingApplication(receipt)
            self._save()
        else:
            self._finish(receipt, result["id"])
        return result

    def _finish(self, receipt: dict[str, Any], revision: str) -> None:
        state = self._data
        state["receipts"].append(receipt)
        cursor = receipt["next_cursor"]
        self._phase = _Complete() if cursor is None else _Ready()
        state["offset"] = cursor if cursor is not None else len(state["files"])
        state["base_revision"] = revision
        self._save()

    def _batch(self) -> tuple[list[dict[str, Any]], int | None]:
        """Materialize only one bounded wire batch from the durable source manifest."""
        state = self._data
        capture_name = state["capture_directory"]
        if Path(capture_name).name != capture_name:
            raise ValueError("invalid capture directory")
        capture = self.path.parent / capture_name
        metadata = capture.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_uid != os.getuid():
            raise ValueError("capture must be a private host-owned directory")
        files, size = [], 0
        cursor = state["offset"]
        for entry in state["files"][cursor:cursor + state["batch_size"]]:
            length = entry.get("size", 0)
            if files and size + length > MAX_SOURCE_BYTES:
                break
            if entry["directory"]:
                files.append({"path": entry["path"], "directory": True, "bytes": []})
            else:
                digest = entry["blob"]
                if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
                    raise ValueError("invalid capture digest")
                descriptor = os.open(capture / digest, os.O_RDONLY | os.O_NOFOLLOW)
                with os.fdopen(descriptor, "rb") as stream:
                    body = stream.read(MAX_SOURCE_BYTES + 1)
                if len(body) != length or hashlib.sha256(body).hexdigest() != digest:
                    raise ValueError("captured source hash mismatch")
                files.append({"path": entry["path"], "directory": False, "bytes": list(body)})
            cursor += 1
            size += length
        return files, (cursor if cursor < len(state["files"]) else None)

    def acknowledge_applied(self, client: Client, *, expected_revision: str) -> None:
        """Advance an agent cursor only after externally approved/applied proposal."""
        self._require_writable()
        if not isinstance(self._phase, _AwaitingApplication):
            raise ValueError("no proposal is awaiting application")
        receipt = self._phase.receipt
        review = client.call({"type": "review", "proposal_id": receipt["result"]["id"]})
        if (review["state"] != "applied" or review["revision_id"] != expected_revision
                or client.call({"type": "head"}) != expected_revision):
            raise ValueError("proposal is not applied at the explicitly expected current revision")
        self._finish(receipt, expected_revision)
