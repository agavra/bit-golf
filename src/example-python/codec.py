#!/usr/bin/env python3
"""
Example external codec for compression-golf.
This is a naive JSON + zlib implementation for testing the harness.
"""
import sys
import json
import zlib

def encode():
    """Read JSON events from stdin, write compressed bytes to stdout."""
    lines = sys.stdin.read()
    compressed = zlib.compress(lines.encode('utf-8'), level=9)
    sys.stdout.buffer.write(compressed)

def decode():
    """Read compressed bytes from stdin, write JSON events to stdout."""
    compressed = sys.stdin.buffer.read()
    decompressed = zlib.decompress(compressed)
    sys.stdout.write(decompressed.decode('utf-8'))

if __name__ == '__main__':
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <encode|decode>", file=sys.stderr)
        sys.exit(1)

    command = sys.argv[1]
    if command == 'encode':
        encode()
    elif command == 'decode':
        decode()
    else:
        print(f"Unknown command: {command}", file=sys.stderr)
        sys.exit(1)
