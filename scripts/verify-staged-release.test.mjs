import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import {
  releaseCertificateIdentityRegexp,
  stagedFiles,
  verifyStagedFiles,
} from './verify-staged-release.mjs';

test('release certificate identity accepts only the exact Mono workflow in either org alias', () => {
  const identity = new RegExp(releaseCertificateIdentityRegexp);
  assert.match('https://github.com/evalops/mono/.github/workflows/maestro-release.yml@refs/heads/main', identity);
  assert.match('https://github.com/dx-corp/mono/.github/workflows/maestro-release.yml@refs/heads/main', identity);
  assert.doesNotMatch('https://github.com/attacker/mono/.github/workflows/maestro-release.yml@refs/heads/main', identity);
  assert.doesNotMatch('https://github.com/dx-corp/mono/.github/workflows/release.yml@refs/heads/main', identity);
  assert.doesNotMatch('https://github.com/dx-corp/mono/.github/workflows/maestro-release.yml@refs/heads/feature', identity);
});

function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), 'staged-release-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  for (const name of stagedFiles) writeFileSync(join(dir, name), name);
  const digest = name => createHash('sha256').update(readFileSync(join(dir, name))).digest('hex');
  writeFileSync(join(dir, 'release-metadata.json'), JSON.stringify({version:'0.10.72', releaseTag:'v0.10.72', receipt:{sourceSha:'a'.repeat(40)}}));
  for (const p of ['darwin-x64', 'darwin-arm64']) writeFileSync(join(dir, `notarized-${p}.json`), JSON.stringify({schema:'evalops.maestro.macos-notarization.v1',status:'Accepted',platform:p,binarySha256:digest(`maestro-${p}`)}));
  writeFileSync(join(dir, 'package.json'), '{"version":"0.10.72"}');
  writeFileSync(join(dir, 'release-source-manifest.json'), JSON.stringify({schemaVersion:1,files:[{path:'package.json',sha256:digest('package.json')}]}));
  for (const p of ['darwin-x64', 'darwin-arm64']) writeFileSync(join(dir, `code-device-${p}.json`), JSON.stringify({schemaVersion:1,platform:p,enabled:false}));
  const seal = () => writeFileSync(join(dir, 'MONO_SHA256SUMS'), stagedFiles.map(name => `${digest(name)}  ${name}`).join('\n')+'\n');
  seal();
  return {dir, seal};
}
test('accepts complete content matching the authenticated manifest', t => {
  const {dir} = fixture(t);
  assert.equal(verifyStagedFiles(dir, '0.10.72', dir).receipt.sourceSha, 'a'.repeat(40));
});
test('rejects changed binary bytes', t => {
  const {dir} = fixture(t); writeFileSync(join(dir,'maestro-darwin-arm64'),'changed');
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Checksum mismatch/);
});
test('rejects a different requested release', t => {
  assert.throws(() => verifyStagedFiles(fixture(t).dir,'0.10.73'), /version or source/);
});
test('requires every platform even when the checksum list omits one', t => {
  const {dir} = fixture(t);
  writeFileSync(join(dir,'MONO_SHA256SUMS'),readFileSync(join(dir,'MONO_SHA256SUMS'),'utf8').split('\n').filter(l => !l.endsWith('  maestro-darwin-x64')).join('\n'));
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Checksum mismatch/);
});
test('rejects path traversal and duplicate manifest entries', t => {
  const {dir} = fixture(t); const path=join(dir,'MONO_SHA256SUMS'); const original=readFileSync(path,'utf8');
  for (const extra of [`${'a'.repeat(64)}  ../secret`, original.split('\n')[0]]) {
    writeFileSync(path,original+extra+'\n');
    assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Invalid or duplicate/);
  }
});
test('rejects signed but rejected notarization receipts', t => {
  const {dir,seal} = fixture(t); const path=join(dir,'notarized-darwin-arm64.json');
  const marker=JSON.parse(readFileSync(path,'utf8')); marker.status='Rejected'; writeFileSync(path,JSON.stringify(marker));seal();
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Invalid notarization/);
});
test('rejects notarization for different binary bytes even with valid checksums', t => {
  const {dir,seal} = fixture(t); writeFileSync(join(dir,'maestro-darwin-arm64'),'new binary');seal();
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Invalid notarization/);
});

test('rejects a public source tag with the same version but different content', t => {
  const {dir}=fixture(t); writeFileSync(join(dir,'package.json'), '{"version":"0.10.72","changed":true}');
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Public source mismatch/);
});
test('rejects traversal in an authenticated source manifest', t => {
  const {dir,seal}=fixture(t);
  writeFileSync(join(dir,'release-source-manifest.json'),JSON.stringify({schemaVersion:1,files:[{path:'../outside',sha256:'a'.repeat(64)}]}));seal();
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Invalid source path/);
});

test('enabled Code device requires an authenticated helper archive', t => {
  const {dir,seal}=fixture(t);
  writeFileSync(join(dir,'code-device-darwin-arm64.json'), JSON.stringify({schemaVersion:1,platform:'darwin-arm64',enabled:true})); seal();
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /ENOENT/);
  const name='deixic-code-device-darwin-arm64.app.tar.gz'; writeFileSync(join(dir,name),'helper');
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Checksum mismatch/);
  const digest=createHash('sha256').update('helper').digest('hex');
  writeFileSync(join(dir,'MONO_SHA256SUMS'),readFileSync(join(dir,'MONO_SHA256SUMS'),'utf8')+`${digest}  ${name}\n`);
  assert.equal(verifyStagedFiles(dir,'0.10.72',dir).version,'0.10.72');
});
test('disabled Code device rejects an injected helper', t => {
  const {dir}=fixture(t); writeFileSync(join(dir,'deixic-code-device-darwin-arm64.app.tar.gz'),'helper');
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Unexpected disabled/);
});
test('Code device capability must be a typed per-platform receipt', t => {
  const {dir,seal}=fixture(t); writeFileSync(join(dir,'code-device-darwin-arm64.json'),JSON.stringify({schemaVersion:1,platform:'darwin-arm64',enabled:'false'})); seal();
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Invalid Code device/);
});

test('rejects a sealed native binary containing a private protocol', t => {
  const {dir,seal}=fixture(t);
  writeFileSync(join(dir,'maestro-linux-x64'), 'binary\0console.v1\0'); seal();
  assert.throws(() => verifyStagedFiles(dir,'0.10.72',dir), /Private protocol/);
});
