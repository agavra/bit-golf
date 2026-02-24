# Multi-Language Codec Support Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable non-Rust codec submissions via Docker containers that implement a standard encode/decode ABI.

**Architecture:** The Rust harness discovers external codecs by scanning for `src/*/Dockerfile`, builds each container, then invokes encode/decode via stdin/stdout. Results integrate into the existing output table.

**Tech Stack:** Rust (std::process::Command for Docker orchestration), Docker containers for external codecs.

---

## Task 1: Add Docker CLI Flag and External Codec Discovery

**Files:**
- Modify: `src/main.rs:147-168` (argument parsing)

**Step 1: Add --docker flag parsing**

In the argument parsing section of main.rs, add a `docker_enabled` flag:

```rust
fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();

    let mut path = "data.json".to_string();
    let mut codec_filter: Option<String> = None;
    let mut docker_enabled = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--codec" => {
                if i + 1 < args.len() {
                    codec_filter = Some(args[i + 1].to_lowercase());
                    i += 1;
                }
            }
            "--docker" => {
                docker_enabled = true;
            }
            arg if !arg.starts_with('-') => {
                path = arg.to_string();
            }
            _ => {}
        }
        i += 1;
    }
```

**Step 2: Run harness to verify flag is accepted**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo run --release -- --docker 2>&1 | head -5`
Expected: Compiles and runs without error (flag parsed but not used yet)

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add --docker CLI flag for external codec support"
```

---

## Task 2: Add External Codec Discovery Function

**Files:**
- Modify: `src/main.rs` (add function before main)

**Step 1: Add discover_external_codecs function**

Add this function before the `main()` function:

```rust
fn discover_external_codecs() -> Vec<String> {
    let src_path = std::path::Path::new("src");
    let mut codecs = Vec::new();

    if let Ok(entries) = std::fs::read_dir(src_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dockerfile = path.join("Dockerfile");
                if dockerfile.exists() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        codecs.push(name.to_string());
                    }
                }
            }
        }
    }

    codecs.sort();
    codecs
}
```

**Step 2: Run build to verify it compiles**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo build --release 2>&1 | tail -3`
Expected: `Finished` with no errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add external codec discovery via Dockerfile scanning"
```

---

## Task 3: Add Docker Build Function

**Files:**
- Modify: `src/main.rs` (add function)

**Step 1: Add build_docker_codec function**

Add this function after `discover_external_codecs`:

```rust
fn build_docker_codec(name: &str) -> Result<(), Box<dyn Error>> {
    let path = format!("src/{}", name);

    let status = std::process::Command::new("docker")
        .args(["build", "-t", name, &path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()?;

    if !status.success() {
        return Err(format!("Docker build failed for {}", name).into());
    }

    Ok(())
}
```

**Step 2: Run build to verify it compiles**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo build --release 2>&1 | tail -3`
Expected: `Finished` with no errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add docker build function for external codecs"
```

---

## Task 4: Add Docker Encode/Decode Functions

**Files:**
- Modify: `src/main.rs` (add functions)

**Step 1: Add run_docker_encode function**

Add this function after `build_docker_codec`:

```rust
fn run_docker_encode(image: &str, events: &[(EventKey, EventValue)]) -> Result<Vec<u8>, Box<dyn Error>> {
    use std::io::Write;

    let mut child = std::process::Command::new("docker")
        .args(["run", "-i", "--rm", image, "encode"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // Write events as line-delimited JSON to stdin
    {
        let stdin = child.stdin.as_mut().ok_or("Failed to open stdin")?;
        for (key, value) in events {
            let event = serde_json::json!({
                "id": key.id,
                "type": key.event_type,
                "repo": value.repo,
                "created_at": value.created_at
            });
            writeln!(stdin, "{}", serde_json::to_string(&event)?)?;
        }
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Encode failed: {}", stderr).into());
    }

    Ok(output.stdout)
}
```

**Step 2: Add run_docker_decode function**

