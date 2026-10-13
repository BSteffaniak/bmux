#!/usr/bin/env python3
"""Small bounded, synchronous Chrome DevTools client; Python standard library only.

No CDP payloads or browser console messages are logged: navigation and network
payloads can contain private bootstrap/controller capabilities.
"""
import base64
import hashlib
import json
import os
import socket
import struct
import time
import urllib.parse


class CDPError(RuntimeError):
    pass


class CDP:
    MAX_MESSAGE = 32 * 1024 * 1024

    def __init__(self, url, timeout=10):
        parts = urllib.parse.urlsplit(url)
        if parts.scheme != "ws" or parts.hostname not in ("127.0.0.1", "localhost"):
            raise CDPError("CDP must use a loopback websocket")
        self.socket = socket.create_connection((parts.hostname, parts.port), timeout)
        self.timeout = timeout
        self.sequence = 0
        self.pending = bytearray()
        key = base64.b64encode(os.urandom(16)).decode()
        path = parts.path + ("?" + parts.query if parts.query else "")
        request = (f"GET {path} HTTP/1.1\r\nHost: {parts.netloc}\r\n"
                   "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                   f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        try:
            self.socket.sendall(request.encode())
            header = bytearray()
            while b"\r\n\r\n" not in header:
                chunk = self.socket.recv(4096)
                if not chunk or len(header) + len(chunk) > 16384:
                    raise CDPError("invalid websocket upgrade")
                header.extend(chunk)
            raw, remaining = bytes(header).split(b"\r\n\r\n", 1)
            lines = raw.decode("ascii").split("\r\n")
            headers = {name.lower(): value for name, value in
                       (line.split(":", 1) for line in lines[1:] if ":" in line)}
            expected = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            if not lines[0].startswith("HTTP/1.1 101 ") or headers.get(
                    "sec-websocket-accept", "").strip() != expected:
                raise CDPError("websocket upgrade rejected")
            self.pending.extend(remaining)
        except BaseException:
            self.socket.close()
            raise

    def _exact(self, length):
        while len(self.pending) < length:
            chunk = self.socket.recv(min(65536, length - len(self.pending)))
            if not chunk:
                raise CDPError("CDP disconnected")
            self.pending.extend(chunk)
        result = bytes(self.pending[:length])
        del self.pending[:length]
        return result

    def _send(self, opcode, payload):
        length = len(payload)
        if length > self.MAX_MESSAGE:
            raise CDPError("CDP payload exceeds bound")
        prefix = bytes([0x80 | opcode])
        if length < 126:
            prefix += bytes([0x80 | length])
        elif length < 65536:
            prefix += bytes([0x80 | 126]) + struct.pack("!H", length)
        else:
            prefix += bytes([0x80 | 127]) + struct.pack("!Q", length)
        mask = os.urandom(4)
        self.socket.sendall(prefix + mask + bytes(value ^ mask[i % 4]
                                                 for i, value in enumerate(payload)))

    def _message(self):
        fragments = bytearray()
        started = False
        while True:
            first, second = self._exact(2)
            opcode = first & 15
            length = second & 127
            if first & 0x70 or second & 0x80:
                raise CDPError("unsupported websocket frame")
            if length == 126:
                length = struct.unpack("!H", self._exact(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self._exact(8))[0]
            if length > self.MAX_MESSAGE or len(fragments) + length > self.MAX_MESSAGE:
                raise CDPError("CDP message exceeds bound")
            payload = self._exact(length)
            if opcode == 8:
                raise CDPError("CDP websocket closed")
            if opcode in (9, 10):
                if not first & 0x80 or length > 125:
                    raise CDPError("invalid websocket control frame")
                if opcode == 9:
                    self._send(10, payload)
                continue
            if opcode == 1 and not started:
                started = True
            elif opcode != 0 or not started:
                raise CDPError("unsupported websocket message")
            fragments.extend(payload)
            if first & 0x80:
                return json.loads(fragments)

    def call(self, method, params=None):
        self.sequence += 1
        deadline = time.monotonic() + self.timeout
        self._send(1, json.dumps({"id": self.sequence, "method": method,
                                 "params": params or {}}).encode())
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise CDPError("CDP command timed out: " + method)
                self.socket.settimeout(remaining)
                message = self._message()
                if message.get("id") != self.sequence:
                    continue  # Events may include credentials; never print them.
                if "error" in message:
                    raise CDPError("CDP command failed: " + method)
                return message.get("result", {})
        except (OSError, ValueError) as error:
            raise CDPError("CDP transport failed: " + method) from error

    def evaluate(self, expression):
        result = self.call("Runtime.evaluate", {"expression": expression,
                          "returnByValue": True, "awaitPromise": True})
        if "exceptionDetails" in result:
            raise CDPError("browser evaluation failed (details withheld)")
        return result.get("result", {}).get("value")

    def close(self):
        self.socket.close()
