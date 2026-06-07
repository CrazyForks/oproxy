#!/usr/bin/env python3
"""
Comprehensive Proxy Tester – auto‑venv, installs deps, runs all tests, cleans up.
Tests HTTP/1.1, HTTP/2, WebSocket, gRPC, SOCKS5 TCP/UDP.

Note: HTTP/2 tests connect to a local test server via the proxy.
The test server uses a self‑signed certificate, so SSL verification is disabled
for the test server only – your proxy's certificate is not involved in these tests.

Usage:
  python proxy_tester.py --http-proxy http://localhost:8080 --socks-proxy socks5://localhost:1080 [--verbose]
"""

import os
import sys
import subprocess
import tempfile
import shutil
import signal
import atexit
import time
import argparse

# ----------------------------------------------------------------------
# Check if we are running inside the temporary venv (flag --run-tests)
# ----------------------------------------------------------------------
RUN_TESTS_FLAG = '--run-tests'

if RUN_TESTS_FLAG not in sys.argv:
    # --------------- BOOTSTRAP: create venv, install deps, re-launch ---------------
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument('--http-proxy')
    parser.add_argument('--socks-proxy')
    parser.add_argument('--target-host', default='127.0.0.1')
    parser.add_argument('--timeout', type=int, default=10)
    parser.add_argument('--verbose', action='store_true')
    args, unknown = parser.parse_known_args()

    temp_dir = tempfile.mkdtemp(prefix='proxy_tester_venv_')
    venv_dir = os.path.join(temp_dir, 'venv')

    print(f"Creating temporary virtual environment at {venv_dir} ...")
    subprocess.run([sys.executable, '-m', 'venv', venv_dir], check=True)

    if os.name == 'nt':
        pip_path = os.path.join(venv_dir, 'Scripts', 'pip.exe')
        python_path = os.path.join(venv_dir, 'Scripts', 'python.exe')
    else:
        pip_path = os.path.join(venv_dir, 'bin', 'pip')
        python_path = os.path.join(venv_dir, 'bin', 'python')

    packages = [
        'requests',
        'httpx[http2]',
        'websockets',
        'grpcio',
        'grpcio-tools',
        'PySocks',
        'hypercorn',
        'brotli',
        'zstandard',
        'certifi',
        'trustme',
    ]
    print("Installing required packages (this may take a moment) ...")
    subprocess.run([pip_path, 'install', *packages], check=True)

    cmd = [
        python_path,
        os.path.abspath(__file__),
        RUN_TESTS_FLAG,
    ]
    for arg in sys.argv[1:]:
        if arg != RUN_TESTS_FLAG:
            cmd.append(arg)

    def cleanup():
        print("\nCleaning up temporary venv...")
        try:
            shutil.rmtree(temp_dir, ignore_errors=True)
        except Exception as e:
            print(f"Cleanup error: {e}")

    atexit.register(cleanup)

    inner_process = None
    def signal_handler(sig, frame):
        if inner_process and inner_process.poll() is None:
            inner_process.terminate()
            try:
                inner_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                inner_process.kill()
        cleanup()
        sys.exit(0)

    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)

    inner_process = subprocess.Popen(cmd)
    try:
        inner_process.wait()
    except:
        inner_process.terminate()
        inner_process.wait()
        raise
    finally:
        cleanup()
        atexit.unregister(cleanup)

    sys.exit(inner_process.returncode)

# ----------------------------------------------------------------------
# Inside venv: actual tests
# ----------------------------------------------------------------------
import argparse
import asyncio
import atexit
import os
import signal
import socket
import sys
import threading
import time
import traceback
import gzip
import zlib
import json
import struct
from urllib.parse import urlparse
from concurrent import futures
from io import BytesIO

import requests
import httpx
import websockets
import grpc
import grpc_tools
from grpc_tools import protoc
import socks
import brotli
import zstandard
from hypercorn.config import Config as HypercornConfig
from hypercorn.asyncio import serve as hypercorn_serve

import urllib3
urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

# ----------------------------------------------------------------------
# Global state
# ----------------------------------------------------------------------
server_manager = None
_shutdown_event = threading.Event()

def _handle_signal(signum, frame):
    print("\nShutting down...")
    _shutdown_event.set()
    if server_manager:
        server_manager.stop_all()
    sys.exit(0)

signal.signal(signal.SIGINT, _handle_signal)
signal.signal(signal.SIGTERM, _handle_signal)

# ----------------------------------------------------------------------
# Helper
# ----------------------------------------------------------------------
def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

# ----------------------------------------------------------------------
# Simple protobuf message
# ----------------------------------------------------------------------
class EchoMessage:
    @staticmethod
    def encode(msg: str) -> bytes:
        payload = msg.encode('utf-8')
        field = (1 << 3) | 2
        return struct.pack('B', field) + struct.pack('B', len(payload)) + payload

    @staticmethod
    def decode(data: bytes) -> str:
        if not data:
            raise ValueError("empty data")
        pos = 0
        while pos < len(data):
            tag, pos = _read_varint(data, pos)
            field_number = tag >> 3
            wire_type = tag & 0x07
            if wire_type == 2:
                length, pos = _read_varint(data, pos)
                value = data[pos:pos+length]
                pos += length
                if field_number == 1:
                    return value.decode('utf-8')
        raise ValueError("field 1 not found")

