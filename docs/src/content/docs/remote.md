---
title: Remote control
description: Expose the same Council to a browser with explicit network and session boundaries.
---

`mj server` starts Mjolnir's remote-control server with the same resolved
Council as the terminal client.

## Default local server

```bash
mj server
```

The default listens on loopback HTTPS (`127.0.0.1` and `::1`) on port 11921
with a locally generated certificate. It is reachable only from the same
machine and does not print a device-login QR code.

The viewer uses a bearer login token or short viewer code, then stores a signed
session cookie. Treat QR codes, login URLs, tokens, cookies, certificate keys,
and downloaded transcripts as secrets.

## Tailscale

```bash
mj server --tailscale
```

This requires Tailscale, MagicDNS, and HTTPS Certificates enabled on the
tailnet. Mjolnir binds to network interfaces, asks `tailscale cert` for the
machine's `ts.net` certificate, and renews it. Tailnet reachability and ACLs are
part of the security boundary.

## Public hostname

```bash
mj server --hostname mj.example.com
```

This binds to network interfaces and generates a self-signed certificate for
the supplied hostname. It does not provision DNS, a trusted public certificate,
a reverse proxy, firewall rules, or internet authentication. Do not expose this
mode directly to an untrusted network without designing those layers.

## Retention and sign-out

```bash
mj server \
  --history-days 7 \
  --session-ttl-days 2
```

- `--history-days 0` keeps disconnected session history indefinitely.
- `--session-ttl-days 0` makes viewer sessions ephemeral.
- `--logout-all` rotates the cookie signing key and signs every viewer out. The
  underlying QR/bearer login token remains available for reauthentication.

Remote state includes local SQLite session/transcript data, queued prompts,
permission decisions, authentication material, and certificates under
Mjolnir's platform state/config directories.

## Before leaving loopback

1. Decide who can reach port 11921 and enforce that with host or tailnet policy.
2. Protect the login token, cookie key, certificates, and transcript storage.
3. Set finite history and session lifetimes.
4. Confirm remote users understand the active workspace and permission mode.
5. Test `--logout-all` and recovery before relying on remote access.
6. Keep the host patched; the server can drive provider agents and answer nested permissions.

See [Data and trust boundaries](/data-boundaries/) and [Storage and network
activity](/storage-network/) for the complete surface.
