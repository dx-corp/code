import test from 'node:test';
import assert from 'node:assert/strict';
import { assertPublicProtocolArtifact } from './check-public-protocol-artifact.mjs';

test('allows public native RPC bytes and rejects private schema and route bytes', () => {
  assertPublicProtocolArtifact(Buffer.from('\0/deixicpublic.v1.DeixicPublicService/GetThread\0maestro.v1\0'));
  for (const marker of ['console.v1', 'console/v1/console.proto', 'deixic.v1.DeixicService', 'platform/v1/internal.proto']) {
    assert.throws(() => assertPublicProtocolArtifact(Buffer.from(`\0${marker}\0`)), /Private protocol/);
  }
  assert.throws(() => assertPublicProtocolArtifact(Buffer.alloc(0)), /Empty/);
});
