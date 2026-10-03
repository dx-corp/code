# T3 Code adaptations

Source: https://github.com/pingdotgg/t3code

Reviewed revisions:

- `4f7760e6a0037b06917adaae1b8a220e3ee6e5cc`: bounded event buffers and diagnostics, capability enforcement, reconnect lifetime, lazy diff workers, and checkpoint overlap tests.
- `a5b34b25378fbbf90ce36ee101a11528ccf11c7e`: bounded terminal output windows, checkpoint-backed turn review, and deterministic provider restart replay scenarios.

Maestro adapts these mechanisms into its existing desktop, session, gateway, and native provider owners. The upstream application and TypeScript server runtime are not imported.

Second-batch references:

- `apps/server/src/terminal/OutputProtocol.ts`
- `apps/server/src/checkpointing/CheckpointDiffQuery.ts`
- `apps/server/src/orchestration-v2/testkit/ProviderReplayHarness.ts`
- `apps/server/src/orchestration-v2/testkit/OrchestratorReplayRecovery.integration.test.ts`

The original MIT copyright and license are retained in `LICENSE`. Provider fixtures authored for Maestro are synthetic, scrubbed protocol scenarios; they do not claim to be recordings of live upstream or customer sessions.
