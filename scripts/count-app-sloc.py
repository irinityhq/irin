#!/usr/bin/env python3
"""Count tracked application/tooling physical lines at a Git commit.

Usage: python3 scripts/count-app-sloc.py --ref HEAD
       python3 scripts/count-app-sloc.py --self-test

JSON includes per-file exclusions and bucket totals. Tests, proof fixtures and
literal cfg(test)/cfg(kani) items are excluded; optional production features
(including test-helpers) remain included. Unknown test cfg forms fail closed.
Use this same script revision on both commits when comparing source size.
"""

import argparse
import collections
import json
from pathlib import Path, PurePosixPath
import re
import subprocess

EXTENSIONS = {'.rs', '.lua', '.ts', '.tsx', '.js', '.mjs', '.cjs', '.py',
              '.sh', '.swift', '.css', '.html'}
BUCKETS = ('gateway/sidecar-rs', 'council-rs/src', 'council-rs/warroom/web',
           'council-rs/warroom-tauri/src-tauri/src', 'gateway/lua', 'scripts',
           'packaging', 'tools')
TEST_NAME = re.compile(r'(^tests?\.|^test[-_]|_tests?\.|\.(test|spec)\.|_spec\.)')
LITERAL = re.compile(
    r'//[^\n]*|/\*|(?:br|r)(\#*)"|b?"(?:\\.|[^"\\])*"'
    r"|b?'(?:\\(?:u\{[0-9a-fA-F]+\}|x[0-9a-fA-F]{2}|.)|[^'\\])'", re.S)


def mask_literals(source):
    """Replace Rust comments/literals with spaces, retaining offsets/newlines."""
    masked, pos = list(source), 0
    while match := LITERAL.search(source, pos):
        start, end = match.span()
        if match.group() == '/*':
            depth = 1
            while depth:
                token = re.search(r'/\*|\*/', source[end:])
                if token is None:
                    raise ValueError('unterminated Rust comment')
                depth += 1 if token.group() == '/*' else -1
                end += token.end()
        elif match.group(1) is not None:
            closing = '"' + match.group(1)
            finish = source.find(closing, end)
            if finish < 0:
                raise ValueError('unterminated Rust raw string')
            end = finish + len(closing)
        masked[start:end] = ['\n' if c == '\n' else ' ' for c in source[start:end]]
        pos = end
    return ''.join(masked)


def rust_test_lines(source):
    """Return excluded line indexes; reject unsupported test attributes."""
    code, removed = mask_literals(source), set()
    for match in re.finditer(r'#\[cfg\((.*?)\)\]', code, re.S):
        expression = match.group(1).strip()
        if not re.search(r'\b(test|kani)\b', expression):
            continue
        if expression in ('not(test)', 'not(kani)'):
            continue
        if expression.startswith('not(any(test, feature ='):
            continue
        if expression not in ('test', 'kani') and not re.fullmatch(r'all\(test,\s*[^()]+\)', expression):
            # These items are also available in a supported production feature.
            original = source[match.start():match.end()]
            if re.fullmatch(r'#\[cfg\(any\(test,\s*feature\s*=\s*"test-helpers"\)\)\]', original):
                continue
            raise ValueError(f'unsupported test cfg at line {source.count(chr(10), 0, match.start()) + 1}')
        pos = match.end()
        while True:
            while pos < len(code) and code[pos].isspace():
                pos += 1
            if not code.startswith('#[', pos):
                break
            end = code.find(']', pos)
            if end < 0:
                raise ValueError('unterminated Rust attribute')
            pos = end + 1
        if re.match(r'(pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*;', code[pos:]):
            # External test modules must have a classified test/proof filename.
            module = re.match(r'(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)', code[pos:])[1]
            attrs = source[match.end():pos]
            path = re.search(r'#\[path\s*=\s*"([^"]+)"\]', attrs)
            filename = PurePosixPath(path[1]).name if path else module + '.rs'
            # build_support is also compiled by the production Tauri build.rs.
            if not TEST_NAME.search(filename) and filename not in ('kani_proofs.rs', 'build_support.rs'):
                raise ValueError(f'unclassified external test module: {filename}')
        depth, body = 0, False
        while pos < len(code):
            char = code[pos]
            pos += 1
            if char == '{':
                depth += 1
                body = True
            elif char == '}':
                depth -= 1
                if body and depth == 0:
                    break
            elif char == ';' and depth == 0:
                break
        else:
            raise ValueError('unterminated test item')
        first = source.count('\n', 0, match.start())
        last = source.count('\n', 0, pos)
        removed.update(range(first, last + 1))
    return removed


