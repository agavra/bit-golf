# Multi-Language Codec Support

## Goal

Maximize participation in the compression-golf competition by allowing submissions in languages other than Rust (Python, Java, C, Go, etc.).

## Overview

External codecs are Docker containers that implement a standard ABI. The existing Rust harness orchestrates building and running these containers, then verifies correctness using the same round-trip validation as Rust codecs.

## ABI Specification

External codecs are Docker containers with an entrypoint that accepts two subcommands:

```bash
# Encode: JSON events in via stdin, compressed bytes out via stdout
docker run <image> encode < events.json > compressed.bin

# Decode: Compressed bytes in via stdin, JSON events out via stdout
docker run <image> decode < compressed.bin > events.json
```

### Input/Output Format

- **Encode input**: Line-delimited JSON (same format as `data.json`)
- **Encode output**: Raw bytes (the codec's proprietary compressed format)
- **Decode input**: The exact bytes produced by encode
- **Decode output**: Line-delimited JSON (must match original input exactly)

### Exit Codes

- `0` = success
- Non-zero = error (harness captures stderr for diagnostics)

### Verification

The harness calls encode, then decode, then compares the decoded output to the original input. If they match byte-for-byte, the submission is valid. This is the same integrity guarantee as Rust submissions.

## Directory Structure

External codecs use a convention-based directory structure:

```
src/
├── alice.rs              # Rust codec (existing)
├── bob.rs                # Rust codec (existing)
├── alice-python/         # External codec: name=alice, lang=python
│   ├── Dockerfile
│   ├── codec.py
│   └── requirements.txt
├── carol-java/           # External codec: name=carol, lang=java
│   ├── Dockerfile
│   └── Codec.java
└── dave-c/               # External codec: name=dave, lang=c
    ├── Dockerfile
    └── codec.c
```

### Naming Convention

- Directory name format: `{name}-{lang}`
- `name` can match an existing Rust codec (e.g., `alice.rs` and `alice-python/` can coexist)
- `lang` should be from common identifiers: `python`, `java`, `c`, `cpp`, `go`, `rust`, etc.

### Auto-Discovery

The harness discovers external codecs by scanning for `src/*/Dockerfile`. No configuration file needed.

## Dockerfile Requirements

1. Must accept `encode` or `decode` as the command argument
2. Must read from stdin, write to stdout
3. Must exit 0 on success, non-zero on failure

### Example: Python

```dockerfile
FROM python:3.11-slim
WORKDIR /app
COPY . .
RUN pip install -r requirements.txt
ENTRYPOINT ["python", "codec.py"]
```

### Example: Java

```dockerfile
FROM eclipse-temurin:21
WORKDIR /app
COPY . .
RUN javac Codec.java
ENTRYPOINT ["java", "Codec"]
```

### Example: C

```dockerfile
FROM gcc:13
WORKDIR /app
COPY . .
RUN gcc -O3 -o codec codec.c
ENTRYPOINT ["./codec"]
```

## CLI Interface

### Flags

```bash
# Default: Rust codecs only (current behavior)
cargo run --release

# Include external Docker codecs
cargo run --release -- --docker

# Target specific codec - harness auto-detects type
cargo run --release -- --codec alice          # finds alice.rs (Rust)
cargo run --release -- --codec alice-python   # finds src/alice-python/Dockerfile (Docker)
```

### Codec Resolution

When `--codec <name>` is specified:

1. Check if `name` is a registered Rust codec → run as Rust
2. Check if `src/{name}/Dockerfile` exists → run as Docker
3. Neither → error: "Unknown codec: {name}"

Targeting a Docker codec with `--codec` implicitly enables Docker mode.

## Harness Implementation

The Rust harness handles the full lifecycle:

```rust
fn build_external_codec(name: &str) -> Result<String> {
    let path = format!("src/{}", name);

    Command::new("docker")
        .args(["build", "-t", name, &path])
        .status()?;

    Ok(name.to_string())
}

fn run_encode(image: &str, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("docker")
        .args(["run", "-i", "--rm", image, "encode"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    child.stdin.take().unwrap().write_all(input)?;
    let output = child.wait_with_output()?;

    if !output.status.success() {
        return Err(/* error with stderr */);
    }

    Ok(output.stdout)
}

fn run_decode(image: &str, input: &[u8]) -> Result<Vec<u8>> {
    // Same pattern as run_encode but with "decode" argument
}
```

## CI/CD Integration

### PR Workflow Updates

The workflow detects both Rust and Docker codec changes:

```yaml
# Detect Rust codec changes (existing)
rust_changes=$(git diff --name-only origin/main...HEAD -- 'src/*.rs')

# Detect Docker codec changes (new)
docker_changes=$(git diff --name-only origin/main...HEAD -- 'src/*/Dockerfile' 'src/*/**')
```

### Workflow Behavior

| Change detected | Action |
|-----------------|--------|
| `src/alice.rs` | `cargo run --release -- --codec alice` |
| `src/alice-python/*` | `cargo run --release -- --codec alice-python` |
| Both | Run both targeted tests |

GitHub Actions runners have Docker pre-installed, so no special setup is needed.

### Constraints

- Timeout: Uses existing CI timeout (harness inherits runner limits)
- Build on every PR (can optimize with caching later)

## Error Handling

| Error | Harness response |
|-------|------------------|
| `docker build` fails | Report build error, show stderr, skip codec |
| `encode` times out | Report timeout, skip codec |
| `encode` exits non-zero | Report error, show stderr, skip codec |
| `decode` fails | Report decode error, skip codec |
| Round-trip mismatch | Report verification failure, show sample mismatches |

### Output Format

External codecs appear in the same results table as Rust codecs:

```
┌────────────────────────┬────────────────┬────────────┐
│ Codec                  │           Size │ vs Naive   │
├────────────────────────┼────────────────┼────────────┤
│ Naive                  │    210,727,389 │ baseline   │
│ XiangpengHao           │      6,847,283 │ -96.7%     │
│ alice-python           │      7,102,445 │ -96.6%     │
│ bob-java               │      8,234,112 │ -96.1%     │
│ carol-c [BUILD FAILED] │              - │ -          │
└────────────────────────┴────────────────┴────────────┘
```

### Debugging

- `--verbose` flag shows full Docker build output and stderr
- Failed codecs don't block other codecs from running
- Temporary files preserved on failure for inspection

## Submission Process

To submit an external codec:

1. Create `src/<github-username>-<lang>/` directory
2. Add a `Dockerfile` implementing the ABI
3. Add your codec implementation
4. Test locally: `cargo run --release -- --codec <name>`
5. Submit PR with the new directory

## Future Optimizations

- Docker layer caching in CI
- Pre-built base images for common languages
- Optional memory limits via `docker run --memory`
- Image registry for faster CI (authors push pre-built images)
