#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, lstatSync, existsSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { verifySourceManifest } from './release-source-manifest.mjs';

export const platforms = ['linux-x64', 'linux-arm64', 'darwin-x64', 'darwin-arm64'];
export const releaseCertificateIdentityRegexp = '^https://github.com/(evalops|dx-corp)/mono/\\.github/workflows/maestro-release\\.yml@refs/heads/main$';
export const stagedFiles = [
  'release-metadata.json', 'release-source-manifest.json',
  ...platforms.flatMap(p => [`maestro-${p}`, `smoked-${p}.txt`, `rustc-${p}.txt`, `runtime-passport-maestro-${p}.json`]),
  ...platforms.filter(p => p.startsWith('darwin-')).flatMap(p => [`signed-${p}.json`, `notarized-${p}.json`, `code-device-${p}.json`]),
];

// Only interpret the checksum manifest after authenticating it with Cosign.
export function verifyStagedFiles(dir, version, sourceRoot = process.cwd()) {
  const sums = new Map();
  for (const line of readFileSync(join(dir, 'MONO_SHA256SUMS'), 'utf8').trim().split('\n')) {
    const match = /^([a-f0-9]{64})  ([A-Za-z0-9][A-Za-z0-9._-]*)$/.exec(line);
    if (!match || sums.has(match[2])) throw new Error('Invalid or duplicate checksum entry');
    sums.set(match[2], match[1]);
  }
  const verifyFile = name => {
    const path = join(dir, name);
    if (!lstatSync(path).isFile()) throw new Error(`Not a regular file: ${name}`);
    const digest = createHash('sha256').update(readFileSync(path)).digest('hex');
    if (sums.get(name) !== digest) throw new Error(`Checksum mismatch: ${name}`);
  };
  for (const name of stagedFiles) verifyFile(name);
  const metadata = JSON.parse(readFileSync(join(dir, 'release-metadata.json'), 'utf8'));
  if (metadata.version !== version || metadata.releaseTag !== `v${version}` || !/^[a-f0-9]{40}$/.test(metadata.receipt?.sourceSha ?? '')) {
    throw new Error('Staged release version or source does not match');
  }
  for (const p of platforms.filter(p => p.startsWith('darwin-'))) {
    const capability = JSON.parse(readFileSync(join(dir, `code-device-${p}.json`), 'utf8'));
    if (capability.schemaVersion !== 1 || capability.platform !== p || typeof capability.enabled !== 'boolean') {
      throw new Error(`Invalid Code device capability: ${p}`);
    }
    const helper = `deixic-code-device-${p}.app.tar.gz`;
    if (capability.enabled) verifyFile(helper);
    else if (sums.has(helper) || existsSync(join(dir, helper))) throw new Error(`Unexpected disabled Code device helper: ${p}`);
    const marker = JSON.parse(readFileSync(join(dir, `notarized-${p}.json`), 'utf8'));
    if (marker.schema !== 'evalops.maestro.macos-notarization.v1' || marker.status !== 'Accepted' || marker.platform !== p || marker.binarySha256 !== sums.get(`maestro-${p}`)) {
      throw new Error(`Invalid notarization receipt: ${p}`);
    }
  }
  verifySourceManifest(sourceRoot, JSON.parse(readFileSync(join(dir, 'release-source-manifest.json'), 'utf8')));
  return metadata;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const [dir, version] = process.argv.slice(2);
  if (!dir || !version) throw new Error('Usage: verify-staged-release.mjs DIRECTORY VERSION');
  execFileSync('cosign', ['verify-blob', '--bundle', join(dir, 'MONO_SHA256SUMS.cosign.bundle'),
    '--certificate-identity-regexp', releaseCertificateIdentityRegexp,
    '--certificate-oidc-issuer', 'https://token.actions.githubusercontent.com', join(dir, 'MONO_SHA256SUMS')], { stdio: 'inherit' });
  const metadata = verifyStagedFiles(dir, version);
  execFileSync(process.execPath, [new URL('./verify-release-smoke-coverage.mjs', import.meta.url).pathname, dir], {
    stdio: 'inherit', env: { ...process.env, MAESTRO_RELEASE_PLATFORMS: platforms.join(' ') },
  });
  console.log(`Verified staged release ${version} from Mono ${metadata.receipt.sourceSha}`);
}