Add this function after `run_docker_encode`:

```rust
fn run_docker_decode(image: &str, data: &[u8]) -> Result<Vec<(EventKey, EventValue)>, Box<dyn Error>> {
    use std::io::Write;

    let mut child = std::process::Command::new("docker")
        .args(["run", "-i", "--rm", image, "decode"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().ok_or("Failed to open stdin")?;
        stdin.write_all(data)?;
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Decode failed: {}", stderr).into());
    }

    // Parse line-delimited JSON from stdout
    let stdout = String::from_utf8(output.stdout)?;
    let mut events = Vec::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let raw: RawGitHubEvent = serde_json::from_str(line)?;
        let key = EventKey {
            event_type: raw.event_type,
            id: raw.id,
        };
        let value = EventValue {
            repo: raw.repo,
            created_at: raw.created_at,
        };
        events.push((key, value));
    }

    Ok(events)
}
```

**Step 3: Run build to verify it compiles**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo build --release 2>&1 | tail -3`
Expected: `Finished` with no errors

**Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: add docker encode/decode execution functions"
```

---

## Task 5: Integrate External Codecs into Main Loop

**Files:**
- Modify: `src/main.rs:209-215` (after Rust codec loop, before table close)

**Step 1: Add external codec execution after Rust codecs**

Replace the table closing and success message (lines 211-214) with:

```rust
    // Run external Docker codecs if --docker flag is set
    if docker_enabled {
        let external_codecs = discover_external_codecs();
        for codec_name in external_codecs {
            // Skip if filter is set and doesn't match
            if let Some(ref filter) = codec_filter {
                if !codec_name.to_lowercase().contains(filter) {
                    continue;
                }
            }

            // Build the Docker image
            if let Err(e) = build_docker_codec(&codec_name) {
                println!(
                    "│ {:<22} │ {:>14} │ {:>10} │",
                    format!("{} [BUILD FAILED]", codec_name),
                    "-",
                    "-"
                );
                eprintln!("Build error for {}: {}", codec_name, e);
                continue;
            }

            // Run encode
            let encoded = match run_docker_encode(&codec_name, &events) {
                Ok(data) => data,
                Err(e) => {
                    println!(
                        "│ {:<22} │ {:>14} │ {:>10} │",
                        format!("{} [ENCODE FAILED]", codec_name),
                        "-",
                        "-"
                    );
                    eprintln!("Encode error for {}: {}", codec_name, e);
                    continue;
                }
            };

            print_row(&codec_name, encoded.len(), baseline);

            // Run decode and verify
            let decoded = match run_docker_decode(&codec_name, &encoded) {
                Ok(data) => data,
                Err(e) => {
                    eprintln!("Decode error for {}: {}", codec_name, e);
                    continue;
                }
            };

            // Sort decoded events for comparison (external codecs may not preserve order)
            let mut sorted_decoded = decoded.clone();
            sorted_decoded.sort_by(|a, b| a.0.cmp(&b.0));
            assert_events_eq(&codec_name, &sorted_events, &sorted_decoded);
        }
    }

    println!("└────────────────────────┴────────────────┴────────────┘");
    println!("\nAll verifications passed");

    Ok(())
}
```

**Step 2: Run build to verify it compiles**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo build --release 2>&1 | tail -3`
Expected: `Finished` with no errors

**Step 3: Run harness without --docker to verify existing behavior unchanged**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo run --release -- --codec naive 2>&1`
Expected: Shows Naive codec results, "All verifications passed"

**Step 4: Run harness with --docker (no external codecs yet)**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo run --release -- --docker --codec naive 2>&1`
Expected: Same as above (no external codecs discovered)

**Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: integrate external Docker codecs into main evaluation loop"
```

---

## Task 6: Auto-Enable Docker Mode for External Codec Targets

**Files:**
- Modify: `src/main.rs` (after argument parsing, before loading events)

**Step 1: Add auto-detection for external codec targets**

