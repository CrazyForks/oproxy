#!/usr/bin/env python3
"""Long-lived local protocol fixture origins for docker compose.

Ports:
  18080  HTTP/1.1 origin
  18443  HTTP/2 TLS origin
  18081  WebSocket echo origin
  19090  gRPC TLS echo origin
"""

import asyncio
import gzip
import json
import os
import signal
import struct
import sys
import tempfile
import threading
import time
import zlib
from concurrent import futures
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import grpc
import trustme
import websockets
from grpc_tools import protoc
from hypercorn.asyncio import serve as hypercorn_serve
from hypercorn.config import Config as HypercornConfig


BIND_HOST = os.environ.get("FIXTURE_BIND_HOST", "0.0.0.0")
HTTP1_PORT = int(os.environ.get("FIXTURE_HTTP1_PORT", "18080"))
HTTP2_PORT = int(os.environ.get("FIXTURE_HTTP2_PORT", "18443"))
WS_PORT = int(os.environ.get("FIXTURE_WS_PORT", "18081"))
GRPC_PORT = int(os.environ.get("FIXTURE_GRPC_PORT", "19090"))
CERT_DIR = os.environ.get("FIXTURE_CERT_DIR", "/fixtures/certs")

shutdown_event = threading.Event()


def encode_echo_message(msg: str) -> bytes:
    payload = msg.encode("utf-8")
    return struct.pack("B", (1 << 3) | 2) + struct.pack("B", len(payload)) + payload


class Http1Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        return

    def _send(self, status, content_type, body, extra_headers=None):
        extra_headers = extra_headers or {}
        lower = {k.lower() for k in extra_headers}
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        if "content-length" not in lower and "transfer-encoding" not in lower:
            self.send_header("Content-Length", str(len(body)))
        for key, value in extra_headers.items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/json":
            self._send(200, "application/json", json.dumps({"message": "Hello JSON"}).encode())
        elif self.path == "/gzip":
            self._send(200, "text/plain", gzip.compress(b"Hello gzip"), {"Content-Encoding": "gzip"})
        elif self.path == "/deflate":
            self._send(200, "text/plain", zlib.compress(b"Hello deflate"), {"Content-Encoding": "deflate"})
        elif self.path == "/proto":
            self._send(200, "application/x-protobuf", encode_echo_message("Hello Proto"))
        elif self.path == "/large":
            self._send(200, "application/octet-stream", b"A" * (1024 * 1024))
        else:
            self._send(200, "text/plain", b"Hello HTTP/1.1")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length) if length else b""
        if self.path == "/json-echo":
            try:
                payload = json.loads(body or b"{}")
                payload["received"] = True
                self._send(200, "application/json", json.dumps(payload).encode())
            except Exception:
                self._send(400, "text/plain", b"Bad JSON")
        else:
            self._send(200, self.headers.get("Content-Type", "text/plain"), body)


