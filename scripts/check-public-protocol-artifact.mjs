#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

// This supplements canonical source parity: prost does not embed a descriptor
// set. Inspect the exact native bytes for private schema and RPC namespaces.
const privateMarkers = [
  'console.v1', 'console/v1/', 'deixic.v1.DeixicService',
  'evalops.console', 'platform.v1', 'platform/v1/',
];
export function assertPublicProtocolArtifact(bytes, name = 'native artifact') {
  if (bytes.length === 0) throw new Error(`Empty ${name}`);
  for (const marker of privateMarkers) {
    if (bytes.includes(Buffer.from(marker))) {
      throw new Error(`Private protocol marker ${marker} in ${name}`);
    }
  }
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  if (process.argv.length < 3) throw new Error('Usage: check-public-protocol-artifact.mjs BINARY...');
  for (const path of process.argv.slice(2)) assertPublicProtocolArtifact(readFileSync(path), path);
  console.log('Native public protocol artifact audit passed');
}
