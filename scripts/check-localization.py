#!/usr/bin/env python3
"""Check the native display catalog without a translation service or Rust build."""
from __future__ import annotations
import argparse
import json
from pathlib import Path
import re

STRING = r'"(?:[^"\\]|\\.)*"'
ENTRY = re.compile(r'\(\s*(' + STRING + r'),\s*\[\s*(' + STRING + r'(?:\s*,\s*' + STRING + r'){5})\s*,?\s*\]\s*,?\s*\)', re.S)
CALL = re.compile(r'(?:\btr|\.translate|\blocale\.format|cli_locale\(\)\.format|localization::format)\s*\(\s*(' + STRING + r')')
FIELDS = re.compile(r'\{[0-9]+\}')
TOKENS = re.compile(r'`[^`\n]+`|/[a-z][a-z0-9_-]*|--[a-z][a-z0-9_-]*|<[^>\n]+>|(?:Ctrl|Alt|Shift)(?:\+[A-Za-z0-9]+)+')


# This guard covers direct application-owned rendering and status text. It does
# not guess whether arbitrary model/tool data or metadata is translatable.
DISPLAY_CALL = re.compile(
    r'(?:Span::(?:raw|styled)|Line::(?:from|styled)|Paragraph::new|'
    r'\.(?:title|help|set_status|add_system_message)|\.status\s*\.replace)'
    r'\(\s*(?:format!\(\s*)?(' + STRING + r')', re.S)
# Product names and literal keyboard bindings are stable display tokens.
DISPLAY_TOKENS = {'Dex', 'Ctrl+C'}
RUST_STRING = r'r(?P<hashes>#{0,16})"[\s\S]*?"(?P=hashes)|' + STRING.replace(r'\\.', r'\\[\s\S]') + r"|'(?:\\.|[^'\\\n])'"


def application_source(text: str) -> str:
    """Mask Rust comments and test modules while preserving source positions."""
    # Consume strings first so URLs and comment markers inside copy stay intact.
    tokens = re.compile(RUST_STRING + r'|//[^\n]*|/\*[\s\S]*?\*/')
    text = tokens.sub(lambda m: re.sub(r'[^\n]', ' ', m[0])
                      if m[0].startswith(('/', '/*')) else m[0], text)
    structure = tokens.sub(lambda m: re.sub(r'[^\n]', ' ', m[0]), text)
    pattern = re.compile(r'#\[cfg\(test\)\]\s*mod\s+\w+\s*\{')
    while match := pattern.search(structure):
        depth, pos = 1, match.end()
        pieces = re.compile(r'[{}]')
        for token in pieces.finditer(structure, pos):
            if token[0] == '{':
                depth += 1
            elif token[0] == '}':
                depth -= 1
            if depth == 0:
                pos = token.end()
                break
        else:
            raise ValueError('Unclosed Rust test module')
        structure = structure[:match.start()] + re.sub(r'[^\n]', ' ', structure[match.start():pos]) + structure[pos:]
        text = text[:match.start()] + re.sub(r'[^\n]', ' ', text[match.start():pos]) + text[pos:]
    return text


def unlocalized_display(text: str) -> list[tuple[int, str]]:
    text = application_source(text)
    return [(text.count('\n', 0, match.start()) + 1, json.loads(match[1]))
            for match in DISPLAY_CALL.finditer(text)
            if re.search(r'[A-Za-z]{2}', re.sub(r'\{[^{}]*\}', '', json.loads(match[1])))
            and json.loads(match[1]) not in DISPLAY_TOKENS]


def check(root: Path) -> list[str]:
    catalog = root / 'packages/ui-rs/src/translations.rs'
    entries = [(json.loads(key), [json.loads(x) for x in re.findall(STRING, values)])
               for key, values in ENTRY.findall(catalog.read_text())]
    errors: list[str] = []
    keys = [key for key, _ in entries]
    if len(keys) < 2000:
        errors.append('The full catalog is missing (expected more than 2,000 messages).')
    if keys != sorted(set(keys)):
        errors.append('Catalog keys must be unique and sorted.')
    for key, values in entries:
        for language, value in zip(('es', 'fr', 'de', 'ja', 'ko', 'zh-CN'), values):
            if not value.strip():
                errors.append(f'{language}: empty translation for {key!r}')
            if sorted(FIELDS.findall(key)) != sorted(FIELDS.findall(value)):
                errors.append(f'{language}: placeholder mismatch for {key!r}')
            for token in set(TOKENS.findall(key)):
                if value.count(token) < key.count(token):
                    errors.append(f'{language}: changed command or binding {token!r} in {key!r}')
            if re.search(r'98765\d{3}', value):
                errors.append(f'{language}: unresolved editing marker for {key!r}')
    known = set(keys)
    for package in ('maestro-rs', 'tui-rs', 'ui-rs', 'presentation-rs'):
        for path in (root / 'packages' / package / 'src').rglob('*.rs'):
            if path.name in ('localization.rs', 'translations.rs'):
                continue
            text = path.read_text()
            if not ((path.stem == 'tests' or path.stem.endswith(('_test', '_tests'))) or 'tests' in path.parts):
                for line, key in unlocalized_display(text):
                    errors.append(f'{path.relative_to(root)}:{line}: unlocalized display literal {key!r}')
            for match in CALL.finditer(text):
                key = json.loads(match[1])
                # Templates containing only dynamic values need no translation.
                if not re.search(r'[A-Za-z]', FIELDS.sub('', key)):
                    continue
                if key not in known:
                    line = text.count('\n', 0, match.start()) + 1
                    errors.append(f'{path.relative_to(root)}:{line}: missing catalog key {key!r}')
    metadata = root / 'packages/cli-commands-rs/src/mcp_catalog.rs'
    for match in re.finditer(r'(?:description|category):\s*(' + STRING + r')', metadata.read_text()):
        key = json.loads(match[1])
        if key not in known:
            errors.append(f'MCP catalog metadata is missing translations: {key!r}')
    if not errors:
        print(f'{len(keys)} messages; six translation catalogs; placeholders, literal call sites, and direct display text verified.')
    return errors


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    problems = check(args.root)
    for problem in problems:
        print(problem)
    raise SystemExit(bool(problems))
