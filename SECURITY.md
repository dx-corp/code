# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Maestro, please report it responsibly.

**Email:** security@evalops.dev

Please include:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

## Response Timeline

- **Acknowledgement:** Within 2 business days
- **Initial assessment:** Within 5 business days
- **Fix or mitigation:** Depends on severity, but we aim for 30 days for critical issues

## Disclosure Policy

We follow a 90-day coordinated disclosure window. If you report a vulnerability, we ask that you:

1. Do not disclose publicly until we have released a fix or 90 days have passed
2. Do not exploit the vulnerability beyond what is necessary to demonstrate it
3. Do not access or modify other users' data

## Scope

This policy applies to the Maestro codebase and its official packages:

- `@evalops/maestro` and all `@evalops/*` packages
- Official Docker images (`ghcr.io/dx-corp/maestro`)
- The Maestro VS Code extension and JetBrains plugin

## Recognition

We appreciate security researchers who help keep Maestro safe. With your permission, we will acknowledge your contribution in the relevant release notes.

## Running the container image

`ghcr.io/dx-corp/maestro` binds the runtime gateway to `0.0.0.0`, so it requires
API-key auth and will refuse to start without it:

```bash
docker run --rm -p 3000:3000 \
  -e MAESTRO_WEB_API_KEY="$(openssl rand -hex 32)" \
  ghcr.io/dx-corp/maestro
```

Clients send the key as `Authorization: Bearer <key>` or `x-maestro-api-key`.
`MAESTRO_WEB_REQUIRE_KEY=0` is a loopback-only development switch; combining it
with a non-loopback `MAESTRO_CONTROL_HOST` is a startup error, not a downgrade.

## Local binds and DNS rebinding

When the runtime gateway is bound to a loopback address it validates the `Host`
header and answers `421 Misdirected Request` for anything that does not name
that interface. Without this check, an attacker can point a short-TTL DNS record
at `127.0.0.1` and have the victim's browser treat `http://evil.example:8080/`
as same-origin with the local server: no `Origin` header is sent on a
same-origin navigation, and the default local posture needs no credential.

`localhost`, any loopback IP literal, and the configured bind host are accepted,
with or without a port. If you front the server with a tunnel or a proxy that
rewrites `Host`, list the extra names:

```bash
MAESTRO_WEB_ALLOWED_HOSTS="maestro.tunnel.example,dev.box.internal" maestro serve
```

Non-loopback binds are not `Host`-checked, because the names that reach them are
not knowable from inside the process. API-key auth is the control there.