def exclusion(path, source):
    p = PurePosixPath(path)
    if any(part in ('test', 'tests', 'e2e', 'node_modules', 'target',
                    'warroom-web-dist', '.next', '.next-hosted') for part in p.parts):
        return 'test-or-generated-directory'
    if path.startswith(('packaging/build/', 'security/opengrep/fixtures/')):
        return 'generated-or-proof-fixture'
    if TEST_NAME.search(p.name) or p.name == 'kani_proofs.rs':
        return 'test-or-proof-file'
    if p.suffix not in EXTENSIONS and not (not p.suffix and source.startswith('#!')):
        return 'not-application-source'
    return None


def count(ref, root):
    def git(*args):
        return subprocess.check_output(['git', '-C', str(root), *args])
    commit = git('rev-parse', '--verify', ref + '^{commit}').decode().strip()
    entries = git('ls-tree', '-rz', commit).split(b'\0')
    files, totals = {}, collections.defaultdict(lambda: [0, 0, 0])
    for entry in filter(None, entries):
        metadata, raw_path = entry.split(b'\t', 1)
        mode, kind, oid = metadata.split()
        path = raw_path.decode()
        if kind != b'blob' or mode == b'120000':
            continue
        # Ignore non-source extensions before reading blobs (assets/config stay private).
        suffix = PurePosixPath(path).suffix
        if suffix and suffix not in EXTENSIONS:
            continue
        source = git('cat-file', 'blob', oid.decode()).decode('utf-8')
        reason = exclusion(path, source)
        raw = len(source.splitlines())
        if reason:
            files[path] = {'excluded': reason, 'raw': raw}
            continue
        try:
            tests = len(rust_test_lines(source)) if suffix == '.rs' else 0
        except ValueError as error:
            raise ValueError(f'{path}: {error}') from error
        bucket = next((b for b in BUCKETS if path.startswith(b + '/')), 'other')
        files[path] = {'raw': raw, 'test_only': tests, 'application': raw - tests, 'bucket': bucket}
        for i, value in enumerate((raw, tests, raw - tests)):
            totals[bucket][i] += value
    names = ('raw', 'test_only', 'application')
    return {'commit': commit,
            'totals': dict(zip(names, (sum(v[i] for v in totals.values()) for i in range(3)))),
            'buckets': {k: dict(zip(names, v)) for k, v in sorted(totals.items())},
            'files': files}


def self_test():
    sample = ('fn before() {}\n#[cfg(test)]\nmod tests {\n'
              ' let text = r##"} /*"##; // }\n /* { /* } */ } */\n'
              ' #[cfg(test)]\n fn nested() {}\n}\nfn after() {}\n')
    assert rust_test_lines(sample) == set(range(1, 8))
    assert rust_test_lines('#[cfg(test)]\n#[path = "some_tests.rs"]\nmod cases;\nfn live() {}\n') == {0, 1, 2}
    assert not rust_test_lines('#[cfg(not(test))]\nfn live() {}\n')
    assert exclusion('gateway/bin/arm', '#!/bin/sh\nexit 0\n') is None
    assert exclusion('x/test_parser.py', '') == 'test-or-proof-file'
    assert exclusion('x/e2e/support.ts', '') == 'test-or-generated-directory'
    assert exclusion('x/jcs/kani_proofs.rs', '') == 'test-or-proof-file'
    assert rust_test_lines('#[cfg(all(test, unix))]\nfn test_case() {}\n') == {0, 1}
    try:
        rust_test_lines('#[cfg(all(unix, any(test, kani)))]\nfn test_case() {}\n')
    except ValueError:
        pass
    else:
        raise AssertionError('unsupported cfg must refuse a misleading count')
    print('source counter self-test: PASS')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ref', default='HEAD')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        try:
            print(json.dumps(count(args.ref, Path(__file__).resolve().parents[1]), indent=2))
        except (ValueError, subprocess.CalledProcessError) as error:
            parser.exit(1, f'count refused: {error}\n')