def _read_varint(data, pos):
    result = 0
    shift = 0
    while True:
        if pos >= len(data):
            raise ValueError("truncated varint")
        byte = data[pos]
        result |= (byte & 0x7f) << shift
        pos += 1
        if not (byte & 0x80):
            break
        shift += 7
    return result, pos

# ----------------------------------------------------------------------
# Test server implementations
# ----------------------------------------------------------------------
class ServerManager:
    def __init__(self, target_host='127.0.0.1'):
        self.target_host = target_host
        self.servers = []
        self.grpc_ca_pem = None  # set by start_grpc()

    def _add_server(self, name, port, shutdown):
        self.servers.append((name, port, shutdown))

    def stop_all(self):
        for name, port, shutdown in self.servers:
            try:
                shutdown()
                print(f"Stopped {name} server on port {port}")
            except Exception as e:
                print(f"Error stopping {name} server: {e}")

    # --- HTTP/1.1 server ---
    def start_http1(self):
        from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
        port = free_port()

        class Handler(BaseHTTPRequestHandler):
            # HTTP/1.1 is required for the /chunked response (Transfer-Encoding:
            # chunked is invalid over the BaseHTTPRequestHandler HTTP/1.0 default).
            protocol_version = 'HTTP/1.1'

            def log_message(self, format, *args):
                pass

            def _send_response(self, status, content_type, body, extra_headers=None):
                self.send_response(status)
                self.send_header('Content-Type', content_type)
                extra_headers = extra_headers or {}
                # Under HTTP/1.1 every non-chunked response needs a Content-Length
                # or the client blocks waiting for more. Add one unless the caller
                # already framed the body (Content-Length or Transfer-Encoding).
                lower = {k.lower() for k in extra_headers}
                if 'content-length' not in lower and 'transfer-encoding' not in lower:
                    self.send_header('Content-Length', str(len(body)))
                for k, v in extra_headers.items():
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                if self.path == '/gzip':
                    body = gzip.compress(b'Hello gzip')
                    self._send_response(200, 'text/plain', body, {'Content-Encoding': 'gzip'})
                elif self.path == '/deflate':
                    body = zlib.compress(b'Hello deflate')
                    self._send_response(200, 'text/plain', body, {'Content-Encoding': 'deflate'})
                elif self.path == '/brotli':
                    body = brotli.compress(b'Hello brotli')
                    self._send_response(200, 'text/plain', body, {'Content-Encoding': 'br'})
                elif self.path == '/zstd':
                    body = zstandard.compress(b'Hello zstd')
                    self._send_response(200, 'text/plain', body, {'Content-Encoding': 'zstd'})
                elif self.path == '/json':
                    data = json.dumps({'message': 'Hello JSON'}).encode()
                    self._send_response(200, 'application/json', data)
                elif self.path == '/proto':
                    data = EchoMessage.encode('Hello Proto')
                    self._send_response(200, 'application/x-protobuf', data)
                elif self.path == '/chunked':
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/plain')
                    self.send_header('Transfer-Encoding', 'chunked')
                    self.end_headers()
                    chunks = [b'Hello ', b'chunked ', b'world!']
                    for chunk in chunks:
                        self.wfile.write(f"{len(chunk):X}\r\n".encode())
                        self.wfile.write(chunk)
                        self.wfile.write(b"\r\n")
                    self.wfile.write(b"0\r\n\r\n")
                elif self.path == '/keep-alive':
                    body = b'Hello keepalive'
                    self._send_response(200, 'text/plain', body,
                                        {'Connection': 'keep-alive', 'Content-Length': str(len(body))})
                elif self.path == '/large':
                    size = 1024 * 1024
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/octet-stream')
                    self.send_header('Content-Length', str(size))
                    self.end_headers()
                    self.wfile.write(b'A' * size)
                else:
                    self._send_response(200, 'text/plain', b'Hello HTTP/1.1')

            def do_POST(self):
                content_length = int(self.headers.get('Content-Length', 0))
                body = self.rfile.read(content_length) if content_length > 0 else b''
                content_type = self.headers.get('Content-Type', '')

                if self.path == '/echo':
                    self._send_response(200, content_type, body)
                elif self.path == '/json-echo':
                    try:
                        data = json.loads(body)
                        data['received'] = True
                        resp_body = json.dumps(data).encode()
                        self._send_response(200, 'application/json', resp_body)
                    except Exception:
                        self.send_error(400, 'Bad JSON')
                elif self.path == '/proto-echo':
                    try:
                        msg = EchoMessage.decode(body)
                        resp_body = EchoMessage.encode(f"Echo: {msg}")
                        self._send_response(200, 'application/x-protobuf', resp_body)
                    except Exception:
                        self.send_error(400, 'Bad Proto')
                else:
                    self._send_response(200, 'text/plain', body)

        httpd = ThreadingHTTPServer((self.target_host, port), Handler)
        t = threading.Thread(target=httpd.serve_forever, daemon=True)
        t.start()
        self._add_server("HTTP/1.1", port, lambda: httpd.shutdown())
        return port

    # --- HTTP/2 server (Hypercorn) ---
    def start_http2(self):
        port = free_port()

        async def app(scope, receive, send):
            assert scope['type'] == 'http'
            path = scope['path']
            method = scope['method']

            status = 200
            headers = [(b'content-type', b'text/plain')]
            body = b'Hello HTTP/2'

            if method == 'GET':
                if path == '/gzip':
                    body = gzip.compress(b'Hello gzip')
                    headers.append((b'content-encoding', b'gzip'))
                elif path == '/deflate':
                    body = zlib.compress(b'Hello deflate')
                    headers.append((b'content-encoding', b'deflate'))
                elif path == '/brotli':
                    body = brotli.compress(b'Hello brotli')
                    headers.append((b'content-encoding', b'br'))
                elif path == '/zstd':
                    body = zstandard.compress(b'Hello zstd')
                    headers.append((b'content-encoding', b'zstd'))
                elif path == '/json':
                    body = json.dumps({'message': 'Hello JSON'}).encode()
                    headers = [(b'content-type', b'application/json')]
                elif path == '/proto':
                    body = EchoMessage.encode('Hello Proto')
                    headers = [(b'content-type', b'application/x-protobuf')]
                elif path == '/chunked':
                    headers = [(b'content-type', b'text/plain')]
                    await send({
                        'type': 'http.response.start',
                        'status': 200,
                        'headers': headers,
                    })
                    for chunk in [b'Hello ', b'chunked ', b'world!']:
                        await send({
                            'type': 'http.response.body',
                            'body': chunk,
                            'more_body': True,
                        })
                    await send({
                        'type': 'http.response.body',
                        'body': b'',
                        'more_body': False,
                    })
                    return
                elif path == '/keep-alive':
                    body = b'Hello keepalive'
                elif path == '/large':
                    body = b'A' * 1024 * 1024
            elif method == 'POST':
                body_chunks = []
                more_body = True
                while more_body:
                    message = await receive()
                    body_chunks.append(message.get('body', b''))
                    more_body = message.get('more_body', False)
                request_body = b''.join(body_chunks)
                if path == '/echo':
                    body = request_body
                elif path == '/json-echo':
                    try:
                        data = json.loads(request_body)
                        data['received'] = True
                        body = json.dumps(data).encode()
                        headers = [(b'content-type', b'application/json')]
                    except:
                        status = 400
                        body = b'Bad JSON'
                elif path == '/proto-echo':
                    try:
                        msg = EchoMessage.decode(request_body)
                        body = EchoMessage.encode(f"Echo: {msg}")
                        headers = [(b'content-type', b'application/x-protobuf')]
                    except:
                        status = 400
                        body = b'Bad Proto'
                else:
                    body = request_body
            else:
                status = 405
                body = b'Method Not Allowed'

            await send({
                'type': 'http.response.start',
                'status': status,
                'headers': headers,
            })
            await send({
                'type': 'http.response.body',
                'body': body,
                'more_body': False,
            })

        config = HypercornConfig()
        config.bind = [f"{self.target_host}:{port}"]
        config.alpn_protocols = ["h2"]
        # Serve real TLS so the https:// h2 target is reachable through the proxy's
        # MITM (the client does CONNECT + TLS, so the origin must speak TLS too).
        # A self-signed cert via trustme; the proxy accepts it when run with
        # OPROXY_INSECURE_UPSTREAM=1.
        import trustme
        _ca = trustme.CA()
        _server_cert = _ca.issue_cert("127.0.0.1", "localhost", self.target_host)
        self._http2_cert = tempfile.NamedTemporaryFile(suffix=".pem", delete=False)
        self._http2_key = tempfile.NamedTemporaryFile(suffix=".pem", delete=False)
        _server_cert.cert_chain_pems[0].write_to_path(self._http2_cert.name)
        _server_cert.private_key_pem.write_to_path(self._http2_key.name)
        config.certfile = self._http2_cert.name
        config.keyfile = self._http2_key.name

        loop = asyncio.new_event_loop()
        t = threading.Thread(target=loop.run_forever, daemon=True)
        t.start()

        async def _serve():
            # shutdown_trigger must be an async callable (returns awaitable).
            # _shutdown_event is a threading.Event; poll it from the async loop.
            async def _shutdown_trigger():
                while not _shutdown_event.is_set():
                    await asyncio.sleep(0.05)

            await hypercorn_serve(
                app,
                config,
                shutdown_trigger=_shutdown_trigger
            )
        asyncio.run_coroutine_threadsafe(_serve(), loop)
        time.sleep(1)

        def shutdown():
            # Setting the global event causes _shutdown_trigger to return,
            # which signals Hypercorn to stop gracefully.
            _shutdown_event.set()
        self._add_server("HTTP/2", port, shutdown)
        return port

    # --- WebSocket echo server ---
    def start_websocket(self):
        port = free_port()
        async def echo(websocket):
            async for message in websocket:
                await websocket.send(message)
        async def _serve():
            server = await websockets.serve(echo, self.target_host, port)
            await server.wait_closed()
        loop = asyncio.new_event_loop()
        t = threading.Thread(target=loop.run_forever, daemon=True)
        t.start()
        asyncio.run_coroutine_threadsafe(_serve(), loop)
        time.sleep(0.5)
        def shutdown():
            async def _stop():
                await server.close()
                loop.call_soon_threadsafe(loop.stop)
            asyncio.run_coroutine_threadsafe(_stop(), loop)
        self._add_server("WebSocket", port, shutdown)
        return port

    # --- gRPC echo server ---
    def start_grpc(self):
        port = free_port()
        gen_dir = os.path.join(os.path.dirname(__file__), "__generated_grpc__")
        os.makedirs(gen_dir, exist_ok=True)
        proto_path = os.path.join(gen_dir, "echo.proto")
        with open(proto_path, "w") as f:
            f.write("""
syntax = "proto3";
package echo;

service EchoService {
  rpc UnaryEcho (EchoRequest) returns (EchoResponse);
  rpc ServerStreamingEcho (EchoRequest) returns (stream EchoResponse);
  rpc ClientStreamingEcho (stream EchoRequest) returns (EchoResponse);
  rpc BidirectionalStreamingEcho (stream EchoRequest) returns (stream EchoResponse);
}

message EchoRequest {
  string message = 1;
}

message EchoResponse {
  string message = 1;
}
""")
        pb2_file = os.path.join(gen_dir, "echo_pb2.py")
        if not os.path.exists(pb2_file):
            ret = protoc.main([
                'grpc_tools.protoc',
                '-I', gen_dir,
                '--python_out=' + gen_dir,
                '--grpc_python_out=' + gen_dir,
                proto_path,
            ])
            if ret != 0:
                raise RuntimeError("protoc compilation failed")
        sys.path.insert(0, gen_dir)
        import echo_pb2
        import echo_pb2_grpc

        class EchoServicer(echo_pb2_grpc.EchoServiceServicer):
            def UnaryEcho(self, request, context):
                return echo_pb2.EchoResponse(message=request.message)
            def ServerStreamingEcho(self, request, context):
                for _ in range(3):
                    yield echo_pb2.EchoResponse(message=request.message)
            def ClientStreamingEcho(self, request_iterator, context):
                msgs = [req.message for req in request_iterator]
                return echo_pb2.EchoResponse(message=",".join(msgs))
            def BidirectionalStreamingEcho(self, request_iterator, context):
                for req in request_iterator:
                    yield echo_pb2.EchoResponse(message=req.message)

        server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
        echo_pb2_grpc.add_EchoServiceServicer_to_server(EchoServicer(), server)
        # Use TLS so the gRPC client goes through CONNECT (insecure_channel bypasses
        # the proxy for loopback addresses in many grpcio builds).
        import trustme as _trustme
        _grpc_ca = _trustme.CA()
        _grpc_srv_cert = _grpc_ca.issue_cert("127.0.0.1", "localhost", self.target_host)
        _key_pem = _grpc_srv_cert.private_key_pem.bytes()
        _cert_pem = b"".join(b.bytes() for b in _grpc_srv_cert.cert_chain_pems)
        server_cred = grpc.ssl_server_credentials([(_key_pem, _cert_pem)])
        server.add_secure_port(f"{self.target_host}:{port}", server_cred)
        server.start()
        # Expose the CA PEM so test_grpc can include it in root_certificates.
        self.grpc_ca_pem = _grpc_ca.cert_pem.bytes()
        self._add_server("gRPC", port, lambda: server.stop(0))
        return port

