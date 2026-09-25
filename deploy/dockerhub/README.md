# DBX

DBX is a lightweight, self-hosted database client for the browser. It supports 90+ databases, including MySQL, PostgreSQL, SQLite, Redis, MongoDB, DuckDB, ClickHouse, SQL Server, Oracle, and Elasticsearch.

- Official website: https://dbxio.com
- Documentation: https://dbxio.com/en/docs/getting-started
- 中文文档: https://dbxio.com/cn/docs/getting-started
- Source code: https://github.com/t8y2/dbx

## Quick Start

Set a strong access password and start DBX:

```bash
docker run -d \
  --pull=always \
  --name dbx \
  -p 127.0.0.1:4224:4224 \
  -e DBX_PASSWORD='change-this-password' \
  -v dbx-data:/app/data \
  --restart unless-stopped \
  t8y2/dbx:latest
```

Open `http://localhost:4224` and sign in with the value of `DBX_PASSWORD`.

The examples publish the port on `127.0.0.1` only. To reach DBX from other machines, put a TLS reverse proxy in front of it, or publish on all interfaces (`-p 4224:4224`) on a trusted network. If no password is configured, DBX prints a one-time setup token in the container logs (`docker logs dbx`) that is required to set the first password.

The image supports `linux/amd64` and `linux/arm64`.

## Docker Compose

```yaml
services:
  dbx:
    image: t8y2/dbx:latest
    pull_policy: always
    environment:
      # Required; compose refuses to start without it (set it in .env).
      DBX_PASSWORD: ${DBX_PASSWORD:?Set DBX_PASSWORD}
    ports:
      # Loopback by default; DBX_BIND=0.0.0.0 publishes on all interfaces.
      - "${DBX_BIND:-127.0.0.1}:4224:4224"
    volumes:
      - dbx-data:/app/data
    restart: unless-stopped

volumes:
  dbx-data:
```

Start or update the service:

```bash
docker compose up -d --pull always
```

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `DBX_PASSWORD` | Not set | Access password for the DBX Web login page. Set a strong value for server deployments. |
| `DBX_DISABLE_PASSWORD` | `false` | Disables login protection when set to `true`. Do not use this on an untrusted network. |
| `DBX_DATA_DIR` | `/app/data` | Directory containing the DBX database, plugins, drivers, and other persistent data. |
| `DBX_SECRET_KEY_FILE` | Not set | Optional external key file. Takes precedence over the managed data-directory key. |
| `DBX_SECRET_KEY` | Not set | Optional key supplied by a secret manager. Used when no key file is configured. |
| `DBX_PORT` | `4224` | HTTP port inside the container. |
| `DBX_HOST` | `0.0.0.0` (image) | Listen address inside the container. The image sets `0.0.0.0` so the published port works; restrict exposure with the host-side port mapping. |
| `DBX_RUN_AS_ROOT` | Not set | The container starts as root only to hand `/app/data` and `/app/backups` to the unprivileged `dbx` user (uid/gid 10001), then drops privileges. Set to `1` to keep running as root, e.g. when a mounted secret is readable only by root. |
| `DBX_PUBLIC_BASE_PATH` | `/` | URL prefix for reverse-proxy deployments, for example `/dbx`. |
| `DBX_WEB_MCP_TOKEN` | Not set | Enables native Streamable HTTP MCP with this bearer token. Keep it secret. |
| `DBX_WEB_MCP_TOKEN_FILE` | Not set | Read the native MCP bearer token from a file, for example a mounted Docker secret. Cannot be combined with `DBX_WEB_MCP_TOKEN`. |
| `DBX_WEB_MCP_ALLOWED_HOSTS` | Not set | Required when native MCP is enabled. Comma-separated public Host authorities, including ports when present. |
| `DBX_WEB_MCP_ALLOWED_ORIGINS` | Not set | Comma-separated browser Origins allowed to call native MCP. Optional for non-browser MCP clients. |

DBX runs as the unprivileged user `dbx` (uid/gid 10001). Volumes created by older, root-based images are re-owned automatically on start. When you start the container with `--user`, make sure the data directory is writable by that user. The image has a `HEALTHCHECK` that probes `/api/auth/check`.

Persist `/app/data` with a named volume or bind mount. DBX creates `/app/data/.dbx/secret.key` on the first sensitive write or data migration. Back up this file together with `/app/data/dbx.db`; losing it makes existing encrypted credentials unreadable. A key stored in the same volume does not protect against disclosure of the entire volume.

For production, an external Docker Secret can replace the managed key:

```yaml
services:
  dbx:
    image: t8y2/dbx:latest
    environment:
      DBX_SECRET_KEY_FILE: /run/secrets/dbx_secret_key
    secrets:
      - dbx_secret_key
    volumes:
      - dbx-data:/app/data

secrets:
  dbx_secret_key:
    file: ./dbx_secret_key
```

Create the external key once, keep it stable across upgrades, and never replace it while encrypted data exists.

## Native HTTP MCP

Native MCP is disabled by default. When enabled, it is served by the existing DBX Web listener at `/mcp`; no second container port is required. With a host mapping of `4225:4224`, configure the public authority clients use:

```yaml
environment:
  DBX_WEB_MCP_TOKEN: replace-with-a-long-random-secret
  DBX_WEB_MCP_ALLOWED_HOSTS: localhost:4225
ports:
  - "4225:4224"
```

The MCP endpoint is `http://localhost:4225/mcp` and requires `Authorization: Bearer <DBX_WEB_MCP_TOKEN>`. For a reverse proxy, set `DBX_WEB_MCP_ALLOWED_HOSTS` to the public hostname (and port if non-default); with `DBX_PUBLIC_BASE_PATH: /dbx`, the endpoint becomes `/dbx/mcp`. Browser-based clients must also set `DBX_WEB_MCP_ALLOWED_ORIGINS` to their exact `https://host[:port]` origin.

The token is a deployment credential: rotate it through your secret manager and restart the container. DBX Desktop offers a local **Rotate Token** action for its separately managed loopback HTTP MCP service.

DuckDB is delivered as a standalone native driver instead of being embedded in `dbx-web`. Install the DuckDB driver from Driver Manager after the first launch. It is stored under `/app/data/agents` and remains available across container upgrades when `/app/data` is persisted.

## Reverse Proxy

To publish DBX under a path such as `https://example.com/dbx`, set:

```yaml
environment:
  DBX_PUBLIC_BASE_PATH: /dbx
```

Configure the reverse proxy to forward the same `/dbx` prefix to port `4224` in the container.

## China Mirror

For faster pulls in mainland China, use the CNB mirror:

```text
docker.cnb.cool/dbxio.com/dbx:latest
```

## 1Panel

DBX is available from the 1Panel app store. See the official installation guide for port, password, persistence, and access instructions:

- 中文教程: https://dbxio.com/cn/docs/1panel
- English guide: https://dbxio.com/en/docs/1panel

## Tags

- `latest`: latest stable DBX release
- `<version>`: a specific DBX release
- `dev`: current development image

For production deployments, pin a version tag when you need controlled upgrades.