After the argument parsing loop but before `let events = load_events(&path)?;`, add:

```rust
    // Auto-enable docker mode if targeting an external codec
    if let Some(ref filter) = codec_filter {
        let potential_path = format!("src/{}/Dockerfile", filter);
        if std::path::Path::new(&potential_path).exists() {
            docker_enabled = true;
        }
    }
```

**Step 2: Run build to verify it compiles**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo build --release 2>&1 | tail -3`
Expected: `Finished` with no errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: auto-enable docker mode when targeting external codec"
```

---

## Task 7: Create Example Python Codec for Testing

**Files:**
- Create: `src/example-python/Dockerfile`
- Create: `src/example-python/codec.py`

**Step 1: Create the directory**

```bash
mkdir -p src/example-python
```

**Step 2: Create the Dockerfile**

Create `src/example-python/Dockerfile`:

```dockerfile
FROM python:3.11-slim
WORKDIR /app
COPY codec.py .
ENTRYPOINT ["python", "codec.py"]
```

**Step 3: Create a minimal Python codec**

Create `src/example-python/codec.py`:

```python
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
```

**Step 4: Verify Docker is available**

Run: `docker --version`
Expected: Docker version info (e.g., "Docker version 24.x.x")

**Step 5: Test the example codec manually**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && docker build -t example-python src/example-python/`
Expected: Successfully builds the image

**Step 6: Test encode/decode manually with small input**

Run: `echo '{"id":"1","type":"PushEvent","repo":{"id":123,"name":"test/repo","url":"https://api.github.com/repos/test/repo"},"created_at":"2024-01-01T00:00:00Z"}' | docker run -i --rm example-python encode | docker run -i --rm example-python decode`
Expected: Same JSON line output

**Step 7: Commit**

```bash
git add src/example-python/
git commit -m "feat: add example Python codec for testing external codec support"
```

---

## Task 8: End-to-End Test with Harness

**Files:**
- None (testing only)

**Step 1: Run harness with --docker targeting example-python**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo run --release -- --docker --codec example-python 2>&1`
Expected: Shows example-python in results table with size and vs Naive percentage, "All verifications passed"

**Step 2: Run harness with --docker to show all codecs**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo run --release -- --docker 2>&1`
Expected: Shows all Rust codecs plus example-python, all verifications pass

**Step 3: Verify Naive still works standalone**

Run: `cd /Users/jedgington/github/jakedgy/compression-golf/.worktrees/multi-language && cargo run --release -- --codec naive 2>&1`
Expected: Shows Naive results only, no Docker codecs run

---

## Task 9: Update CI Workflow for Docker Support

**Files:**
- Modify: `.github/workflows/pr.yml`

**Step 1: Update detection to include Docker codecs**

Replace the entire pr.yml with:

```yaml
name: PR Validation

on:
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always

