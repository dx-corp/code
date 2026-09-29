import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { findPrivateProtocolSources } from './check-public-protocol-source.mjs';

test('finds private RPC paths in release Rust source before tagging', t => {
  const root = mkdtempSync(join(tmpdir(), 'maestro-protocol-source-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, 'packages', 'cloud'), { recursive: true });
  writeFileSync(join(root, 'packages', 'cloud', 'client.rs'),
    'const PUBLIC: &str = "deixicpublic.v1.DeixicPublicService";\nconst PRIVATE: &str = "deixic.v1.DeixicService";\n');
  assert.deepEqual(findPrivateProtocolSources(root), ['packages/cloud/client.rs:2: deixic.v1.DeixicService']);
  writeFileSync(join(root, 'packages', 'cloud', 'client.rs'),
    'const PUBLIC: &str = "deixicpublic.v1.DeixicPublicService";\n');
  assert.deepEqual(findPrivateProtocolSources(root), []);
});
