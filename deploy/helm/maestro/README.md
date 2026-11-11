# Deixic Code Helm runtime

This chart runs the standalone native Deixic Code image. It does not promote
Platform's separately signed `platform/maestro` image or dispatch a Platform
production deployment.

The image publisher creates `main`, `latest`, and full/short `sha-*` tags.
An empty `image.tag` uses `main` with `pullPolicy: Always`. For a repeatable
release, set `image.digest` or `image.tag` to the reviewed immutable image.
`image.digest` takes precedence over a tag. Registry access is configured
separately from this chart; successful publication does not prove anonymous pull.

`service.port` sets the Service, container/probe port and native server's `PORT`.
The chart generates or preserves `MAESTRO_WEB_API_KEY` in its existing Secret.
Liveness uses `/healthz`; readiness uses `/readyz` and returns 503 after drain.

The default preStop runs `maestro drain` from the native image. It sends the
existing API key to the loopback control endpoint without a proxy or redirect.
Remote tenant credentials cannot drain the process. A successful acknowledgement
means the process stopped admitting new work; the command waits up to 25 seconds
for accepted foreground and background owners to finish. Controls for accepted
turns, including approvals and cancellation, remain authorized by their existing
owners. If work remains, the command exits unsuccessfully and readiness stays off.

SIGTERM and SIGINT stop listening and wait ten seconds for accepted work. The
server then requests cancellation through the existing turn owners and waits
five more seconds. Completed shutdown returns success. A deadline with work still
running returns an error and records an `incomplete` daemon exit receipt when
`MAESTRO_DAEMON_EXIT_RECEIPT` is configured. It does not claim those turns finished.
The native entry points bound final runtime teardown to five seconds, including
blocking I/O workers. The chart's 60-second termination grace covers the preStop,
server and runtime deadlines.

The prepared public mirror guardrail runs the chart render contract with its
pinned Helm toolchain before opening a sync PR. To run it locally with Helm
installed:

```sh
node --test deploy/helm/maestro/tests/render.test.mjs
```