def start_http1():
    server = ThreadingHTTPServer((BIND_HOST, HTTP1_PORT), Http1Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


async def h2_app(scope, receive, send):
    path = scope["path"]
    method = scope["method"]
    status = 200
    headers = [(b"content-type", b"text/plain")]
    body = b"Hello HTTP/2"

    if method == "GET":
        if path == "/json":
            headers = [(b"content-type", b"application/json")]
            body = json.dumps({"message": "Hello JSON"}).encode()
        elif path == "/gzip":
            headers.append((b"content-encoding", b"gzip"))
            body = gzip.compress(b"Hello gzip")
        elif path == "/deflate":
            headers.append((b"content-encoding", b"deflate"))
            body = zlib.compress(b"Hello deflate")
        elif path == "/proto":
            headers = [(b"content-type", b"application/x-protobuf")]
            body = encode_echo_message("Hello Proto")
        elif path == "/large":
            headers = [(b"content-type", b"application/octet-stream")]
            body = b"A" * (1024 * 1024)
    elif method == "POST":
        chunks = []
        more = True
        while more:
            message = await receive()
            chunks.append(message.get("body", b""))
            more = message.get("more_body", False)
        body = b"".join(chunks)
    else:
        status = 405
        body = b"Method Not Allowed"

    await send({"type": "http.response.start", "status": status, "headers": headers})
    await send({"type": "http.response.body", "body": body, "more_body": False})


def cert_hosts():
    return [
        "127.0.0.1",
        "localhost",
        "protocol-fixtures",
        "fixtures",
        "h1.local",
        "h2.local",
        "ws.local",
        "grpc.local",
    ]


def make_cert(prefix):
    os.makedirs(CERT_DIR, exist_ok=True)
    ca = trustme.CA()
    cert = ca.issue_cert(*cert_hosts())
    cert_path = os.path.join(CERT_DIR, f"{prefix}.crt")
    key_path = os.path.join(CERT_DIR, f"{prefix}.key")
    ca_path = os.path.join(CERT_DIR, f"{prefix}-ca.pem")
    cert.cert_chain_pems[0].write_to_path(cert_path)
    cert.private_key_pem.write_to_path(key_path)
    ca.cert_pem.write_to_path(ca_path)
    return ca, cert_path, key_path, ca_path


async def websocket_echo(websocket):
    async for message in websocket:
        await websocket.send(message)


def ensure_grpc_modules():
    gen_dir = os.path.join(tempfile.gettempdir(), "oproxy_fixture_grpc")
    os.makedirs(gen_dir, exist_ok=True)
    proto_path = os.path.join(gen_dir, "echo.proto")
    with open(proto_path, "w", encoding="utf-8") as f:
        f.write(
            """
syntax = "proto3";
package echo;

service EchoService {
  rpc UnaryEcho (EchoRequest) returns (EchoResponse);
  rpc ServerStreamingEcho (EchoRequest) returns (stream EchoResponse);
  rpc ClientStreamingEcho (stream EchoRequest) returns (EchoResponse);
  rpc BidirectionalStreamingEcho (stream EchoRequest) returns (stream EchoResponse);
}

message EchoRequest { string message = 1; }
message EchoResponse { string message = 1; }
"""
        )
    pb2 = os.path.join(gen_dir, "echo_pb2.py")
    if not os.path.exists(pb2):
        result = protoc.main(
            [
                "grpc_tools.protoc",
                "-I",
                gen_dir,
                "--python_out=" + gen_dir,
                "--grpc_python_out=" + gen_dir,
                proto_path,
            ]
        )
        if result != 0:
            raise RuntimeError("grpc proto generation failed")
    if gen_dir not in sys.path:
        sys.path.insert(0, gen_dir)
    import echo_pb2
    import echo_pb2_grpc

    return echo_pb2, echo_pb2_grpc


def start_grpc():
    echo_pb2, echo_pb2_grpc = ensure_grpc_modules()
    ca, cert_path, key_path, _ = make_cert("grpc")

    class EchoServicer(echo_pb2_grpc.EchoServiceServicer):
        def UnaryEcho(self, request, context):
            return echo_pb2.EchoResponse(message=request.message)

        def ServerStreamingEcho(self, request, context):
            for _ in range(3):
                yield echo_pb2.EchoResponse(message=request.message)

        def ClientStreamingEcho(self, request_iterator, context):
            return echo_pb2.EchoResponse(message=",".join(req.message for req in request_iterator))

        def BidirectionalStreamingEcho(self, request_iterator, context):
            for req in request_iterator:
                yield echo_pb2.EchoResponse(message=req.message)

    with open(cert_path, "rb") as cert_file, open(key_path, "rb") as key_file:
        credentials = grpc.ssl_server_credentials([(key_file.read(), cert_file.read())])

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
    echo_pb2_grpc.add_EchoServiceServicer_to_server(EchoServicer(), server)
    server.add_secure_port(f"{BIND_HOST}:{GRPC_PORT}", credentials)
    server.start()
    return server


async def main():
    def handle_signal(*_):
        shutdown_event.set()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    http1 = start_http1()
    _, h2_cert, h2_key, _ = make_cert("http2")
    grpc_server = start_grpc()

    h2_config = HypercornConfig()
    h2_config.bind = [f"{BIND_HOST}:{HTTP2_PORT}"]
    h2_config.certfile = h2_cert
    h2_config.keyfile = h2_key
    h2_config.alpn_protocols = ["h2"]
    h2_config.accesslog = None
    h2_config.errorlog = "-"

    async def h2_shutdown():
        while not shutdown_event.is_set():
            await asyncio.sleep(0.1)

    ws_server = await websockets.serve(websocket_echo, BIND_HOST, WS_PORT)
    h2_task = asyncio.create_task(hypercorn_serve(h2_app, h2_config, shutdown_trigger=h2_shutdown))

    print(
        "oproxy protocol fixtures ready: "
        f"h1={HTTP1_PORT}, h2={HTTP2_PORT}, ws={WS_PORT}, grpc={GRPC_PORT}, certs={CERT_DIR}",
        flush=True,
    )

    while not shutdown_event.is_set():
        await asyncio.sleep(0.2)

    ws_server.close()
    await ws_server.wait_closed()
    http1.shutdown()
    grpc_server.stop(0)
    await h2_task


if __name__ == "__main__":
    asyncio.run(main())