# ----------------------------------------------------------------------
# Test functions
# ----------------------------------------------------------------------
def make_test_url(base, path):
    return f"{base.rstrip('/')}{path}"

# HTTP/1.1 tests
def test_http1_basic(proxy_url, target_url, timeout, verbose=False):
    try:
        r = requests.get(target_url, proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout)
        if r.status_code == 200 and b'Hello' in r.content:
            print("[PASS] HTTP/1.1 GET")
            return True
        else:
            print(f"[FAIL] HTTP/1.1 GET: status={r.status_code}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 GET: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_encoding(proxy_url, base_url, path, encoding, expected_body, timeout, verbose):
    url = make_test_url(base_url, path)
    try:
        r = requests.get(url, proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout)
        if r.status_code == 200 and r.content == expected_body:
            print(f"[PASS] HTTP/1.1 {encoding}")
            return True
        else:
            print(f"[FAIL] HTTP/1.1 {encoding}: status={r.status_code}, body mismatch")
            if verbose:
                print(f"  Expected: {expected_body!r}")
                print(f"  Received: {r.content[:200]!r}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 {encoding}: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_json(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/json')
    try:
        r = requests.get(url, proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout)
        if r.status_code == 200 and r.json() == {'message': 'Hello JSON'}:
            print("[PASS] HTTP/1.1 JSON")
            return True
        else:
            print(f"[FAIL] HTTP/1.1 JSON")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 JSON: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_proto(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/proto')
    try:
        r = requests.get(url, proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout)
        msg = EchoMessage.decode(r.content)
        if r.status_code == 200 and msg == 'Hello Proto':
            print("[PASS] HTTP/1.1 Proto")
            return True
        else:
            print(f"[FAIL] HTTP/1.1 Proto: status={r.status_code}, msg={msg}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 Proto: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_chunked(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/chunked')
    try:
        r = requests.get(url, proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout, stream=True)
        data = r.raw.read()
        if r.status_code == 200 and data == b'Hello chunked world!':
            print("[PASS] HTTP/1.1 Chunked")
            return True
        else:
            print(f"[FAIL] HTTP/1.1 Chunked: status={r.status_code}, body={data}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 Chunked: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_keepalive(proxy_url, base_url, timeout, verbose):
    """Test that the proxy supports HTTP/1.1 Keep-Alive by sending two
    requests on the same connection and ensuring both succeed."""
    target = make_test_url(base_url, '/keep-alive')
    parsed_proxy = urlparse(proxy_url)
    proxy_host = parsed_proxy.hostname
    proxy_port = parsed_proxy.port or 80

    try:
        import http.client

        # Open a single connection to the proxy
        conn = http.client.HTTPConnection(proxy_host, proxy_port, timeout=timeout)
        # First request
        conn.request('GET', target, headers={'Connection': 'keep-alive'})
        resp1 = conn.getresponse()
        body1 = resp1.read()

        if resp1.status != 200 or body1 != b'Hello keepalive':
            print(f"[FAIL] HTTP/1.1 Keep-Alive: first request failed status={resp1.status} body={body1!r}")
            conn.close()
            return False

        # Check if proxy indicates it will close the connection
        if resp1.will_close or resp1.getheader('Connection', '').lower() == 'close':
            print("[FAIL] HTTP/1.1 Keep-Alive: proxy closed connection after first request")
            conn.close()
            return False

        # Second request on the same connection
        conn.request('GET', target, headers={'Connection': 'keep-alive'})
        resp2 = conn.getresponse()
        body2 = resp2.read()
        conn.close()

        if resp2.status == 200 and body2 == b'Hello keepalive':
            print("[PASS] HTTP/1.1 Keep-Alive")
            return True
        else:
            print(f"[FAIL] HTTP/1.1 Keep-Alive: second request failed status={resp2.status} body={body2!r}")
            return False

    except Exception as e:
        print(f"[FAIL] HTTP/1.1 Keep-Alive: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_large(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/large')
    try:
        r = requests.get(url, proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout, stream=True)
        data = r.raw.read()
        expected_len = 1024 * 1024
        if r.status_code == 200 and len(data) == expected_len and all(b == 0x41 for b in data):
            print("[PASS] HTTP/1.1 Large (1MB)")
            return True
        else:
            print(f"[FAIL] HTTP/1.1 Large: status={r.status_code}, len={len(data)}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 Large: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_post_json(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/json-echo')
    payload = {'hello': 'world'}
    try:
        r = requests.post(url, json=payload, proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout)
        if r.status_code == 200:
            data = r.json()
            if data.get('hello') == 'world' and data.get('received'):
                print("[PASS] HTTP/1.1 POST JSON")
                return True
        print(f"[FAIL] HTTP/1.1 POST JSON: status={r.status_code}, body={r.text[:200]}")
        return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 POST JSON: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http1_post_proto(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/proto-echo')
    payload = EchoMessage.encode('Test Proto')
    try:
        r = requests.post(url, data=payload,
                          headers={'Content-Type': 'application/x-protobuf'},
                          proxies={'http': proxy_url, 'https': proxy_url}, timeout=timeout)
        if r.status_code == 200:
            resp_msg = EchoMessage.decode(r.content)
            if resp_msg == 'Echo: Test Proto':
                print("[PASS] HTTP/1.1 POST Proto")
                return True
        print(f"[FAIL] HTTP/1.1 POST Proto: status={r.status_code}")
        return False
    except Exception as e:
        print(f"[FAIL] HTTP/1.1 POST Proto: {e}")
        if verbose:
            traceback.print_exc()
        return False

# HTTP/2 tests – we disable SSL verification because the test server uses a self-signed cert.
# The proxy's own certificate (for client <-> proxy) is not involved in these tests.
def test_http2_encoding(proxy_url, base_url, path, encoding, expected_body, timeout, verbose):
    url = make_test_url(base_url, path)
    try:
        client = httpx.Client(http2=True, proxy=proxy_url, timeout=timeout, verify=False)
        r = client.get(url)
        if r.http_version == 'HTTP/2' and r.status_code == 200 and r.content == expected_body:
            print(f"[PASS] HTTP/2 {encoding}")
            return True
        else:
            print(f"[FAIL] HTTP/2 {encoding}: version={r.http_version} status={r.status_code}")
            if verbose:
                print(f"  Expected: {expected_body!r}")
                print(f"  Received: {r.content[:200]!r}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/2 {encoding}: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http2_json(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/json')
    try:
        client = httpx.Client(http2=True, proxy=proxy_url, timeout=timeout, verify=False)
        r = client.get(url)
        if r.http_version == 'HTTP/2' and r.status_code == 200 and r.json() == {'message': 'Hello JSON'}:
            print("[PASS] HTTP/2 JSON")
            return True
        else:
            print("[FAIL] HTTP/2 JSON")
            if verbose:
                print(f"  Status: {r.status_code}, body: {r.text[:200]}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/2 JSON: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http2_proto(proxy_url, base_url, timeout, verbose):
    url = make_test_url(base_url, '/proto')
    try:
        client = httpx.Client(http2=True, proxy=proxy_url, timeout=timeout, verify=False)
        r = client.get(url)
        try:
            msg = EchoMessage.decode(r.content)
        except ValueError as decode_err:
            if verbose:
                print(f"  Raw content (hex): {r.content.hex()}")
                print(f"  Raw content (repr): {r.content!r}")
            raise decode_err
        if r.http_version == 'HTTP/2' and r.status_code == 200 and msg == 'Hello Proto':
            print("[PASS] HTTP/2 Proto")
            return True
        else:
            print(f"[FAIL] HTTP/2 Proto: version={r.http_version} status={r.status_code} msg={msg!r}")
            if verbose:
                print(f"  Raw content: {r.content!r}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/2 Proto: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_http2_basic(proxy_url, target_url, timeout, verbose):
    try:
        client = httpx.Client(http2=True, proxy=proxy_url, timeout=timeout, verify=False)
        r = client.get(target_url)
        if r.http_version == 'HTTP/2' and r.status_code == 200:
            print("[PASS] HTTP/2 GET")
            return True
        else:
            print(f"[FAIL] HTTP/2 GET: version={r.http_version} status={r.status_code}")
            return False
    except Exception as e:
        print(f"[FAIL] HTTP/2 GET: {e}")
        if verbose:
            traceback.print_exc()
        return False

# WebSocket tests
def test_websocket_text(proxy_url, target_url, timeout, verbose):
    if not proxy_url.startswith('http://'):
        print("[FAIL] WebSocket Text: websockets library requires HTTP proxy (http://host:port)")
        return False
    try:
        async def _test():
            async with websockets.connect(
                target_url,
                proxy=proxy_url,
                ping_timeout=timeout,
                close_timeout=timeout,
            ) as ws:
                test_msg = "Hello WebSocket"
                await ws.send(test_msg)
                resp = await asyncio.wait_for(ws.recv(), timeout=timeout)
                return resp == test_msg
        result = asyncio.run(_test())
        if result:
            print("[PASS] WebSocket Text")
            return True
        else:
            print("[FAIL] WebSocket Text: echo mismatch")
            return False
    except websockets.exceptions.InvalidMessage:
        print("[FAIL] WebSocket Text: proxy did not return valid HTTP response (check proxy support for WebSocket upgrade)")
        if verbose:
            traceback.print_exc()
        return False
    except Exception as e:
        print(f"[FAIL] WebSocket Text: {e}")
        if verbose:
            traceback.print_exc()
        return False

def test_websocket_binary(proxy_url, target_url, timeout, verbose):
    if not proxy_url.startswith('http://'):
        print("[FAIL] WebSocket Binary: websockets library requires HTTP proxy (http://host:port)")
        return False
    try:
        async def _test():
            async with websockets.connect(
                target_url,
                proxy=proxy_url,
                ping_timeout=timeout,
                close_timeout=timeout,
            ) as ws:
                payload = b'\x00\x01\x02\xfe\xff'
                await ws.send(payload)
                resp = await asyncio.wait_for(ws.recv(), timeout=timeout)
                return resp == payload
        result = asyncio.run(_test())
        if result:
            print("[PASS] WebSocket Binary")
            return True
        else:
            print("[FAIL] WebSocket Binary: echo mismatch")
            return False
    except websockets.exceptions.InvalidMessage:
        print("[FAIL] WebSocket Binary: proxy did not return valid HTTP response (check proxy support for WebSocket upgrade)")
        if verbose:
            traceback.print_exc()
        return False
    except Exception as e:
        print(f"[FAIL] WebSocket Binary: {e}")
        if verbose:
            traceback.print_exc()
        return False

# gRPC test
def test_grpc(proxy_url, target_host, target_port, timeout, verbose=False,
              grpc_server_ca_pem=None):
    gen_dir = os.path.join(os.path.dirname(__file__), "__generated_grpc__")
    if gen_dir not in sys.path:
        sys.path.insert(0, gen_dir)
    import echo_pb2
    import echo_pb2_grpc

    # Fetch the proxy's root CA cert so MITM-generated certs are trusted.
    # grpc.secure_channel always uses HTTP CONNECT through the proxy (unlike
    # insecure_channel which silently bypasses the proxy for loopback addresses).
    proxy_ca_pem = None
    try:
        from urllib.parse import urlparse as _urlparse
        _pa = _urlparse(proxy_url)
        _ca_url = f"http://{_pa.netloc}/admin/ca"
        import urllib.request as _ur
        proxy_ca_pem = _ur.urlopen(_ca_url, timeout=5).read()
    except Exception as _e:
        if verbose:
            print(f"[WARN] gRPC: could not fetch proxy CA ({_e}); TLS will fail")

    # Combine proxy CA (trusted in MITM mode) and server CA (trusted in tunnel
    # mode).  Including both is harmless: in MITM mode the client sees the
    # proxy's cert; in tunnel mode it sees the server's cert directly.
    combined_ca = b""
    if proxy_ca_pem:
        combined_ca += proxy_ca_pem
    if grpc_server_ca_pem:
        combined_ca += grpc_server_ca_pem
    channel_creds = grpc.ssl_channel_credentials(root_certificates=combined_ca or None)

    target = f"{target_host}:{target_port}"
    # Use the channel option rather than env-vars: grpcio's env-var proxy
    # support is unreliable for loopback addresses on many builds.
    channel_options = [('grpc.http_proxy', proxy_url)]
    try:
        with grpc.secure_channel(target, channel_creds, options=channel_options) as channel:
            stub = echo_pb2_grpc.EchoServiceStub(channel)
            req = echo_pb2.EchoRequest(message="hello")

            # unary
            resp = stub.UnaryEcho(req, timeout=timeout)
            assert resp.message == "hello", f"unary mismatch: {resp.message}"

            # server streaming
            responses = stub.ServerStreamingEcho(req, timeout=timeout)
            count = 0
            for r in responses:
                assert r.message == "hello", f"server stream mismatch: {r.message}"
                count += 1
            assert count == 3, f"server stream count {count} != 3"

            # client streaming
            def req_iter():
                for _ in range(3):
                    yield req
            resp = stub.ClientStreamingEcho(req_iter(), timeout=timeout)
            assert resp.message == "hello,hello,hello", f"client stream mismatch: {resp.message}"

            # bidirectional streaming
            def bidi_req():
                yield req
                yield echo_pb2.EchoRequest(message="world")
            bidi_responses = stub.BidirectionalStreamingEcho(bidi_req(), timeout=timeout)
            msgs = [r.message for r in bidi_responses]
            assert msgs == ["hello", "world"], f"bidi mismatch: {msgs}"

        print("[PASS] gRPC (unary, server streaming, client streaming, bidirectional)")
        return True
    except Exception as e:
        print(f"[FAIL] gRPC: {e}")
        if verbose:
            traceback.print_exc()
        return False

# SOCKS5 TCP test
def test_socks5_tcp(proxy_url, target_host, target_port, timeout, verbose=False):
    parsed = urlparse(proxy_url)
    proxy_host = parsed.hostname
    proxy_port = parsed.port or 1080
    try:
        s = socks.socksocket()
        s.set_proxy(socks.SOCKS5, proxy_host, proxy_port)
        s.settimeout(timeout)
        s.connect((target_host, target_port))
        request = f"GET / HTTP/1.1\r\nHost: {target_host}:{target_port}\r\nConnection: close\r\n\r\n"
        s.sendall(request.encode())
        response = b""
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            response += chunk
        s.close()
        if b"200 OK" in response and b"Hello HTTP/1.1" in response:
            print("[PASS] SOCKS5 TCP (HTTP over SOCKS5)")
            return True
        else:
            print("[FAIL] SOCKS5 TCP: unexpected response")
            if verbose:
                print(f"  Raw response (first 500 bytes): {response[:500]}")
            return False
    except Exception as e:
        print(f"[FAIL] SOCKS5 TCP: {e}")
        if verbose:
            traceback.print_exc()
        return False

# SOCKS5 UDP test
def test_socks5_udp(proxy_url, target_host, target_port, timeout, verbose):
    parsed = urlparse(proxy_url)
    proxy_host = parsed.hostname
    proxy_port = parsed.port or 1080
    try:
        s = socks.socksocket(socket.AF_INET, socket.SOCK_DGRAM)
        s.set_proxy(socks.SOCKS5, proxy_host, proxy_port)
        s.settimeout(timeout)
        s.bind(('0.0.0.0', 0))
        message = b'Hello UDP'
        s.sendto(message, (target_host, target_port))
        data, addr = s.recvfrom(1024)
        s.close()
        if data == message:
            print("[PASS] SOCKS5 UDP")
            return True
        else:
            print(f"[FAIL] SOCKS5 UDP: received {data}")
            return False
    except socks.SOCKS5Error as e:
        if '0x07' in str(e):
            # UDP ASSOCIATE (cmd 0x07) is not implemented; skip rather than fail.
            print(f"[SKIP] SOCKS5 UDP: UDP ASSOCIATE not supported by this proxy")
            return None
        print(f"[FAIL] SOCKS5 UDP: {e}")
        if verbose:
            traceback.print_exc()
        return False
    except Exception as e:
        print(f"[FAIL] SOCKS5 UDP: {e}")
        if verbose:
            traceback.print_exc()
        return False

# ----------------------------------------------------------------------
# Main inner test runner
# ----------------------------------------------------------------------
def run_tests():
    parser = argparse.ArgumentParser(description="Comprehensive proxy tester (inner)")
    parser.add_argument('--http-proxy', default=None)
    parser.add_argument('--socks-proxy', default=None)
    parser.add_argument('--target-host', default='127.0.0.1')
    parser.add_argument('--timeout', type=int, default=10)
    parser.add_argument('--verbose', action='store_true')
    parser.add_argument('--run-tests', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()

    http_proxy = args.http_proxy
    socks_proxy = args.socks_proxy
    target_host = args.target_host
    timeout = args.timeout
    verbose = args.verbose

    if not http_proxy and not socks_proxy:
        print("Error: Provide at least --http-proxy or --socks-proxy.")
        sys.exit(1)

    print(f"Starting test servers on {target_host}...")
    global server_manager
    server_manager = ServerManager(target_host)

    http1_port = server_manager.start_http1()
    http1_base_url = f"http://{target_host}:{http1_port}"

    http2_port = server_manager.start_http2()
    http2_base_url = f"https://{target_host}:{http2_port}"

    ws_port = server_manager.start_websocket()
    ws_url = f"ws://{target_host}:{ws_port}"

    grpc_port = server_manager.start_grpc()

    time.sleep(1)

    all_results = []

    if http_proxy:
        print("\n=== Testing HTTP proxy:", http_proxy)
        def http_add(name, result):
            all_results.append(("HTTP", name, result))

        http_add("HTTP/1.1 GET", test_http1_basic(http_proxy, f"{http1_base_url}/", timeout, verbose))
        http_add("HTTP/1.1 gzip", test_http1_encoding(http_proxy, http1_base_url, "/gzip", "gzip", b"Hello gzip", timeout, verbose))
        http_add("HTTP/1.1 deflate", test_http1_encoding(http_proxy, http1_base_url, "/deflate", "deflate", b"Hello deflate", timeout, verbose))
        http_add("HTTP/1.1 brotli", test_http1_encoding(http_proxy, http1_base_url, "/brotli", "br", b"Hello brotli", timeout, verbose))
        http_add("HTTP/1.1 zstd", test_http1_encoding(http_proxy, http1_base_url, "/zstd", "zstd", b"Hello zstd", timeout, verbose))
        http_add("HTTP/1.1 JSON", test_http1_json(http_proxy, http1_base_url, timeout, verbose))
        http_add("HTTP/1.1 Proto", test_http1_proto(http_proxy, http1_base_url, timeout, verbose))
        http_add("HTTP/1.1 Chunked", test_http1_chunked(http_proxy, http1_base_url, timeout, verbose))
        http_add("HTTP/1.1 Keep-Alive", test_http1_keepalive(http_proxy, http1_base_url, timeout, verbose))
        http_add("HTTP/1.1 Large", test_http1_large(http_proxy, http1_base_url, timeout, verbose))
        http_add("HTTP/1.1 POST JSON", test_http1_post_json(http_proxy, http1_base_url, timeout, verbose))
        http_add("HTTP/1.1 POST Proto", test_http1_post_proto(http_proxy, http1_base_url, timeout, verbose))

        http_add("HTTP/2 GET", test_http2_basic(http_proxy, f"{http2_base_url}/", timeout, verbose))
        http_add("HTTP/2 gzip", test_http2_encoding(http_proxy, http2_base_url, "/gzip", "gzip", b"Hello gzip", timeout, verbose))
        http_add("HTTP/2 deflate", test_http2_encoding(http_proxy, http2_base_url, "/deflate", "deflate", b"Hello deflate", timeout, verbose))
        http_add("HTTP/2 brotli", test_http2_encoding(http_proxy, http2_base_url, "/brotli", "br", b"Hello brotli", timeout, verbose))
        http_add("HTTP/2 zstd", test_http2_encoding(http_proxy, http2_base_url, "/zstd", "zstd", b"Hello zstd", timeout, verbose))
        http_add("HTTP/2 JSON", test_http2_json(http_proxy, http2_base_url, timeout, verbose))
        http_add("HTTP/2 Proto", test_http2_proto(http_proxy, http2_base_url, timeout, verbose))

        http_add("WebSocket Text", test_websocket_text(http_proxy, ws_url, timeout, verbose))
        http_add("WebSocket Binary", test_websocket_binary(http_proxy, ws_url, timeout, verbose))
        http_add("gRPC", test_grpc(http_proxy, target_host, grpc_port, timeout, verbose,
                                   grpc_server_ca_pem=server_manager.grpc_ca_pem))

    if socks_proxy:
        print("\n=== Testing SOCKS5 proxy:", socks_proxy)
        def socks_add(name, result):
            all_results.append(("SOCKS5", name, result))

        socks_add("SOCKS5 TCP", test_socks5_tcp(socks_proxy, target_host, http1_port, timeout, verbose))

        udp_port = free_port()
        def udp_echo_server():
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.bind(('127.0.0.1', udp_port))
            while not _shutdown_event.is_set():
                try:
                    data, addr = sock.recvfrom(1024)
                    sock.sendto(data, addr)
                except socket.timeout:
                    continue
                except:
                    break
            sock.close()
        udp_thread = threading.Thread(target=udp_echo_server, daemon=True)
        udp_thread.start()
        server_manager._add_server("UDP Echo", udp_port, lambda: None)
        time.sleep(0.2)
        socks_add("SOCKS5 UDP", test_socks5_udp(socks_proxy, target_host, udp_port, timeout, verbose))

    # Summary
    print("\n" + "="*40)
    print("FINAL TEST SUMMARY")
    print("-"*40)
    passed = sum(1 for _, _, ok in all_results if ok is True)
    skipped = sum(1 for _, _, ok in all_results if ok is None)
    total = len(all_results) - skipped
    for proxy_type, name, ok in all_results:
        status = "PASS" if ok is True else ("SKIP" if ok is None else "FAIL")
        print(f"  [{proxy_type}] {name}: {status}")
    print(f"\n{passed}/{total} tests passed" + (f" ({skipped} skipped)" if skipped else ""))
    if passed != total:
        sys.exit(1)

    server_manager.stop_all()

if __name__ == "__main__":
    run_tests()