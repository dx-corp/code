#!/usr/bin/env node
import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const PRIVATE_MARKERS = [
  'console.v1', 'console/v1/', 'deixic.v1.DeixicService',
  'evalops.console', 'platform.v1', 'platform/v1/',
];

export function findPrivateProtocolSources(root) {
  const violations = [];
  function visit(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) visit(path);
      else if (entry.isFile() && entry.name.endsWith('.rs')) {
        const source = readFileSync(path, 'utf8');
        for (const marker of PRIVATE_MARKERS) {
          let offset = -1;
          while ((offset = source.indexOf(marker, offset + 1)) !== -1) {
            const line = source.slice(0, offset).split('\n').length;
            violations.push(`${relative(root, path)}:${line}: ${marker}`);
          }
        }
      }
    }
  }
  visit(join(root, 'packages'));
  return violations;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const violations = findPrivateProtocolSources(ROOT);
  if (violations.length) {
    console.error(`Private protocol references in public Maestro Rust source:\n${violations.join('\n')}`);
    process.exitCode = 1;
  } else {
    console.log('Public Maestro Rust source protocol audit passed');
  }
}
