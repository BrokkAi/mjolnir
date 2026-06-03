# Remote Control Implementation Plan

## Goals

Implement a simple local remote-control server for `mj`:

- `mj server` starts an HTTPS server on `127.0.0.1:11399` by default.
- No user-authored TOML or manual configuration is required.
- Server state is stored in SQLite.
- TLS material is generated automatically.
- Initial admin authentication uses a printed one-time login token; later browser requests use an HTTP-only cookie.
- Client machine APIs require an approved client certificate fingerprint.
- A machine can have any number of active `mj` sessions.
- Sessions push transcript/events to the server.
- The website can queue prompts for sessions.
- Clients poll queued prompts when idle, using a non-aggressive interval.

## Storage

Server files live under `$XDG_DATA_HOME/mj/remote/`:

- `remote.db`
- `ca-cert.pem`
- `ca-key.pem`
- `server-cert.pem`
- `server-key.pem`

No remote-control TOML file is required.

## SQLite schema

```sql
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS admin_sessions (
    id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS machines (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending', 'approved', 'rejected')),
    csr_pem TEXT NOT NULL,
    cert_pem TEXT,
    cert_fingerprint_sha256 TEXT,
    created_at INTEGER NOT NULL,
    approved_at INTEGER,
    rejected_at INTEGER,
    last_seen_at INTEGER
);

CREATE TABLE IF NOT EXISTS client_sessions (
    id TEXT PRIMARY KEY,
    machine_id TEXT NOT NULL,
    cwd TEXT,
    agent_label TEXT,
    status TEXT NOT NULL CHECK(status IN ('active', 'idle', 'processing', 'closed')),
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER,
    closed_at INTEGER
);

CREATE TABLE IF NOT EXISTS session_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS queued_prompts (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    text TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('queued', 'delivered', 'completed', 'failed')),
    created_at INTEGER NOT NULL,
    delivered_at INTEGER,
    completed_at INTEGER,
    error TEXT
);
```

## TLS and fingerprints

The server owns a local CA and a server certificate signed by that CA.

Client enrollment uses a CSR:

1. A client machine creates a private key locally.
2. The client submits a CSR to `/client/enroll`.
3. The server stores the machine as `pending`.
4. The admin approves the machine in the website.
5. The server signs the CSR, stores the client certificate, and computes `SHA-256(leaf_cert_der)`.
6. Later client requests present that certificate.
7. Client routes require a presented certificate whose SHA-256 fingerprint maps to an `approved` machine.

Fingerprint format is lowercase hex SHA-256 over the DER bytes of the client leaf certificate.

## Routes

### Admin/browser

- `GET /`
- `POST /api/login`
- `POST /api/logout`
- `GET /api/machines`
- `POST /api/machines/:id/approve`
- `POST /api/machines/:id/reject`
- `GET /api/sessions`
- `GET /api/sessions/:id/events`
- `POST /api/sessions/:id/prompts`

All `/api/*` routes except `/api/login` require a valid admin cookie.

### Client bootstrap

- `POST /client/enroll`
- `GET /client/enroll/:id`

### Client authenticated routes

These require an approved client certificate fingerprint:

- `POST /client/heartbeat`
- `POST /client/sessions`
- `POST /client/sessions/:id/events`
- `GET /client/sessions/:id/prompts/next`
- `POST /client/prompts/:id/complete`
- `POST /client/prompts/:id/fail`

## Website

Use one inline HTML/CSS/JavaScript page. No frontend build, no npm, and no static asset pipeline.

The dashboard supports:

- login
- viewing pending/approved/rejected machines
- approving/rejecting machines
- viewing sessions
- viewing a thread of session events
- sending prompts to a session

The browser polls the API periodically.

## Implementation phases

1. Server skeleton, SQLite schema, generated TLS material, admin token, HTTPS listener.
2. Admin login and cookie auth.
3. Machine enrollment, approval/rejection, certificate signing, fingerprint storage.
4. Client session APIs guarded by approved certificate fingerprint.
5. Inline website dashboard.
6. Later: integrate normal `mj` sessions as clients that enroll, push events, and poll prompts every 30 seconds when idle.

## Validation

Run before submitting:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
