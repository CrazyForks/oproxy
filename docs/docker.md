# Docker

The Docker image builds the React UI, compiles the Rust binary, and runs as the `oproxy` system user from `/app`.

## Build

```bash
docker build -t oproxy:latest .
```

## docker run

Using the release image:

```bash
docker run --rm \
  --name oproxy \
  -p 127.0.0.1:8080:8080 \
  -p 127.0.0.1:1080:1080 \
  -e OPROXY_BIND_HOST=0.0.0.0 \
  -e OPROXY_MITM_ENABLED=true \
  -v oproxy-certs:/app/certs \
  -v oproxy-storage:/app/storage \
  ghcr.io/sauravrao637/oproxy:latest
```

Using a local build:

```bash
docker run --rm \
  --name oproxy \
  -p 127.0.0.1:8080:8080 \
  -p 127.0.0.1:1080:1080 \
  -e OPROXY_BIND_HOST=0.0.0.0 \
  -e OPROXY_MITM_ENABLED=true \
  -v oproxy-certs:/app/certs \
  -v oproxy-storage:/app/storage \
  oproxy:latest
```

Docker port publishing needs the process inside the container to bind to `0.0.0.0`. The host mappings above expose the service only on host loopback.

## Docker Compose

```bash
docker compose up --build
```

The checked-in `docker-compose.yml` uses:

- `network_mode: host`
- `OPROXY_BIND_HOST=0.0.0.0`
- `OPROXY_MITM_ENABLED=true`
- `OPROXY_HTTP3_ENABLED=true` + `OPROXY_HTTP3_PORT=8443`
- `OPROXY_ALLOW_REMOTE_ADMIN=false`
- `oproxy-certs:/app/certs`
- `oproxy-storage:/app/storage`

Because it uses host networking, the Compose file does not declare a `ports:` block.

### HTTP/3 (QUIC)

The Docker image is built with `--all-features`, so the `http3` listener is
available out of the box. The Compose file enables it on UDP port 8443; with
host networking it is reachable directly. For bridge networking (or
`docker run`), publish the UDP port explicitly, e.g. `-p 127.0.0.1:8443:8443/udp`.
Clients must trust the oproxy root CA (`/admin/ca`) for QUIC TLS, and forwarded
responses advertise the listener via `alt-svc: h3=":8443"`.

## Protocol Fixtures

For local protocol testing without external services, start the fixture profile:

```bash
docker compose --profile fixtures up --build
```

This starts `oproxy` plus stable local fixture origins:

| Fixture | URL / target |
| --- | --- |
| HTTP/1.1 | `http://127.0.0.1:18080/` |
| HTTP/2 TLS | `https://127.0.0.1:18443/` |
| WebSocket echo | `ws://127.0.0.1:18081/` |
| gRPC TLS echo | `127.0.0.1:19090`, service `echo.EchoService` |

The fixture container writes self-signed origin CAs to the
`protocol-fixture-certs` Docker volume. When clients connect through oproxy with
MITM enabled, install/trust the oproxy CA from `/admin/ca`. When clients connect
directly to the HTTP/2 or gRPC fixture origins, trust the matching fixture CA
from that volume instead.

The checked-in Compose file keeps host networking as the default oproxy path.
The fixture ports are also published on host loopback so host-network oproxy,
browser tests, and local CLI clients can all target the same addresses. For
bridge-mode experiments, remove/comment `network_mode: host`, uncomment the
oproxy `ports:` block, and target the fixture service name
`protocol-fixtures` from inside the Compose network.

## Volumes

`/app/certs` stores the generated root CA files:

- `root.crt`
- `root.key`

`/app/storage` stores persisted control-plane state:

- `rule_sets.json`
- `map_remote_rules.json`
- `map_local_rules.json`
- `access_rules.json`
- `throttle.json`
- `dns_overrides.json`
- `breakpoints.json`
- `capture_filter.json`
- `upstream_proxy.json`
- `hot_config.json`
- `lua_scripts.json`
- `mock_rules.json`
- `webhooks.json`

Live captured sessions are kept in memory unless you export HAR or explicitly save sessions with `/admin/sessions/save`.

## Upgrades

Build or pull the new image, then recreate the container with the same named volumes.

```bash
docker build -t oproxy:latest .
docker stop oproxy || true
docker run --rm \
  --name oproxy \
  -p 127.0.0.1:8080:8080 \
  -p 127.0.0.1:1080:1080 \
  -e OPROXY_BIND_HOST=0.0.0.0 \
  -e OPROXY_MITM_ENABLED=true \
  -v oproxy-certs:/app/certs \
  -v oproxy-storage:/app/storage \
  oproxy:latest
```

Keep the CA volume if clients already trust the current CA. Replacing it creates a new CA and requires reinstalling trust.
