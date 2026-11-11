"""Supported stdlib client for the local Mnemosyne content service (Python 3.10+).

No request is retried automatically. An ambiguous mutation is recovered by
resending it with its original idempotency key.
"""
from __future__ import annotations

from dataclasses import dataclass
import json
import math
import os
import socket
import struct
import time
from typing import Any, Callable, Generic, Mapping, TypeVar, Union

JSON = Union[None, bool, int, float, str, list["JSON"], dict[str, "JSON"]]
T = TypeVar("T")
API_VERSION = 2
MAX_REQUEST_BYTES = 128 * 1024 * 1024
MAX_RESPONSE_BYTES = 256 * 1024 * 1024


class ClientError(Exception):
    """Base for local client failures (domain failures use DomainError)."""


class DomainError(ClientError):
    """A complete, authenticated-service error response; code is stable API data."""

    def __init__(self, code: str, message: str, version: int):
        self.code, self.message, self.version = code, message, version
        super().__init__(f"{code}: {message}")


class ProtocolError(ClientError):
    """An invalid or oversized response. Mutation outcome may be unknown."""


class TransportError(ClientError):
    """I/O failed; may_have_been_sent conservatively marks an ambiguous outcome."""

    def __init__(self, phase: str, may_have_been_sent: bool):
        self.phase, self.may_have_been_sent = phase, may_have_been_sent
        super().__init__(f"Local content transport failed during {phase}")


@dataclass(frozen=True)
class Response(Generic[T]):
    version: int
    result: T


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON field")
        result[key] = value
    return result


def _invalid_constant(value: str) -> None:
    raise ValueError("nonfinite JSON number")


def _finite_float(value: str) -> float:
    result = float(value)
    if not math.isfinite(result):
        raise ValueError("nonfinite JSON number")
    return result


class Client:
    """One bounded connection per request; safe to reuse across service restarts.

    The token is issued by the trusted host, never derived from operation JSON.
    This client does not read server credentials or modify grants. Concurrent
    callers use independent sockets. Store the token like a password.
    """

    def __init__(self, socket_path: str | os.PathLike[str], token: str, *,
                 timeout: float = 120.0, max_request_bytes: int = MAX_REQUEST_BYTES,
                 max_response_bytes: int = MAX_RESPONSE_BYTES):
        if not isinstance(token, str) or not token:
            raise ValueError("a nonempty host-issued token is required")
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be positive and finite")
        for value, maximum in [(max_request_bytes, MAX_REQUEST_BYTES),
                               (max_response_bytes, MAX_RESPONSE_BYTES)]:
            if type(value) is not int or not 0 < value <= maximum:
                raise ValueError("frame limit must be a positive integer within protocol bounds")
        self.socket_path = os.fspath(socket_path)
        self._token = token
        self.timeout = timeout
        self.max_request_bytes = max_request_bytes
        self.max_response_bytes = max_response_bytes

    def request(self, operation: Mapping[str, Any], *, idempotency_key: str | None = None,
                version: int = API_VERSION,
                decode: Callable[[JSON], T] | None = None) -> Response[T | JSON]:
        """Execute a tagged operation and return its generic typed response.

        decode optionally validates/converts a result to an application type.
        DomainError preserves service code/message. TransportError and
        ProtocolError must never trigger an automatic mutation replay.
        """
        if not isinstance(operation, Mapping) or not isinstance(operation.get("type"), str):
            raise ValueError("operation requires a string type tag")
        if type(version) is not int or version < 1:
            raise ValueError("version must be a positive integer")
        if idempotency_key is not None and (not isinstance(idempotency_key, str) or not idempotency_key.strip()):
            raise ValueError("idempotency_key must be a nonempty string")
        request: dict[str, Any] = {"version": version, "operation": dict(operation)}
        if idempotency_key is not None:
            request["idempotency_key"] = idempotency_key
        # Reject non-JSON values and nonfinite numbers before connecting.
        body = json.dumps({"token": self._token, "request": request},
                          ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode("utf-8")
        if not 0 < len(body) <= self.max_request_bytes:
            raise ValueError("request exceeds configured frame bound")
        deadline = time.monotonic() + self.timeout
        phase, sent = "connect", False

        def remaining() -> float:
            value = deadline - time.monotonic()
            if value <= 0:
                raise TimeoutError()
            return value

        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                stream.settimeout(remaining())
                stream.connect(self.socket_path)
                phase = "send"
                wire = struct.pack("!I", len(body)) + body
                at = 0
                while at < len(wire):
                    stream.settimeout(remaining())
                    # Conservatively mark ambiguous before entering the syscall.
                    sent = True
                    count = stream.send(memoryview(wire)[at:])
                    if not count:
                        raise ConnectionError()
                    at += count
                phase = "receive"

                def exact(length: int) -> bytes:
                    output = bytearray()
                    while len(output) < length:
                        stream.settimeout(remaining())
                        block = stream.recv(min(length - len(output), 65536))
                        if not block:
                            raise EOFError()
                        output.extend(block)
                    return bytes(output)

                size = struct.unpack("!I", exact(4))[0]
                if not 0 < size <= self.max_response_bytes:
                    raise ProtocolError("Invalid response frame length")
                payload = exact(size)
        except (OSError, EOFError) as exc:
            raise TransportError(phase, sent) from exc
        try:
            reply = json.loads(payload.decode("utf-8"), object_pairs_hook=_unique_object,
                               parse_constant=_invalid_constant, parse_float=_finite_float)
        except (ValueError, UnicodeError, RecursionError) as exc:
            raise ProtocolError("Invalid response JSON") from exc
        if not isinstance(reply, dict) or type(reply.get("version")) is not int:
            raise ProtocolError("Invalid response envelope")
        # Authentication and framing failures use the server's current version.
        if reply["version"] not in (version, API_VERSION):
            raise ProtocolError("Unexpected response version")
        has_result, has_error = "result" in reply, "error" in reply
        if has_result == has_error or set(reply) - {"version", "result", "error"}:
            raise ProtocolError("Response must contain exactly one result or error")
        if has_error:
            error = reply["error"]
            if (not isinstance(error, dict) or set(error) != {"code", "message"}
                    or not isinstance(error["code"], str) or not isinstance(error["message"], str)):
                raise ProtocolError("Invalid domain error")
            raise DomainError(error["code"], error["message"], reply["version"])
        result = reply["result"]
        return Response(reply["version"], decode(result) if decode else result)

    def call(self, operation: Mapping[str, Any], *, idempotency_key: str | None = None,
             version: int = API_VERSION,
             decode: Callable[[JSON], T] | None = None) -> T | JSON:
        """Convenience form of request() returning only the decoded result."""
        return self.request(operation, idempotency_key=idempotency_key,
                            version=version, decode=decode).result


__all__ = ["API_VERSION", "Client", "ClientError", "DomainError", "JSON",
           "MAX_REQUEST_BYTES", "MAX_RESPONSE_BYTES", "ProtocolError", "Response", "TransportError"]