jobs:
  test:
    name: Test Submissions
    runs-on: ubuntu-latest

    steps:
      - name: Checkout code
        uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Detect changed codecs
        id: changed
        run: |
          # Get list of changed Rust codec files (excluding main.rs, codec.rs, lib.rs)
          RUST_CHANGES=$(git diff --name-only origin/${{ github.base_ref }}...HEAD -- 'src/*.rs' | grep -vE '(main|codec|lib)\.rs$' || true)

          # Get list of changed Docker codec directories
          DOCKER_CHANGES=$(git diff --name-only origin/${{ github.base_ref }}...HEAD -- 'src/*/Dockerfile' 'src/*/*.py' 'src/*/*.java' 'src/*/*.c' 'src/*/*.go' | xargs -n1 dirname 2>/dev/null | sort -u | xargs -n1 basename 2>/dev/null || true)

          RUST_CODECS=""
          DOCKER_CODECS=""

          if [ -n "$RUST_CHANGES" ]; then
            RUST_CODECS=$(echo "$RUST_CHANGES" | xargs -n1 basename | sed 's/\.rs$//' | tr '\n' ' ')
          fi

          if [ -n "$DOCKER_CHANGES" ]; then
            DOCKER_CODECS=$(echo "$DOCKER_CHANGES" | tr '\n' ' ')
          fi

          echo "Rust codecs: $RUST_CODECS"
          echo "Docker codecs: $DOCKER_CODECS"
          echo "rust_codecs=$RUST_CODECS" >> $GITHUB_OUTPUT
          echo "docker_codecs=$DOCKER_CODECS" >> $GITHUB_OUTPUT

      - name: Setup Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo registry
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-

      - name: Decompress dataset
        run: gunzip -k data.json.gz

      - name: Build
        run: cargo build --release

      - name: Run Rust codec tests
        if: steps.changed.outputs.rust_codecs != ''
        run: |
          for codec in ${{ steps.changed.outputs.rust_codecs }}; do
            echo "Testing Rust codec: $codec"
            cargo run --release -- --codec "$codec" 2>&1 | tee -a results.txt
          done

      - name: Run Docker codec tests
        if: steps.changed.outputs.docker_codecs != ''
        run: |
          for codec in ${{ steps.changed.outputs.docker_codecs }}; do
            echo "Testing Docker codec: $codec"
            cargo run --release -- --docker --codec "$codec" 2>&1 | tee -a results.txt
          done

      - name: Run all codecs (no changes detected)
        if: steps.changed.outputs.rust_codecs == '' && steps.changed.outputs.docker_codecs == ''
        run: |
          echo "No codec changes detected, running all codecs"
          cargo run --release -- --docker 2>&1 | tee results.txt

      - name: Check formatting
        run: cargo fmt --check
```

**Step 2: Commit**

```bash
git add .github/workflows/pr.yml
git commit -m "feat: update CI workflow to support Docker codec testing"
```

---

## Task 10: Update README with External Codec Instructions

**Files:**
- Modify: `README.md` (add section for external codecs)

**Step 1: Read current README**

Read the README.md to find the appropriate place to add external codec documentation.

**Step 2: Add external codec section**

Add a new section after the existing submission instructions:

```markdown
## External Codecs (Non-Rust)

You can submit codecs in any language by creating a Docker container that implements the encode/decode ABI.

### Directory Structure

```
src/<github-username>-<lang>/
├── Dockerfile
└── <your implementation files>
```

### ABI Requirements

Your container must accept `encode` or `decode` as the first argument:

```bash
# Encode: JSON events in via stdin, compressed bytes out via stdout
docker run <image> encode < events.json > compressed.bin

# Decode: Compressed bytes in via stdin, JSON events out via stdout
docker run <image> decode < compressed.bin > events.json
```

### Example Dockerfile (Python)

```dockerfile
FROM python:3.11-slim
WORKDIR /app
COPY codec.py .
ENTRYPOINT ["python", "codec.py"]
```

### Testing Locally

```bash
# Run with Docker support
cargo run --release -- --docker

# Test specific external codec
cargo run --release -- --codec <name>-<lang>
```
```

**Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add external codec submission instructions to README"
```

---

## Task 11: Remove Example Codec (Optional Cleanup)

**Files:**
- Delete: `src/example-python/` (if you don't want it in the final PR)

**Step 1: Decide whether to keep example codec**

The example-python codec is useful for:
- Demonstrating the ABI to contributors
- Testing CI workflow

If you want to remove it before merging:

```bash
rm -rf src/example-python
git add -A
git commit -m "chore: remove example codec (served its testing purpose)"
```

If you want to keep it as a reference implementation, skip this task.

---

## Summary

After completing all tasks, the harness will support:

1. **CLI**: `--docker` flag to enable external codecs
2. **Discovery**: Auto-scans `src/*/Dockerfile` for external codecs
3. **Execution**: Builds and runs containers via stdin/stdout
4. **Verification**: Same round-trip validation as Rust codecs
5. **CI**: Detects and tests both Rust and Docker codec changes
6. **Documentation**: README explains how to submit external codecs
