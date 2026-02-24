/*!
This codec implements a multi-layer compression strategy for GitHub timeline events,
combining data restructuring, delta encoding, backreferences, and entropy coding.
Test with `cargo run -r -- --codec lifthrasiir`.

It is no secret that a more powerful entropy coding can replace lots of context modeling;
the winner would probably have to use something like PAQ- or cmix-derived coders.
The first stage of this codec intentionally forgoes such "shortcuts" in favor of
explicit data transformations, 'cause it is more fun and interesting!
Eventually the second, heavyweight entropy stage was added for the record though.

The codec was primarily built with GLM-4.7 and Claude Code harness with human feedback.
No existing codec was referenced during development of the first stage; all ideas are original,
though they do tend to converge towards the local optimum for the obvious reasons.
For the second stage, Claude Sonnet 4.6 and Gemini 3 Flash were also heavily used for debugging.

## Core Principles

### 1. Struct of Arrays (SoA) Layout

Rather than storing events as an array of structs, the codec splits data into separate
parallel arrays (id_deltas, ts_deltas, types, backrefs, repo_ids, repo_users, repo_names).
This groups similar data types together, enabling better compression through locality
and simpler per-type encoding strategies.

Some fields underwent an additional restructuring, particularly ts_deltas and types
which are combined into a single bit-packed field. Most values of ts_deltas are zeroes,
so by storing the low 4 bits alongside the type (which fits in 4 bits), we mostly retain
the compressability of the original types array. Higher bits are stored separately as varints.

### 2. Delta Encoding

- **IDs**: Store only the difference from previous event (monotonically increasing)
- **Timestamps**: Use an adaptive delta scheme referencing the maximum of the two previous
  timestamps to handle one-off out-of-order events while maintaining small deltas

### 3. Combined Backreference Encoding

A single integer encodes both repository and owner references in three cases:
- `0`: New repository + new owner (emits both to string tables)
- `2n+1`: New repository + existing owner with index n
- `2n+2`: Existing repository with index n

This eliminates redundant repository/owner strings while allowing efficient reuse
of previously seen entities.

### 4. Case Compression

Eliminates uppercase letters in strings using two markers:
- `^` (caret): Next character is uppercase (shift)
- `$` (dollar): Latch mode for runs of consecutive uppercase, continues til non-alphanumeric

This is particularly effective for repository names following mixed-case conventions.

### 5. Bidirectional Encoding

When a repository name contains its owner name, the owner substring is replaced with `@`.
During decoding, `@` is substituted back with the actual owner string. This handles cases
like `tensorflow/tensorflow` efficiently.

Additionally, two special cases are recognized where the repository name is extremely
predictable: if the name after replacement is exactly `@`, it is stored as an empty string;
if it is `@.github.io`, it is stored as `@` (should be unambiguous due to the former case).

### 6. Transposed Encoding

For log-scaled multi-byte integers (repo_ids), bytes are stored transposed:
all first bytes, then all second bytes, etc. This groups similar bit-positions together
for better entropy compression.

### 7. Entropy Coding

- **Varint**: Custom leading-ones length prefix encoding for integers
- **Zigzag**: Converts signed deltas to unsigned for varint encoding
- **Zstandard/PAQ**: Final entropy compression layer, providing deduplication and entropy coding

Three different entropy stages were tested.
Timing was done in my workstation (Intel i7-7700, 48 GiB RAM, Windows 10).
Note that timing of PAQ algorithms is symmetric in both compression and decompression
as context model has to be run in both cases, hence no separate decompression time.

| Entropy stage             | Size            | Comp. Time |
|---------------------------|-----------------|------------|
| Zstandard (-22)           | 5,707,152 bytes |  8 seconds |
| LPAQ1 (copied from kjcao) | 5,054,116 bytes | 22 seconds |
| ZPAQ (-m5 + tweaks)       | 5,002,122 bytes | 59 seconds |

*/

use bytes::Bytes;
use chrono::{TimeZone, Utc};
use std::collections::HashMap;
use std::error::Error;
use std::io::{Cursor, Read, Write};

use crate::codec::EventCodec;
use crate::{EventKey, EventValue, Repo};

pub struct LifthrasiirCodec;

impl LifthrasiirCodec {
    pub fn new() -> Self {
        Self
    }
}

const EVENT_TYPES: &[&str] = &[
    "CommitCommentEvent",
    "CreateEvent",
    "DeleteEvent",
    "DiscussionEvent",
    "ForkEvent",
    "GollumEvent",
    "IssueCommentEvent",
    "IssuesEvent",
    "MemberEvent",
    "PublicEvent",
    "PullRequestEvent",
    "PullRequestReviewCommentEvent",
    "PullRequestReviewEvent",
    "PushEvent",
    "ReleaseEvent",
    "WatchEvent",
];

fn get_type_index(type_str: &str) -> Option<u8> {
    EVENT_TYPES
        .iter()
        .position(|&t| t == type_str)
        .map(|i| i as u8)
}

fn get_type_from_index(index: u8) -> Option<&'static str> {
    EVENT_TYPES.get(index as usize).copied()
}

/// Encode string with case compression (shift + latch):
/// - Lowercase by default
/// - ^ (caret) = next character is uppercase (shift)
/// - $ (dollar) = 2+ consecutive uppercase letters when NOT followed by lowercase letter
fn encode_case(input: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if !c.is_alphabetic() {
            result.push(c);
            i += 1;
            continue;
        }

        if c.is_uppercase() {
            // Count consecutive uppercase letters
            let mut run_len = 1;
            while i + run_len < chars.len() && chars[i + run_len].is_uppercase() {
                run_len += 1;
            }

            // Check if next char after uppercase run is a lowercase letter
            let next_is_lower = i + run_len < chars.len() && chars[i + run_len].is_lowercase();

            // Use latch for 2+ consecutive uppercase when NOT followed by lowercase
            if run_len >= 2 && !next_is_lower {
                result.push('$');
                for _ in 0..run_len {
                    result.extend(chars[i].to_lowercase());
                    i += 1;
                }
            } else {
                // Use shift for each uppercase char
                for _ in 0..run_len {
                    result.push('^');
                    result.extend(chars[i].to_lowercase());
                    i += 1;
                }
            }
        } else {
            result.push(c);
            i += 1;
        }
    }

    result
}

/// Decode string with case compression
fn decode_case(input: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '^' && i + 1 < chars.len() {
            result.extend(chars[i + 1].to_uppercase());
            i += 2;
        } else if chars[i] == '$' {
            i += 1;
            // Convert consecutive lowercase letters to uppercase (until non-letter)
            while i < chars.len() && chars[i].is_lowercase() {
                result.extend(chars[i].to_uppercase());
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

impl EventCodec for LifthrasiirCodec {
    fn name(&self) -> &str {
        "lifthrasiir"
    }

    fn encode(&self, events: &[(EventKey, EventValue)]) -> Result<Bytes, Box<dyn Error>> {
        // Sort events by key before encoding (to match expected output).
        // Note that this is not strictly necessary for correctness,
        // the following code does handle non-monotonic ids just fine.
        // Comment out and replace `sorted_events` with `events`
        // in the main `codecs` vector to test with unsorted input.
        let mut events = events.to_vec();
        events.sort_by(|a, b| a.0.cmp(&b.0));
        let events = &events[..];

        let num_events = events.len();

        // Collect data into separate arrays (struct of arrays)
        let mut id_deltas: Vec<i64> = Vec::with_capacity(num_events);
        let mut ts_deltas: Vec<i64> = Vec::with_capacity(num_events);
        let mut types: Vec<u8> = Vec::with_capacity(num_events);
        let mut combined_backrefs: Vec<u64> = Vec::with_capacity(num_events);
        let mut repo_ids: Vec<u32> = Vec::new();
        let mut repo_users: Vec<u8> = Vec::new(); // owner part
        let mut repo_names: Vec<u8> = Vec::new(); // repo name part

        // Calculate timestamp deltas
        let mut timestamps: Vec<i64> = Vec::with_capacity(num_events);
        for (_, value) in events {
            let ts: chrono::DateTime<Utc> = value.created_at.parse()?;
            timestamps.push(ts.timestamp());
        }

        let mut ts_delta_list: Vec<i64> = Vec::with_capacity(num_events);
        let mut prev_ts: i64 = 0;
        let mut prev_prev_ts: i64 = 0;
        for (i, ts) in timestamps.iter().enumerate() {
            if i == 0 {
                ts_delta_list.push(ts - prev_ts);
            } else if i == 1 {
                ts_delta_list.push(ts - prev_ts);
                prev_prev_ts = prev_ts;
            } else {
                let max_prev = prev_ts.max(prev_prev_ts);
                ts_delta_list.push(ts - max_prev);
                prev_prev_ts = prev_ts;
            }
            prev_ts = *ts;
        }

        // Process events
        let mut repos: Vec<(u64, String)> = Vec::new();
        let mut repo_to_idx: HashMap<(u64, &str), u32> = HashMap::new();
        let mut owners: Vec<String> = Vec::new();
        let mut owner_to_idx: HashMap<&str, u32> = HashMap::new();
        let mut prev_id: u64 = 0;

        for (i, (key, value)) in events.iter().enumerate() {
            // ID delta
            let id: u64 = key.id.parse()?;
            let id_delta = (id as i64) - (prev_id as i64);
            prev_id = id;
            id_deltas.push(id_delta);

            // Timestamp delta
            ts_deltas.push(ts_delta_list[i]);

            // Event type
            let type_idx = get_type_index(&key.event_type).ok_or("Unknown event type")?;
            types.push(type_idx);

            // Combined backref encoding:
            // 0 = new repo + new owner
            // 2n+1 = new repo + existing owner of index n
            // 2n+2 = existing repo of index n
            let repo_key = (value.repo.id, value.repo.name.as_str());
            if let Some(&idx) = repo_to_idx.get(&repo_key) {
                // Existing repo: encode as 2 * idx + 2
                combined_backrefs.push((2 * idx + 2) as u64);
            } else {
                let idx = repos.len() as u32;
                repos.push((value.repo.id, value.repo.name.clone()));
                repo_to_idx.insert((value.repo.id, value.repo.name.as_str()), idx);

                repo_ids.push(value.repo.id.try_into()?);

                // Split name into owner and repo name
                let parts: Vec<&str> = value.repo.name.splitn(2, '/').collect();
                assert_eq!(parts.len(), 2, "Invalid repo name format");
                let owner = parts[0];
                let name = parts[1];

                // Check if new or existing owner
                if let Some(&owner_idx) = owner_to_idx.get(owner) {
                    // New repo + existing owner: encode as 2 * owner_idx + 1
                    combined_backrefs.push((2 * owner_idx + 1) as u64);
                } else {
                    // New repo + new owner: encode as 0
                    let owner_idx = owners.len() as u32;
                    owners.push(owner.to_string());
                    owner_to_idx.insert(owner, owner_idx);
                    combined_backrefs.push(0);

                    // Store owner with case compression
                    let case_encoded = encode_case(owner);
                    repo_users.extend_from_slice(case_encoded.as_bytes());
                    repo_users.push(0);
                }

                // Check if name is a substring of owner for bidirectional encoding
                let mut processed_name = if name.contains(owner) && !owner.is_empty() {
                    name.replace(owner, "@")
                } else {
                    name.to_string()
                };

                // Special mapping: empty string instead of `@`, `@` instead of `@.github.io`
                if processed_name == "@" {
                    processed_name = String::new();
                } else if processed_name == "@.github.io" {
                    processed_name = "@".to_string();
                }

                // Apply case compression to name
                let case_encoded = encode_case(&processed_name);

                repo_names.extend_from_slice(case_encoded.as_bytes());
                repo_names.push(0);
            }
        }

        // Now encode each array
        let mut cursor = Cursor::new(Vec::new());

        // Header
        cursor.write_all(&(num_events as u32).to_le_bytes())?;
        cursor.write_all(&(repo_ids.len() as u32).to_le_bytes())?;

        // Section 1: id_delta
        // For non-positive deltas: signal with 0 byte, then encode -delta as varint
        // For positive deltas: encode directly as varint
        for id_delta in &id_deltas {
            if *id_delta <= 0 {
                cursor.write_all(&[0])?;
                write_varint(&mut cursor, (-*id_delta) as u64)?;
            } else {
                write_varint(&mut cursor, *id_delta as u64)?;
            }
        }

        // Section 2: Combined types + ts_delta_low
        // Pack types (4 bits) + low 4 bits of zigzag(ts_delta) into single byte
        // Then store remaining high bits of zigzag(ts_delta) as varint
        let mut ts_delta_highs: Vec<u64> = Vec::new();
        for (ty, ts_delta) in types.iter().zip(ts_deltas.iter()) {
            let zg = zigzag(*ts_delta);
            // Type in high 4 bits, low 4 bits of zg in low 4 bits
            let combined = (*ty << 4) | ((zg & 0x0F) as u8);
            cursor.write_all(&[combined])?;
            // High bits
            ts_delta_highs.push(zg >> 4);
        }

        // Section 3: ts_delta_high (only for non-zero high bits, 99.9% are zero)
        for high in &ts_delta_highs {
            write_varint(&mut cursor, *high)?;
        }

        // Section 4: combined_backrefs
        for backref in &combined_backrefs {
            write_varint(&mut cursor, *backref)?;
        }

        // Section 5: repo_ids (transposed by bytes)
        // Group all byte 0s, then all byte 1s, etc. for better compression
        for byte_idx in 0..4 {
            for repo_id in &repo_ids {
                let bytes = repo_id.to_le_bytes();
                cursor.write_all(&[bytes[byte_idx]])?;
            }
        }

        // Section 6: repo_users (null-terminated)
        cursor.write_all(&repo_users)?;

        // Section 7: repo_names (null-terminated)
        cursor.write_all(&repo_names)?;

        // Compress with zpaq
        let compressed = zpaq::compress_m5(cursor.into_inner().as_slice())?;
        Ok(Bytes::from(compressed))
    }

    fn decode(&self, bytes: &[u8]) -> Result<Vec<(EventKey, EventValue)>, Box<dyn Error>> {
        // Decompress with zpaq
        let decompressed = zpaq::decompress_m5(bytes)?;
        let mut cursor = Cursor::new(&decompressed[..]);

        // Read header
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf)?;
        let num_events = u32::from_le_bytes(buf) as usize;

        cursor.read_exact(&mut buf)?;
        let num_new_repos = u32::from_le_bytes(buf) as usize;

        // Read Section 1: id_delta
        let mut id_deltas: Vec<i64> = Vec::with_capacity(num_events);
        for _ in 0..num_events {
            let first = read_varint(&mut cursor)? as i64;
            if first == 0 {
                // Non-positive delta: read -delta as varint and negate
                let neg_delta = read_varint(&mut cursor)? as i64;
                id_deltas.push(-neg_delta);
            } else {
                // Positive delta
                id_deltas.push(first);
            }
        }

        // Read Section 2+3: Combined types + ts_delta_low
        let mut types: Vec<u8> = Vec::with_capacity(num_events);
        let mut ts_deltas: Vec<i64> = Vec::with_capacity(num_events);

        // Read combined bytes and extract types + low 4 bits of zigzag
        let mut combined_bytes: Vec<u8> = vec![0; num_events];
        cursor.read_exact(&mut combined_bytes)?;
        for combined in combined_bytes {
            let ty = combined >> 4;
            let low = (combined & 0x0F) as u64;
            types.push(ty);

            // Read high bits
            let high = read_varint(&mut cursor)?;
            // Reconstruct zigzag value
            let zg = (high << 4) | low;
            ts_deltas.push(unzigzag(zg));
        }

        // Read Section 4: combined_backrefs
        let mut combined_backrefs: Vec<u64> = Vec::with_capacity(num_events);
        for _ in 0..num_events {
            combined_backrefs.push(read_varint(&mut cursor)?);
        }

        // Read Section 5: repo_ids (transposed by bytes)
        let mut repo_ids: Vec<u32> = Vec::with_capacity(num_new_repos);
        let mut bytes_matrix: Vec<[u8; 4]> = vec![[0u8; 4]; num_new_repos];

        for byte_idx in 0..4 {
            for i in 0..num_new_repos {
                let mut byte = [0u8; 1];
                cursor.read_exact(&mut byte)?;
                bytes_matrix[i][byte_idx] = byte[0];
            }
        }

        for i in 0..num_new_repos {
            repo_ids.push(u32::from_le_bytes(bytes_matrix[i]));
        }

        // Count unique owners (combined_backref == 0 means new owner)
        let num_new_owners = combined_backrefs.iter().filter(|&&r| r == 0).count();

        // Read Section 6: repo_users (null-terminated, with case compression)
        let mut repo_users: Vec<String> = Vec::with_capacity(num_new_owners);
        for _ in 0..num_new_owners {
            let mut user_bytes = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                cursor.read_exact(&mut byte)?;
                if byte[0] == 0 {
                    break;
                }
                user_bytes.push(byte[0]);
            }
            let user_str = String::from_utf8(user_bytes)?;
            repo_users.push(decode_case(&user_str));
        }

        // Read Section 7: repo_names (null-terminated, with case compression)
        // Only for new repos (repo_backref == 0)
        let mut repo_names: Vec<String> = Vec::with_capacity(num_new_repos);
        for _ in 0..num_new_repos {
            let mut name_bytes = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                cursor.read_exact(&mut byte)?;
                if byte[0] == 0 {
                    break;
                }
                name_bytes.push(byte[0]);
            }
            let name_str = String::from_utf8(name_bytes)?;
            repo_names.push(decode_case(&name_str));
        }

        // Reconstruct events
        let mut repos: Vec<(u64, String)> = Vec::new();
        let mut owners: Vec<String> = Vec::new();
        let mut events = Vec::with_capacity(num_events);
        let mut prev_id: u64 = 0;
        let mut prev_ts: i64 = 0;
        let mut prev_prev_ts: i64 = 0;
        let mut repo_id_idx = 0;
        let mut owner_idx = 0;
        let mut name_idx = 0;

        for i in 0..num_events {
            // Reconstruct id
            let id = (prev_id as i64 + id_deltas[i]) as u64;
            prev_id = id;

            // Reconstruct timestamp
            let ts_delta = ts_deltas[i];
            let ts = if i == 0 {
                ts_delta
            } else if i == 1 {
                let ts_val = prev_ts + ts_delta;
                prev_prev_ts = prev_ts;
                ts_val
            } else {
                let max_prev = prev_ts.max(prev_prev_ts);
                let ts_val = ts_delta + max_prev;
                prev_prev_ts = prev_ts;
                ts_val
            };
            prev_ts = ts;

            let created_at = Utc
                .timestamp_opt(ts, 0)
                .single()
                .ok_or("Invalid timestamp")?
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();

            // Get type
            let type_idx = types[i];
            let event_type = get_type_from_index(type_idx).ok_or("Invalid type index")?;

            // Decode combined_backref
            let combined = combined_backrefs[i];

            let (repo_id, repo_name) = if combined == 0 {
                // New repo + new owner
                let id = repo_ids[repo_id_idx];
                repo_id_idx += 1;

                // Get owner
                if owner_idx >= repo_users.len() {
                    return Err(format!(
                        "owner_idx {} >= repo_users.len() {} at event {}",
                        owner_idx,
                        repo_users.len(),
                        i
                    )
                    .into());
                }
                let owner = repo_users[owner_idx].clone();
                owner_idx += 1;
                owners.push(owner.clone());

                // Get name
                let name = repo_names[name_idx].clone();
                name_idx += 1;

                // Undo special mapping
                let name = if name.is_empty() {
                    "@".to_string()
                } else if name == "@" {
                    "@.github.io".to_string()
                } else {
                    name
                };

                // If name has '@', replace it with owner
                let full_name = format!("{}/{}", owner, name.replace('@', &owner));

                repos.push((id as u64, full_name));
                repos.last().unwrap().clone()
            } else if combined.is_multiple_of(2) {
                // Existing repo
                let idx = (combined / 2 - 1) as usize;
                if idx >= repos.len() {
                    return Err(format!(
                        "Invalid combined_backref {} at event {}, repos.len()={}",
                        combined,
                        i,
                        repos.len()
                    )
                    .into());
                }
                repos[idx].clone()
            } else {
                // New repo + existing owner
                let id = repo_ids[repo_id_idx];
                repo_id_idx += 1;

                // Get existing owner
                let owner_idx_decoded = (combined / 2) as usize;
                if owner_idx_decoded >= owners.len() {
                    return Err(format!(
                        "Invalid owner index {} from combined_backref {} at event {}, owners.len()={}",
                        owner_idx_decoded, combined, i, owners.len()
                    )
                    .into());
                }
                let owner = owners[owner_idx_decoded].clone();

                // Get name
                let name = repo_names[name_idx].clone();
                name_idx += 1;

                // Undo special mapping
                let name = if name.is_empty() {
                    "@".to_string()
                } else if name == "@" {
                    "@.github.io".to_string()
                } else {
                    name
                };

                // If name has '@', replace it with owner
                let full_name = format!("{}/{}", owner, name.replace('@', &owner));

                repos.push((id as u64, full_name));
                repos.last().unwrap().clone()
            };

            let repo_url = format!("https://api.github.com/repos/{}", repo_name);

            let key = EventKey {
                id: id.to_string(),
                event_type: event_type.to_string(),
            };

            let value = EventValue {
                repo: Repo {
                    id: repo_id,
                    name: repo_name,
                    url: repo_url,
                },
                created_at,
            };

            events.push((key, value));
        }

        Ok(events)
    }
}

/// Zigzag encode: maps negative numbers to odd, positive to even
/// 0 → 0, -1 → 1, 1 → 2, -2 → 3, 2 → 4, ...
fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

/// Zigzag decode
fn unzigzag(n: u64) -> i64 {
    let n = n as i64;
    (n >> 1) ^ -(n & 1)
}

/// Write varint with leading-ones length encoding:
/// 0xxxxxxx (1 byte), 10xxxxxx yyyyyyyy (2 bytes), 110xxxxx yyyyyyyy zzzzzzzz (3 bytes), etc.
/// For 9 bytes (values >= 2^56): 11111111 yyyyyyyy zzzzzzzz ...
fn write_varint<W: Write>(writer: &mut W, value: u64) -> Result<(), std::io::Error> {
    // Determine number of bytes needed
    let num_bytes = if value < (1 << 7) {
        1
    } else if value < (1 << 14) {
        2
    } else if value < (1 << 21) {
        3
    } else if value < (1 << 28) {
        4
    } else if value < (1 << 35) {
        5
    } else if value < (1 << 42) {
        6
    } else if value < (1 << 49) {
        7
    } else if value < (1 << 56) {
        8
    } else {
        9
    };

    // Write bytes
    if num_bytes == 9 {
        // Special case: first byte is all 1s (0xFF)
        writer.write_all(&[0xFF])?;
        // Write remaining 8 bytes of data
        for i in 0..8 {
            let shift = (7 - i) * 8;
            let byte = (value >> shift) as u8;
            writer.write_all(&[byte])?;
        }
    } else {
        for i in 0..num_bytes {
            let shift = (num_bytes - 1 - i) * 8;
            let mut byte = (value >> shift) as u8;

            if i == 0 {
                // First byte: clear leading bits, then set prefix (leading 1s followed by 0)
                byte &= (1u8 << (8 - num_bytes)) - 1; // Clear leading bits
                let prefix: u8 = [
                    0b00000000, 0b10000000, 0b11000000, 0b11100000, 0b11110000, 0b11111000,
                    0b11111100, 0b11111110,
                ][num_bytes - 1];
                byte |= prefix;
            }

            writer.write_all(&[byte])?;
        }
    }

    Ok(())
}

/// Read varint with leading-ones length encoding
fn read_varint<R: Read>(reader: &mut R) -> Result<u64, std::io::Error> {
    // Read first byte to determine length
    let mut first_byte = [0u8; 1];
    reader.read_exact(&mut first_byte)?;
    let first_byte = first_byte[0];

    // Count leading 1s to get num_bytes
    let num_bytes = if first_byte == 0xFF {
        9
    } else {
        first_byte.leading_ones() as usize + 1
    };

    let mut result = (first_byte & (0xFF >> num_bytes)) as u64;

    // Read remaining bytes
    for _ in 1..num_bytes {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        result = (result << 8) | (byte[0] as u64);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Repo;

    #[test]
    fn test_varint() {
        let values = vec![0, 1, 127, 128, 16383, 16384, 1000000];

        for value in values {
            let mut buf = Vec::new();
            write_varint(&mut buf, value).unwrap();
            let mut cursor = Cursor::new(&buf);
            assert_eq!(read_varint(&mut cursor).unwrap(), value);
        }
    }

    #[test]
    fn test_codec_basic() {
        let events = vec![
            (
                EventKey {
                    id: "100".to_string(),
                    event_type: "PushEvent".to_string(),
                },
                EventValue {
                    repo: Repo {
                        id: 123,
                        name: "test/repo".to_string(),
                        url: "https://api.github.com/repos/test/repo".to_string(),
                    },
                    created_at: "2025-01-01T00:00:00Z".to_string(),
                },
            ),
            (
                EventKey {
                    id: "110".to_string(),
                    event_type: "WatchEvent".to_string(),
                },
                EventValue {
                    repo: Repo {
                        id: 456,
                        name: "other/repo".to_string(),
                        url: "https://api.github.com/repos/other/repo".to_string(),
                    },
                    created_at: "2025-01-01T01:00:00Z".to_string(),
                },
            ),
        ];

        let codec = LifthrasiirCodec::new();
        let encoded = codec.encode(&events).expect("Encode failed");
        println!("Encoded {} bytes", encoded.len());

        let decoded = codec.decode(&encoded).expect("Decode failed");

        assert_eq!(events.len(), decoded.len());
        for (i, (expected, actual)) in events.iter().zip(decoded.iter()).enumerate() {
            assert_eq!(expected, actual, "Mismatch at index {}", i);
        }
    }

    #[test]
    fn test_case_encoding_specific() {
        // Test specific case from actual data
        let name = "NotePad";
        let processed = name.replace("mshdabiola", "@"); // owner shouldn't be in this case
        let encoded = encode_case(&processed);
        let decoded = decode_case(&encoded);
        let reconstructed = decoded.replace('@', "mshdabiola");

        assert_eq!(
            name, reconstructed,
            "Roundtrip failed: {} -> {} (processed={}, encoded={}, decoded={})",
            name, reconstructed, processed, encoded, decoded
        );
    }

    #[test]
    fn test_owner_replacement_with_case() {
        // Test the full flow: owner/name -> @ replacement -> case encoding -> decode -> @ replacement
        let owner = "mshdabiola";
        let name = "NotePad";

        let processed = name.replace(owner, "@");
        let encoded = encode_case(&processed);
        let decoded = decode_case(&encoded);
        let reconstructed = decoded.replace('@', owner);

        assert_eq!(
            name, reconstructed,
            "Failed: {} != {} (processed={}, encoded={}, decoded={})",
            name, reconstructed, processed, encoded, decoded
        );
    }

    #[test]
    fn test_case_encoding_debug() {
        // Test with specific problematic cases
        let test_cases = vec![
            ("facebook", "react"),
            ("facebook", "React"),
            ("OpenAI", "gpt-3"),
            ("OpenAI", "GPT-3"),
            ("tensorflow", "tensorflow"),
            ("microsoft", "TypeScript"),
        ];

        for (owner, name) in test_cases {
            let processed = name.replace(owner, "@");
            let encoded = encode_case(&processed);
            let decoded = decode_case(&encoded);
            let reconstructed = decoded.replace('@', owner);

            println!(
                "Owner: {}, Name: {}, Processed: {}, Encoded: {}, Decoded: {}, Reconstructed: {}",
                owner, name, processed, encoded, decoded, reconstructed
            );

            assert_eq!(
                name, reconstructed,
                "Failed for {}/{}: {} != {}",
                owner, name, name, reconstructed
            );
        }
    }

    #[test]
    fn test_case_encoding() {
        // Test case encoding/decoding
        let test_cases = vec![
            "react",
            "React",
            "Redux",
            "TensorFlow",
            "OpenAI",
            "GitHub",
            "FastAPI",
            "GraphQL",
            "TypeScript",
            "JavaScript",
            "pytest",
            "rust-lang",
            "test-repo-123",
            "API",
            "CNN",
        ];

        for input in test_cases {
            let encoded = encode_case(input);
            let decoded = decode_case(&encoded);
            assert_eq!(
                input, decoded,
                "Failed for '{}': encoded as '{}', decoded as '{}'",
                input, encoded, decoded
            );
        }
    }
}

/// Entropy stage based on ZPAQ compressor with a small tweak to the built-in ZPAQL script.
/// Should be comparable in performance to `zpaq a ... -m5`.
/// This module only uses the public domain part of libzpaq, written by Matt Mahoney.
pub mod zpaq {
    use std::{
        io::{Read, Write},
        sync::OnceLock,
    };

    //////////////////// Utility Functions ////////////////////

    #[inline]
    fn squash(x: i32) -> i32 {
        static SQUASH_TABLE: [u16; 666] = [
            16384, 16256, 16128, 16000, 15872, 15744, 15616, 15488, 15361, 15233, 15106, 14979,
            14852, 14725, 14599, 14472, 14346, 14220, 14095, 13969, 13844, 13719, 13595, 13471,
            13347, 13224, 13101, 12978, 12856, 12734, 12612, 12491, 12371, 12251, 12131, 12012,
            11893, 11775, 11658, 11540, 11424, 11308, 11192, 11078, 10963, 10850, 10737, 10624,
            10512, 10401, 10290, 10180, 10071, 9962, 9854, 9747, 9640, 9534, 9429, 9324, 9221,
            9117, 9015, 8913, 8812, 8712, 8612, 8513, 8415, 8318, 8221, 8126, 8030, 7936, 7842,
            7750, 7658, 7566, 7476, 7386, 7297, 7209, 7121, 7035, 6949, 6863, 6779, 6695, 6613,
            6530, 6449, 6369, 6289, 6210, 6131, 6054, 5977, 5901, 5826, 5752, 5678, 5605, 5533,
            5461, 5390, 5320, 5251, 5183, 5115, 5048, 4981, 4916, 4851, 4786, 4723, 4660, 4598,
            4537, 4476, 4416, 4356, 4298, 4240, 4182, 4126, 4070, 4014, 3960, 3906, 3852, 3799,
            3747, 3696, 3645, 3594, 3545, 3496, 3447, 3399, 3352, 3305, 3259, 3213, 3168, 3124,
            3080, 3037, 2994, 2952, 2910, 2869, 2828, 2788, 2748, 2709, 2671, 2633, 2595, 2558,
            2521, 2485, 2450, 2414, 2380, 2345, 2312, 2278, 2245, 2213, 2181, 2149, 2118, 2087,
            2057, 2027, 1998, 1968, 1940, 1911, 1883, 1856, 1829, 1802, 1775, 1749, 1724, 1698,
            1673, 1649, 1624, 1600, 1577, 1554, 1531, 1508, 1486, 1464, 1442, 1421, 1399, 1379,
            1358, 1338, 1318, 1298, 1279, 1260, 1241, 1223, 1204, 1186, 1169, 1151, 1134, 1117,
            1100, 1084, 1067, 1051, 1036, 1020, 1005, 990, 975, 960, 946, 931, 917, 903, 890, 876,
            863, 850, 837, 825, 812, 800, 788, 776, 764, 752, 741, 730, 719, 708, 697, 686, 676,
            666, 656, 646, 636, 626, 617, 607, 598, 589, 580, 571, 562, 554, 545, 537, 529, 521,
            513, 505, 497, 490, 482, 475, 467, 460, 453, 446, 440, 433, 426, 420, 413, 407, 401,
            394, 388, 382, 377, 371, 365, 360, 354, 349, 343, 338, 333, 328, 323, 318, 313, 308,
            303, 299, 294, 289, 285, 281, 276, 272, 268, 264, 260, 256, 252, 248, 244, 240, 237,
            233, 229, 226, 222, 219, 215, 212, 209, 206, 202, 199, 196, 193, 190, 187, 184, 182,
            179, 176, 173, 171, 168, 165, 163, 160, 158, 155, 153, 151, 148, 146, 144, 141, 139,
            137, 135, 133, 131, 129, 127, 125, 123, 121, 119, 117, 115, 114, 112, 110, 108, 107,
            105, 103, 102, 100, 99, 97, 96, 94, 93, 91, 90, 88, 87, 86, 84, 83, 82, 81, 79, 78, 77,
            76, 74, 73, 72, 71, 70, 69, 68, 67, 66, 65, 64, 63, 62, 61, 60, 59, 58, 57, 56, 55, 54,
            54, 53, 52, 51, 50, 49, 49, 48, 47, 46, 46, 45, 44, 44, 43, 42, 42, 41, 40, 40, 39, 38,
            38, 37, 37, 36, 36, 35, 34, 34, 33, 33, 32, 32, 31, 31, 30, 30, 29, 29, 28, 28, 28, 27,
            27, 26, 26, 25, 25, 25, 24, 24, 23, 23, 23, 22, 22, 22, 21, 21, 21, 20, 20, 20, 19, 19,
            19, 18, 18, 18, 18, 17, 17, 17, 17, 16, 16, 16, 15, 15, 15, 15, 15, 14, 14, 14, 14, 13,
            13, 13, 13, 13, 12, 12, 12, 12, 12, 11, 11, 11, 11, 11, 10, 10, 10, 10, 10, 10, 10, 9,
            9, 9, 9, 9, 9, 8, 8, 8, 8, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6, 6,
            6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 3,
            3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
            1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
            1,
        ];
        match x {
            ..-665 => 0,
            -665..=0 => SQUASH_TABLE[(-x) as usize] as i32,
            1..666 => 32767 - SQUASH_TABLE[x as usize] as i32,
            666.. => 32767,
        }
    }

    struct Stretch(&'static [i32; 32768]);

    impl Stretch {
        fn get() -> Self {
            static STRETCH_COUNT: [u8; 711] = [
                64, 128, 128, 128, 128, 128, 127, 128, 127, 128, 127, 127, 127, 127, 126, 126, 126,
                126, 126, 125, 125, 124, 125, 124, 123, 123, 123, 123, 122, 122, 121, 121, 120,
                120, 119, 119, 118, 118, 118, 116, 117, 115, 116, 114, 114, 113, 113, 112, 112,
                111, 110, 110, 109, 108, 108, 107, 106, 106, 105, 104, 104, 102, 103, 101, 101,
                100, 99, 98, 98, 97, 96, 96, 94, 94, 94, 92, 92, 91, 90, 89, 89, 88, 87, 86, 86,
                84, 84, 84, 82, 82, 81, 80, 79, 79, 78, 77, 76, 76, 75, 74, 73, 73, 72, 71, 70, 70,
                69, 68, 67, 67, 66, 65, 65, 64, 63, 62, 62, 61, 61, 59, 59, 59, 57, 58, 56, 56, 55,
                54, 54, 53, 52, 52, 51, 51, 50, 49, 49, 48, 48, 47, 47, 45, 46, 44, 45, 43, 43, 43,
                42, 41, 41, 40, 40, 40, 39, 38, 38, 37, 37, 36, 36, 36, 35, 34, 34, 34, 33, 32, 33,
                32, 31, 31, 30, 31, 29, 30, 28, 29, 28, 28, 27, 27, 27, 26, 26, 25, 26, 24, 25, 24,
                24, 23, 23, 23, 23, 22, 22, 21, 22, 21, 20, 21, 20, 19, 20, 19, 19, 19, 18, 18, 18,
                18, 17, 17, 17, 17, 16, 16, 16, 16, 15, 15, 15, 15, 15, 14, 14, 14, 14, 13, 14, 13,
                13, 13, 12, 13, 12, 12, 12, 11, 12, 11, 11, 11, 11, 11, 10, 11, 10, 10, 10, 10, 9,
                10, 9, 9, 9, 9, 9, 8, 9, 8, 9, 8, 8, 8, 7, 8, 8, 7, 7, 8, 7, 7, 7, 6, 7, 7, 6, 6,
                7, 6, 6, 6, 6, 6, 6, 5, 6, 5, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 4, 5, 4, 5, 4, 4, 5, 4,
                4, 4, 4, 4, 4, 3, 4, 4, 3, 4, 4, 3, 3, 4, 3, 3, 3, 4, 3, 3, 3, 3, 3, 3, 2, 3, 3, 3,
                2, 3, 2, 3, 3, 2, 2, 3, 2, 2, 3, 2, 2, 2, 2, 3, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 1,
                2, 2, 2, 1, 2, 1, 2, 2, 1, 2, 1, 2, 1, 1, 2, 1, 1, 2, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1,
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1, 0, 1, 1, 1, 0,
                1, 1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 0, 1, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1,
                0, 1, 0, 1, 0, 1, 0, 0, 1, 0, 1, 0, 0, 1, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 1,
                0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0,
                1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
                0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            ];

            static CACHE: OnceLock<Box<[i32; 32768]>> = OnceLock::new();
            let cached = CACHE.get_or_init(|| {
                let mut stretch = Box::new([0i32; 32768]);
                let mut k = 16384;
                for (i, &count) in STRETCH_COUNT.iter().enumerate() {
                    let i = i as i32;
                    for _ in 0..count {
                        stretch[k] = i;
                        stretch[32767 - k] = -i;
                        k += 1;
                    }
                }
                assert_eq!(k, 32768);
                stretch
            });
            Self(cached)
        }

        #[inline]
        fn call(&self, x: i32) -> i32 {
            self.0[(x & 32767) as usize]
        }
    }

    #[test]
    fn test_logistic() {
        let mut hash = 0u32;
        for i in (-2048..2048).rev() {
            hash = hash.wrapping_mul(3).wrapping_add(squash(i) as u32);
        }
        assert_eq!(hash, 2278286169);

        let stretch = Stretch::get();
        let mut hash = 0u32;
        for i in (0..32768).rev() {
            hash = hash.wrapping_mul(3).wrapping_add(stretch.call(i) as u32);
        }
        assert_eq!(hash, 3887533746);
    }

    /// Clamp to 12-bit signed integer (-2048..2047)
    #[inline]
    fn clamp2k(x: i32) -> i32 {
        x.clamp(-2048, 2047)
    }

    /// Clamp to 19-bit signed integer
    #[inline]
    fn clamp512k(x: i32) -> i32 {
        x.clamp(-(1 << 19), (1 << 19) - 1)
    }

    /// Find cxt row in hash table ht (for ICM and ISSE)
    /// Returns index of row (matches libzpaq::find)
    fn find(ht: &mut [u8], shift: u32, cxt: u32) -> usize {
        let chk = ((cxt >> shift) & 255) as u8;
        let ht_size = ht.len();
        let h0 = ((cxt.wrapping_mul(16)) & (ht_size as u32 - 16)) as usize;
        if ht[h0] == chk {
            return h0;
        }
        let h1 = h0 ^ 16;
        if ht[h1] == chk {
            return h1;
        }
        let h2 = h0 ^ 32;
        if ht[h2] == chk {
            return h2;
        }
        // Replace row with lowest priority
        if ht[h0 + 1] <= ht[h1 + 1] && ht[h0 + 1] <= ht[h2 + 1] {
            ht[h0..h0 + 16].fill(0);
            ht[h0] = chk;
            h0
        } else if ht[h1 + 1] < ht[h2 + 1] {
            ht[h1..h1 + 16].fill(0);
            ht[h1] = chk;
            h1
        } else {
            ht[h2..h2 + 16].fill(0);
            ht[h2] = chk;
            h2
        }
    }

    #[inline]
    fn dt(count: u32) -> u32 {
        ((1 << 17) / (2 * count + 3)) * 2
    }

    fn train(pn: &mut u32, limit: u32, y: u8) {
        let count = *pn & 0x3ff;
        let pn_val = (*pn >> 17) as i32;
        let error = y as i32 * 32767 - pn_val;
        *pn = (*pn).wrapping_add(
            ((error * dt(count) as i32) & !1023) as u32 + if count < limit { 1 } else { 0 },
        );
    }

    //////////////////// State Table ////////////////////

    /// State transition table for bit history
    /// Each state has 4 entries: next state if 0, next state if 1, n0, n1
    static STATE_TABLE: [u8; 1024] = [
        1, 2, 0, 0, 3, 5, 1, 0, 4, 6, 0, 1, 7, 9, 2, 0, //
        8, 11, 1, 1, 8, 11, 1, 1, 10, 12, 0, 2, 13, 15, 3, 0, //
        14, 17, 2, 1, 14, 17, 2, 1, 16, 19, 1, 2, 16, 19, 1, 2, //
        18, 20, 0, 3, 21, 23, 4, 0, 22, 25, 3, 1, 22, 25, 3, 1, //
        24, 27, 2, 2, 24, 27, 2, 2, 26, 29, 1, 3, 26, 29, 1, 3, //
        28, 30, 0, 4, 31, 33, 5, 0, 32, 35, 4, 1, 32, 35, 4, 1, //
        34, 37, 3, 2, 34, 37, 3, 2, 36, 39, 2, 3, 36, 39, 2, 3, //
        38, 41, 1, 4, 38, 41, 1, 4, 40, 42, 0, 5, 43, 33, 6, 0, //
        44, 47, 5, 1, 44, 47, 5, 1, 46, 49, 4, 2, 46, 49, 4, 2, //
        48, 51, 3, 3, 48, 51, 3, 3, 50, 53, 2, 4, 50, 53, 2, 4, //
        52, 55, 1, 5, 52, 55, 1, 5, 40, 56, 0, 6, 57, 45, 7, 0, //
        58, 47, 6, 1, 58, 47, 6, 1, 60, 63, 5, 2, 60, 63, 5, 2, //
        62, 65, 4, 3, 62, 65, 4, 3, 64, 67, 3, 4, 64, 67, 3, 4, //
        66, 69, 2, 5, 66, 69, 2, 5, 52, 71, 1, 6, 52, 71, 1, 6, //
        54, 72, 0, 7, 73, 59, 8, 0, 74, 61, 7, 1, 74, 61, 7, 1, //
        76, 63, 6, 2, 76, 63, 6, 2, 78, 81, 5, 3, 78, 81, 5, 3, //
        80, 83, 4, 4, 80, 83, 4, 4, 82, 85, 3, 5, 82, 85, 3, 5, //
        66, 87, 2, 6, 66, 87, 2, 6, 68, 89, 1, 7, 68, 89, 1, 7, //
        70, 90, 0, 8, 91, 59, 9, 0, 92, 77, 8, 1, 92, 77, 8, 1, //
        94, 79, 7, 2, 94, 79, 7, 2, 96, 81, 6, 3, 96, 81, 6, 3, //
        98, 101, 5, 4, 98, 101, 5, 4, 100, 103, 4, 5, 100, 103, 4, 5, //
        82, 105, 3, 6, 82, 105, 3, 6, 84, 107, 2, 7, 84, 107, 2, 7, //
        86, 109, 1, 8, 86, 109, 1, 8, 70, 110, 0, 9, 111, 59, 10, 0, //
        112, 77, 9, 1, 112, 77, 9, 1, 114, 97, 8, 2, 114, 97, 8, 2, //
        116, 99, 7, 3, 116, 99, 7, 3, 62, 101, 6, 4, 62, 101, 6, 4, //
        80, 83, 5, 5, 80, 83, 5, 5, 100, 67, 4, 6, 100, 67, 4, 6, //
        102, 119, 3, 7, 102, 119, 3, 7, 104, 121, 2, 8, 104, 121, 2, 8, //
        86, 123, 1, 9, 86, 123, 1, 9, 70, 124, 0, 10, 125, 59, 11, 0, //
        126, 77, 10, 1, 126, 77, 10, 1, 128, 97, 9, 2, 128, 97, 9, 2, //
        60, 63, 8, 3, 60, 63, 8, 3, 66, 69, 3, 8, 66, 69, 3, 8, //
        104, 131, 2, 9, 104, 131, 2, 9, 86, 133, 1, 10, 86, 133, 1, 10, //
        70, 134, 0, 11, 135, 59, 12, 0, 136, 77, 11, 1, 136, 77, 11, 1, //
        138, 97, 10, 2, 138, 97, 10, 2, 104, 141, 2, 10, 104, 141, 2, 10, //
        86, 143, 1, 11, 86, 143, 1, 11, 70, 144, 0, 12, 145, 59, 13, 0, //
        146, 77, 12, 1, 146, 77, 12, 1, 148, 97, 11, 2, 148, 97, 11, 2, //
        104, 151, 2, 11, 104, 151, 2, 11, 86, 153, 1, 12, 86, 153, 1, 12, //
        70, 154, 0, 13, 155, 59, 14, 0, 156, 77, 13, 1, 156, 77, 13, 1, //
        158, 97, 12, 2, 158, 97, 12, 2, 104, 161, 2, 12, 104, 161, 2, 12, //
        86, 163, 1, 13, 86, 163, 1, 13, 70, 164, 0, 14, 165, 59, 15, 0, //
        166, 77, 14, 1, 166, 77, 14, 1, 168, 97, 13, 2, 168, 97, 13, 2, //
        104, 171, 2, 13, 104, 171, 2, 13, 86, 173, 1, 14, 86, 173, 1, 14, //
        70, 174, 0, 15, 175, 59, 16, 0, 176, 77, 15, 1, 176, 77, 15, 1, //
        178, 97, 14, 2, 178, 97, 14, 2, 104, 181, 2, 14, 104, 181, 2, 14, //
        86, 183, 1, 15, 86, 183, 1, 15, 70, 184, 0, 16, 185, 59, 17, 0, //
        186, 77, 16, 1, 186, 77, 16, 1, 74, 97, 15, 2, 74, 97, 15, 2, //
        104, 89, 2, 15, 104, 89, 2, 15, 86, 187, 1, 16, 86, 187, 1, 16, //
        70, 188, 0, 17, 189, 59, 18, 0, 190, 77, 17, 1, 86, 191, 1, 17, //
        70, 192, 0, 18, 193, 59, 19, 0, 194, 77, 18, 1, 86, 195, 1, 18, //
        70, 196, 0, 19, 193, 59, 20, 0, 197, 77, 19, 1, 86, 198, 1, 19, //
        70, 196, 0, 20, 199, 77, 20, 1, 86, 200, 1, 20, 201, 77, 21, 1, //
        86, 202, 1, 21, 203, 77, 22, 1, 86, 204, 1, 22, 205, 77, 23, 1, //
        86, 206, 1, 23, 207, 77, 24, 1, 86, 208, 1, 24, 209, 77, 25, 1, //
        86, 210, 1, 25, 211, 77, 26, 1, 86, 212, 1, 26, 213, 77, 27, 1, //
        86, 214, 1, 27, 215, 77, 28, 1, 86, 216, 1, 28, 217, 77, 29, 1, //
        86, 218, 1, 29, 219, 77, 30, 1, 86, 220, 1, 30, 221, 77, 31, 1, //
        86, 222, 1, 31, 223, 77, 32, 1, 86, 224, 1, 32, 225, 77, 33, 1, //
        86, 226, 1, 33, 227, 77, 34, 1, 86, 228, 1, 34, 229, 77, 35, 1, //
        86, 230, 1, 35, 231, 77, 36, 1, 86, 232, 1, 36, 233, 77, 37, 1, //
        86, 234, 1, 37, 235, 77, 38, 1, 86, 236, 1, 38, 237, 77, 39, 1, //
        86, 238, 1, 39, 239, 77, 40, 1, 86, 240, 1, 40, 241, 77, 41, 1, //
        86, 242, 1, 41, 243, 77, 42, 1, 86, 244, 1, 42, 245, 77, 43, 1, //
        86, 246, 1, 43, 247, 77, 44, 1, 86, 248, 1, 44, 249, 77, 45, 1, //
        86, 250, 1, 45, 251, 77, 46, 1, 86, 252, 1, 46, 253, 77, 47, 1, //
        86, 254, 1, 47, 253, 77, 48, 1, 86, 254, 1, 48, 0, 0, 0, 0,
    ];

    /// Get next state given current state and bit
    fn next(state: u8, y: u8) -> u8 {
        STATE_TABLE[state as usize * 4 + y as usize]
    }

    /// Get initial probability of 1 * 2^23 for a state
    fn cminit(state: u8) -> u32 {
        let base = state as usize * 4;
        let n0 = STATE_TABLE[base + 2] as u32;
        let n1 = STATE_TABLE[base + 3] as u32;
        if n0 + n1 == 0 {
            1 << 22 // 0.5 probability
        } else {
            ((n1 * 2 + 1) << 22) / (n0 + n1 + 1)
        }
    }

    //////////////////// Component Types ////////////////////

    /// A component in the context mixing model
    #[derive(Clone)]
    pub enum Component {
        Cm {
            limit: usize,
            cm: Vec<u32>,
        },
        Icm {
            sizebits: u32,
            c: usize,   // hash index from find()
            cxt: usize, // bit history (bh)
            ht: Vec<u8>,
            cm: Vec<u32>, // for ICM, this stores the stretched prediction
        },
        Match {
            cxt: usize,      // bit position within byte (0-7)
            buf: Vec<u8>,    // circular buffer
            index: Vec<u32>, // hash index
            a: usize,        // match length
            b: usize,        // match offset
            ptr: usize,      // current position in buffer
        },
        Mix {
            sizebits: u32,
            start_idx: usize,
            m: usize,
            rate: i32,
            weights: Vec<i32>,
        },
        Mix2 {
            sizebits: u32,
            p1_idx: usize,
            p2_idx: usize,
            rate: i32,
            weights: Vec<i32>, // Changed to i32 to match libzpaq's a16 (U16 in C++, but stores up to 65535)
        },
        Isse {
            prev_component: usize, // ID of previous component to use as input
            sizebits: u32,
            c: usize,   // hash index from find()
            cxt: usize, // bit history (bh)
            ht: Vec<u8>,
            wt: Vec<i32>, // ISSE weights (int, not i16!)
        },
        Sse {
            prev_component: usize, // ID of previous component to use as input
            limit: usize,
            cxt: usize,
            cm: Vec<u32>,
        },
    }

    impl Component {
        pub fn cm(sizebits: u32, limit: u32) -> Self {
            let size = 1 << sizebits;
            let mut cm = vec![0u32; size];
            // Initialize with 0x80000000
            for entry in cm.iter_mut() {
                *entry = 0x80000000;
            }
            Component::Cm {
                limit: (limit * 4) as usize, // libzpaq: cr.limit=cp[2]*4;
                cm,
            }
        }

        pub fn icm(sizebits: u32) -> Self {
            let size = 1 << sizebits;
            let ht_size = size * 64;
            let mut cm = vec![0u32; 256];
            // Initialize with cminit values
            for j in 0..256 {
                cm[j] = cminit(j as u8);
            }
            Component::Icm {
                sizebits,
                c: 0,
                cxt: 0,
                ht: vec![0u8; ht_size],
                cm,
            }
        }

        pub fn match_(sizebits1: u32, sizebits2: u32) -> Self {
            let index_size = 1 << sizebits1;
            let buf_size = 1 << sizebits2;
            let mut buf = vec![0u8; buf_size];
            buf[0] = 1; // cr.ht(0)=1;
            Component::Match {
                cxt: 0,
                buf,
                index: vec![0u32; index_size],
                a: 0,
                b: 0,
                ptr: 0,
            }
        }

        pub fn mix(sizebits: u32, idx: std::ops::RangeInclusive<usize>, rate: u32) -> Self {
            let start_idx = *idx.start();
            let m = idx.end() - idx.start() + 1;
            let size = 1 << sizebits;
            let mut weights = vec![0i32; size * m];
            let init_val = (65536 / m) as i32;
            for w in weights.iter_mut() {
                *w = init_val;
            }
            Component::Mix {
                sizebits,
                start_idx,
                m,
                rate: rate as i32,
                weights,
            }
        }

        pub fn mix2(sizebits: u32, p1_idx: usize, p2_idx: usize, rate: u32) -> Self {
            let size = 1 << sizebits;
            Component::Mix2 {
                sizebits,
                p1_idx,
                p2_idx,
                rate: rate as i32,
                weights: vec![32768i32; size],
            }
        }

        pub fn isse(sizebits: u32, prev_component: usize) -> Self {
            let ht_size = (1 << sizebits) * 64;
            let mut wt = vec![0i32; 512];
            let stretch = Stretch::get();
            for j in 0..256 {
                wt[j * 2] = 1 << 15;
                let p = cminit(j as u8);
                let stretched = stretch.call((p >> 8) as i32);
                let scaled = stretched * 1024;
                wt[j * 2 + 1] = clamp512k(scaled);
            }
            Component::Isse {
                sizebits,
                prev_component,
                c: 0,
                cxt: 0,
                ht: vec![0u8; ht_size],
                wt,
            }
        }

        pub fn sse(sizebits: u32, prev_component: usize, start: u32, limit: u32) -> Self {
            let cm_size = (1 << sizebits) * 32;
            let mut cm = vec![0u32; cm_size];
            for j in 0..cm_size {
                let squash_val = squash((j & 31) as i32 * 64 - 992);
                cm[j] = ((squash_val as u32) << 17) | start;
            }
            Component::Sse {
                prev_component,
                limit: (limit * 4) as usize,
                cxt: 0,
                cm,
            }
        }
    }

    //////////////////// Predictor ////////////////////

    /// Main predictor that combines all components
    pub struct Predictor {
        // Context from ZPAQL HCOMP
        pub m: Vec<u8>,
        pub h: Vec<u32>,

        // Components
        pub components: Vec<Component>,

        // Predictor state
        c8: u32,                   // last 0-7 bits of current byte (can be up to 256)
        hmap4: u32,                // c8 split into nibbles
        pub predictions: Vec<i32>, // predictions from each component

        stretch: Stretch,
    }

    impl Predictor {
        pub fn new(num_components: usize, h_size: usize, m_size: usize) -> Self {
            Predictor {
                m: vec![0u8; m_size],
                components: Vec::with_capacity(num_components),
                h: vec![0u32; h_size],
                predictions: vec![0i32; num_components],
                c8: 1,
                hmap4: 1,
                stretch: Stretch::get(),
            }
        }

        /// Add a component to the predictor
        /// Returns the ID of the newly added component
        pub fn add_component(&mut self, comp: Component) -> usize {
            let id = self.components.len();
            self.components.push(comp);
            id
        }

        /// Predict the next bit (returns probability 0..4095)
        pub fn predict(&mut self) -> i32 {
            // Predict for each component
            for i in 0..self.components.len() {
                // libzpaq stores stretched predictions (-2048..2047)
                // We store the same for consistency
                self.predictions[i] = match &mut self.components[i] {
                    Component::Cm { cm, .. } => {
                        let cxt = ((self.h[i] ^ self.hmap4) & (cm.len() - 1) as u32) as usize;
                        let p = (cm[cxt] >> 17) as i32;
                        self.stretch.call(p)
                    }
                    Component::Icm {
                        sizebits,
                        c,
                        cxt,
                        ht,
                        cm,
                        ..
                    } => {
                        if self.c8 == 1 || (self.c8 & 0xf0) == 16 {
                            *c = find(ht, *sizebits + 2, self.h[i] + self.c8 * 16);
                        }
                        let bh = ht[*c + ((self.hmap4 & 15) as usize)] as usize;
                        *cxt = bh;
                        let p = (cm[bh] >> 8) as i32;
                        self.stretch.call(p)
                    }
                    Component::Isse {
                        sizebits,
                        prev_component,
                        c,
                        cxt,
                        ht,
                        wt,
                        ..
                    } => {
                        if self.c8 == 1 || (self.c8 & 0xf0) == 16 {
                            *c = find(ht, *sizebits + 2, self.h[i] + self.c8 * 16);
                        }
                        let bh = ht[*c + ((self.hmap4 & 15) as usize)] as usize;
                        *cxt = bh;
                        let wt_idx = bh * 2;
                        let p0 = self.predictions[*prev_component];
                        clamp2k((wt[wt_idx] * p0 + wt[wt_idx + 1] * 64) >> 16)
                    }
                    Component::Match {
                        a,
                        b,
                        buf,
                        ptr,
                        cxt,
                        ..
                    } => {
                        if *a == 0 {
                            0
                        } else {
                            let buf_mask = buf.len() - 1;
                            let match_pos = ptr.wrapping_sub(*b) & buf_mask;
                            let bit = (buf[match_pos] >> (7 - *cxt)) & 1;
                            let dt2k = 2048 / (*a as u32);
                            let factor = if bit != 0 { -1 } else { 1 };
                            self.stretch.call(((dt2k as i32) * factor) & 32767)
                        }
                    }
                    Component::Mix {
                        sizebits,
                        start_idx,
                        m,
                        weights,
                        ..
                    } => {
                        let size = 1 << *sizebits;
                        let ctx = ((self.h[i].wrapping_add(self.c8 & 255)) & (size - 1) as u32)
                            as usize
                            * *m;
                        let mut p = 0i32;
                        for j in 0..*m {
                            p += ((weights[ctx + j]) >> 8) * self.predictions[*start_idx + j];
                        }
                        clamp2k(p >> 8)
                    }
                    Component::Mix2 {
                        sizebits,
                        p1_idx,
                        p2_idx,
                        weights,
                        ..
                    } => {
                        let size = 1 << *sizebits;
                        let ctx =
                            (self.h[i].wrapping_add(self.c8 & 255) & (size - 1) as u32) as usize;
                        let w = weights[ctx];
                        let p1 = self.predictions[*p1_idx];
                        let p2 = self.predictions[*p2_idx];
                        (w * p1 + (65536 - w) * p2) >> 16
                    }
                    Component::Sse {
                        prev_component,
                        cxt,
                        cm,
                        ..
                    } => {
                        let cm_mask = cm.len() - 1;
                        let mut cxt_val = self.h[i].wrapping_add(self.c8) as usize;
                        cxt_val *= 32;
                        let mut pq = self.predictions[*prev_component] + 992;
                        pq = pq.clamp(0, 1983);
                        let wt = pq & 63;
                        pq >>= 6;
                        cxt_val += pq as usize;
                        // Store adjusted cxt for use in update (cr.cxt += wt>>5)
                        *cxt = cxt_val + (wt >> 5) as usize;
                        let p0 = (cm[cxt_val & cm_mask] >> 10) as i32;
                        let p1 = (cm[(cxt_val + 1) & cm_mask] >> 10) as i32;
                        let result = (p0 * (64 - wt) + p1 * wt) >> 13;
                        self.stretch.call(result)
                    }
                };
            }

            // Return squash(p[n-1])
            if let Some(last) = self.predictions.last() {
                squash(*last)
            } else {
                2048 // 0.5 probability if no components
            }
        }

        /// Update predictor with actual bit value
        pub fn update(&mut self, y: u8) {
            // Update components first (using current c8/hmap4, matching libzpaq update0 order)
            for i in 0..self.components.len() {
                match &mut self.components[i] {
                    Component::Cm { cm, limit } => {
                        let cxt = ((self.h[i] ^ self.hmap4) & (cm.len() - 1) as u32) as usize;
                        train(&mut cm[cxt], *limit as u32, y);
                    }
                    Component::Icm { c, cxt, ht, cm, .. } => {
                        let idx = *c + ((self.hmap4 & 15) as usize);
                        let bh = ht[idx];
                        ht[idx] = next(bh, y);
                        let entry = &mut cm[*cxt];
                        // Add delta to full 32-bit entry (low 8 bits = fractional part)
                        let delta = ((y as i32 * 32767 - (*entry >> 8) as i32) >> 2) as u32;
                        *entry = entry.wrapping_add(delta);
                    }
                    Component::Isse {
                        prev_component,
                        c,
                        cxt,
                        ht,
                        wt,
                        ..
                    } => {
                        let idx = *c + ((self.hmap4 & 15) as usize);
                        let bh = ht[idx];
                        let err = (y as i32 * 32767) - squash(self.predictions[i]);
                        let wt_idx = *cxt * 2;
                        let p = self.predictions[*prev_component];
                        wt[wt_idx] = clamp512k(wt[wt_idx] + ((err * p + (1 << 12)) >> 13));
                        wt[wt_idx + 1] = clamp512k(wt[wt_idx + 1] + ((err + 16) >> 5));
                        ht[idx] = next(bh, y);
                    }
                    Component::Match {
                        a,
                        b,
                        buf,
                        index,
                        ptr,
                        cxt,
                    } => {
                        let buf_mask = buf.len() - 1;

                        // Check mismatch against the predicted bit (from predict)
                        if *a > 0 {
                            let match_pos = ptr.wrapping_sub(*b) & buf_mask;
                            let predicted = (buf[match_pos] >> (7 - *cxt)) & 1;
                            if predicted != y {
                                *a = 0;
                            }
                        }

                        // Accumulate bit into current byte
                        buf[*ptr] = buf[*ptr].wrapping_mul(2).wrapping_add(y);

                        *cxt += 1;
                        if *cxt == 8 {
                            *cxt = 0;
                            *ptr = (*ptr + 1) & buf_mask;

                            let hash_idx = (self.h[i] as usize) & (index.len() - 1);
                            if *a == 0 {
                                *b = ptr.wrapping_sub(index[hash_idx] as usize) & buf_mask;
                                if *b != 0 {
                                    while *a < 255 {
                                        let p1 = ptr.wrapping_sub(*a + 1) & buf_mask;
                                        let p2 = ptr.wrapping_sub(*a + 1 + *b) & buf_mask;
                                        if buf[p1] == buf[p2] {
                                            *a += 1;
                                        } else {
                                            break;
                                        }
                                    }
                                }
                            } else if *a < 255 {
                                *a += 1;
                            }
                            index[hash_idx] = *ptr as u32;
                        }
                    }
                    Component::Mix {
                        sizebits,
                        start_idx,
                        m,
                        rate,
                        weights,
                        ..
                    } => {
                        let size = 1 << *sizebits;
                        let ctx = ((self.h[i].wrapping_add(self.c8 & 255)) & (size - 1) as u32)
                            as usize
                            * *m;
                        let err = ((y as i32 * 32767 - squash(self.predictions[i])) * *rate) >> 4;
                        for j in 0..*m {
                            weights[ctx + j] = clamp512k(
                                weights[ctx + j]
                                    + ((err * self.predictions[*start_idx + j] + (1 << 12)) >> 13),
                            );
                        }
                    }
                    Component::Mix2 {
                        sizebits,
                        p1_idx,
                        p2_idx,
                        rate,
                        weights,
                        ..
                    } => {
                        let size = 1 << *sizebits;
                        let ctx =
                            (self.h[i].wrapping_add(self.c8 & 255) & (size - 1) as u32) as usize;
                        let err = ((y as i32 * 32767 - squash(self.predictions[i])) * *rate) >> 5;
                        let mut w = weights[ctx];
                        let p1 = self.predictions[*p1_idx];
                        let p2 = self.predictions[*p2_idx];
                        w += (err * (p1 - p2) + (1 << 12)) >> 13;
                        weights[ctx] = w.clamp(0, 65535);
                    }
                    Component::Sse { cxt, cm, limit, .. } => {
                        // train(cr, y): uses cr.cxt saved from predict (includes wt>>5 adjustment)
                        let cm_mask = cm.len() - 1;
                        let idx = *cxt; // already adjusted by wt>>5 in predict
                        let entry = &mut cm[idx & cm_mask];
                        let count = *entry & 0x3ff;
                        let error = (y as i32 * 32767) - (*entry >> 17) as i32;
                        let delta = (error * dt(count) as i32) & !1023i32;
                        let inc = if count < *limit as u32 { 1u32 } else { 0u32 };
                        *entry = entry.wrapping_add(delta as u32).wrapping_add(inc);
                    }
                }
            }

            // Update c8 and hmap4 AFTER components
            self.c8 = (self.c8 << 1) | (y as u32);
            if self.c8 >= 256 {
                // Full byte complete: HCOMP will be called externally, reset hmap4
                self.hmap4 = 1;
                self.c8 = 1;
            } else if self.c8 >= 16 && self.c8 < 32 {
                self.hmap4 = ((self.hmap4 & 0xf) << 5) | ((y as u32) << 4) | 1;
            } else {
                self.hmap4 = (self.hmap4 & 0x1f0) | (((self.hmap4 & 0xf) * 2 + y as u32) & 0xf);
            }
        }

        /// Reset bit position for new byte
        pub fn new_byte(&mut self) {
            self.c8 = 1;
            self.hmap4 = 1;
        }
    }

    //////////////////// Arithmetic Encoder ////////////////////

    pub struct Encoder<W: Write> {
        writer: W,
        low: u32,
        high: u32,
        predictor: Predictor,
    }

    impl<W: Write> Encoder<W> {
        pub fn new(writer: W, predictor: Predictor) -> Self {
            Encoder {
                writer,
                low: 1, // libzpaq: low=1 for modeled predictor
                high: 0xFFFFFFFF,
                predictor,
            }
        }

        /// Encode a single bit
        fn encode_bit(&mut self, y: u8) -> std::io::Result<()> {
            let p = (self.predictor.predict() * 2 + 1) as u32;
            let range = (self.high.wrapping_sub(self.low) as u64) * (p as u64);
            let mid = self.low.wrapping_add((range >> 16) as u32);

            if y != 0 {
                self.high = mid;
            } else {
                self.low = mid + 1;
            }

            // Normalize and output bytes
            while ((self.low ^ self.high) >> 24) == 0 {
                let byte = (self.high >> 24) as u8;
                self.writer.write_all(&[byte])?;
                self.low = self.low.wrapping_shl(8);
                if self.low == 0 {
                    self.low = 1;
                }
                self.high = self.high.wrapping_shl(8) | 0xFF;
            }

            self.predictor.update(y);
            Ok(())
        }

        /// Encode a single byte (8 bits, MSB first)
        pub fn encode_byte(&mut self, byte: u8) -> std::io::Result<()> {
            self.predictor.new_byte();
            for i in 0..8 {
                let bit = (byte >> (7 - i)) & 1;
                self.encode_bit(bit)?;
            }
            Ok(())
        }

        /// Flush any remaining data
        pub fn flush(mut self) -> std::io::Result<W> {
            // libzpaq: just output 4 bytes, no extra normalize
            self.writer.write_all(&self.high.to_be_bytes())?;
            self.writer.flush()?;
            Ok(self.writer)
        }
    }

    //////////////////// Arithmetic Decoder ////////////////////

    pub struct Decoder<R: Read> {
        reader: R,
        low: u32,
        high: u32,
        curr: u32,
        predictor: Predictor,
        buffer: [u8; 4],
        buf_pos: usize,
        buf_len: usize,
    }

    impl<R: Read> Decoder<R> {
        pub fn new(reader: R, predictor: Predictor) -> Self {
            Decoder {
                reader,
                low: 1, // libzpaq: low=1 for modeled predictor
                high: 0xFFFFFFFF,
                curr: 0,
                predictor,
                buffer: [0u8; 4],
                buf_pos: 0,
                buf_len: 0,
            }
        }

        /// Fill current value from input
        fn fill_curr(&mut self) -> std::io::Result<()> {
            for _ in 0..4 {
                self.curr = (self.curr << 8) | self.read_byte()? as u32;
            }
            Ok(())
        }

        /// Read a single byte from input
        fn read_byte(&mut self) -> std::io::Result<u8> {
            if self.buf_pos >= self.buf_len {
                self.buf_len = self.reader.read(&mut self.buffer)?;
                self.buf_pos = 0;
                if self.buf_len == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "unexpected end of file",
                    ));
                }
            }
            let byte = self.buffer[self.buf_pos];
            self.buf_pos += 1;
            Ok(byte)
        }

        /// Decode a single bit
        fn decode_bit(&mut self) -> std::io::Result<u8> {
            let p = (self.predictor.predict() * 2 + 1) as u32;
            let range = (self.high.wrapping_sub(self.low) as u64) * (p as u64);
            let mid = self.low.wrapping_add((range >> 16) as u32);

            let y = if self.curr <= mid {
                self.high = mid;
                1
            } else {
                self.low = mid + 1;
                0
            };

            // Normalize
            while ((self.low ^ self.high) >> 24) == 0 {
                self.low = self.low.wrapping_shl(8);
                if self.low == 0 {
                    self.low = 1;
                }
                self.high = self.high.wrapping_shl(8) | 0xFF;
                let byte = self.read_byte()?;
                self.curr = (self.curr << 8) | byte as u32;
            }

            self.predictor.update(y);
            Ok(y)
        }

        /// Decode a single byte (8 bits, MSB first)
        pub fn decode_byte(&mut self) -> std::io::Result<u8> {
            // Initialize curr if needed (like libzpaq decompress)
            if self.curr == 0 {
                for _i in 0..4 {
                    self.curr = (self.curr << 8) | self.read_byte()? as u32;
                }
            }
            self.predictor.new_byte();
            let mut byte = 0u8;
            for _ in 0..8 {
                byte = (byte << 1) | self.decode_bit()?;
            }
            Ok(byte)
        }
    }

    //////////////////// High-level Compression Functions ////////////////////

    /// Configure predictor with m5.zpaql settings
    /// This matches the component configuration from m5.zpaql (except for commented values)
    pub fn configure_m5(predictor: &mut Predictor, _buf_size: usize) {
        let c0 = predictor.add_component(Component::icm(18));
        let _c1 = predictor.add_component(Component::isse(18, c0));
        let _c2 = predictor.add_component(Component::cm(9, /*255*/ 32));
        let c3 = predictor.add_component(Component::icm(5));
        let c4 = predictor.add_component(Component::isse(11, c3));
        let c5 = predictor.add_component(Component::isse(17, c4));
        let c6 = predictor.add_component(Component::isse(18, c5));
        let c7 = predictor.add_component(Component::isse(18, c6));
        let c8 = predictor.add_component(Component::isse(18, c7));
        let c9 = predictor.add_component(Component::isse(18, c8));
        let _c10 = predictor.add_component(Component::isse(18, c9));
        let _c11 = predictor.add_component(Component::match_(22, 24));
        let c12 = predictor.add_component(Component::icm(13));
        let _c13 = predictor.add_component(Component::isse(18, c12));
        let c14 = predictor.add_component(Component::icm(13));
        let _c15 = predictor.add_component(Component::isse(18, c14));
        let c16 = predictor.add_component(Component::icm(14));
        let c17 = predictor.add_component(Component::isse(18, c16));
        let c18 = predictor.add_component(Component::mix(8, c0..=c17, 24));
        let c19 = predictor.add_component(Component::mix(16, c0..=c18, 24));
        let c20 = predictor.add_component(Component::mix2(8, c19, c18, 24));
        let c21 = predictor.add_component(Component::sse(19, c20, 32, /*255*/ 64));
        let _c22 = predictor.add_component(Component::mix2(0, c21, c20, 24));
    }

    /// HCOMP implementation for m5.zpaql — faithful translation of the HCOMP bytecode.
    ///
    /// ZPAQL registers: a=accumulator, b/c=byte addresses, d=H[] index.
    /// m[c] = current byte (c decrements), so m[c+k] = k-th previous byte.
    /// In Rust, buf_ptr increments, so m[(buf_ptr-k) & mask] = k-th previous byte.
    pub fn hcomp_m5(predictor: &mut Predictor, byte: u8, buf_ptr: usize) {
        // hash instruction: a = (a + m[b] + 512) * 773 / H[d] = (H[d] + a + 512) * 773
        let hash = |a: u32, x: u32| a.wrapping_add(x).wrapping_add(512).wrapping_mul(773);

        let buf_mask = predictor.m.len() - 1;
        let h_mask = predictor.h.len() - 1;

        // c-- *c=a a+= 255 d=a *d=c
        predictor.m[buf_ptr & buf_mask] = byte;
        let a = (byte as u32).wrapping_add(255);
        let d = (a as usize) & h_mask;
        let c_zpaql = buf_mask.wrapping_sub(buf_ptr & buf_mask);
        predictor.h[d] = c_zpaql as u32;

        // get k-th previous byte: k=0 = current, k=1 = previous, etc.
        // ZPAQL m[c+k] corresponds to m[(buf_ptr - k) & mask] in Rust
        let ch = byte as u32;
        let prev = |k: usize| predictor.m[buf_ptr.wrapping_sub(k) & buf_mask] as u32;

        // ── Component 0 (ICM 18): word model ───
        // a=*c a&=223 a-=65 a&=255 a<26 if
        //   d=0 a=*d a*=20 a+=*c a++ *d=a
        // else d=0 *d=0 endif
        let normalized = (ch & 223).wrapping_sub(65) & 255;
        if normalized < 26 {
            predictor.h[0] = predictor.h[0]
                .wrapping_mul(20)
                .wrapping_add(ch)
                .wrapping_add(1);
        } else {
            predictor.h[0] = 0;
        }

        // ── Component 1 (ISSE 18): d=0 b=c a=*d d++ / hash *d=a ───
        // a = H[0], hash uses m[b]=m[c]=current byte
        predictor.h[1] = hash(predictor.h[0], ch);

        // ── Components 2,3: d=2 *d=0 / d=3 *d=0 ───
        predictor.h[2] = 0;
        predictor.h[3] = 0;

        // ── Components 4-10: context chain ───
        // d=3 b=c a=*d d++   (a=H[3]=0, b=c, d→4)
        // hash b++ *d=a d++  × 6  then  hash b++ hash *d=a  (d=10)
        let mut a: u32 = 0; // a = H[3] = 0
        for d in 4usize..=9 {
            let offset = d - 4; // b advances from c: b=c, c+1, c+2, ...
            a = hash(a, prev(offset)); // hash uses m[b]
                                       // b++ happens after hash
            predictor.h[d] = a;
        }
        // Final pair for H[10]: hash b++ hash *d=a
        a = hash(a, prev(6)); // first hash (b=c+6)
                              // b++ → b=c+7
        a = hash(a, prev(7)); // second hash (b=c+7)
        predictor.h[10] = a;

        // ── Component 11 (MATCH 22 24) ───
        // d=11 a=*d a*=24 a+=*c a++ *d=a
        predictor.h[11] = predictor.h[11]
            .wrapping_mul(24)
            .wrapping_add(ch)
            .wrapping_add(1);

        // ── Component 12 (ICM 13) ───
        // d=12 *d=0 / a=c a&=1 hashd / b=c b++ a=*b hashd
        let mut h12: u32 = 0;
        h12 = hash(h12, (c_zpaql & 1) as u32); // a=c a&=1 hashd
        h12 = hash(h12, prev(1)); // b=c b++ a=*b hashd (m[c+1] = prev byte)
        predictor.h[12] = h12;

        // ── Component 13 (ISSE 18 12) ───
        // d=12 b=c a=*d d++ / hash *d=a
        predictor.h[13] = hash(predictor.h[12], ch);

        // ── Component 14 (ICM 13) ───
        // d=14 *d=0 / a=c a%=3 hashd / b=c b++ b++ a=*b hashd
        let mut h14: u32 = 0;
        h14 = hash(h14, (c_zpaql % 3) as u32); // a=c a%=3 hashd
        h14 = hash(h14, prev(2)); // m[c+2] = 2 bytes ago
        predictor.h[14] = h14;

        // ── Component 15 (ISSE 18 14) ───
        predictor.h[15] = hash(predictor.h[14], ch);

        // ── Component 16 (ICM 14) ───
        // d=16 *d=0 / a=c a&=3 hashd / b=c b++ b++ b++ a=*b hashd
        let mut h16: u32 = 0;
        h16 = hash(h16, (c_zpaql & 3) as u32); // a=c a&=3 hashd
        h16 = hash(h16, prev(3)); // m[c+3] = 3 bytes ago
        predictor.h[16] = h16;

        // ── Component 17 (ISSE 18 16) ───
        predictor.h[17] = hash(predictor.h[16], ch);

        // ── Components 18, 20, 22 not set by HCOMP → stay 0 ───

        // ── Component 19 (MIX 16) ───
        // d=19 *d=0 b=c a=0 / a<<=8 a+=*b / a<<=8 *d=a
        // = (m[c] << 8) << 8 ... let's trace:
        // a=0, a<<=8→0, a+=m[c]=ch, a<<=8→ch<<8, *d=a
        predictor.h[19] = ch << 8;

        // ── Component 21 (SSE 19) ───
        // d=21 *d=0 b=c a=0
        // a<<=8 a+=*b     → a = m[c] = ch
        // b++             → b = c+1
        // a<<=8 a+=*b     → a = ch<<8 + m[c+1]
        // a>>=5           → a = (ch<<8 + prev1) >> 5
        // a<<=8 *d=a      → H[21] = ((ch<<8 + prev1) >> 5) << 8
        let prev1 = prev(1);
        predictor.h[21] = ((ch << 8).wrapping_add(prev1) >> 5) << 8;
    }

    /// Compress data using ZPAQ m5 configuration
    pub fn compress_m5(data: &[u8]) -> std::io::Result<Vec<u8>> {
        // Create predictor with m5 config
        let mut predictor = Predictor::new(23, 512, 65536); // 23 components, hh=9, hm=16
        configure_m5(&mut predictor, data.len());

        // Create output buffer with space for size header
        let mut output = Vec::with_capacity(data.len() / 2 + 4);

        // Write size header (little-endian u32)
        let size = data.len() as u32;
        output.extend_from_slice(&size.to_le_bytes());

        // Create encoder that writes to output
        struct Writer<'a> {
            output: &'a mut Vec<u8>,
        }

        impl<'a> Write for Writer<'a> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.output.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut encoder = Encoder::new(
            Writer {
                output: &mut output,
            },
            predictor,
        );

        // Encode data byte by byte, running HCOMP after each byte
        let mut buf_ptr = 0;
        for &byte in data {
            encoder.encode_byte(byte)?;

            // Run HCOMP after encoding to update contexts for next byte
            let predictor = &mut encoder.predictor;
            hcomp_m5(predictor, byte, buf_ptr);

            buf_ptr = (buf_ptr + 1) & 0xFFFF;
        }

        // Flush encoder
        encoder.flush()?;

        Ok(output)
    }

    /// Decompress data using ZPAQ m5 configuration
    pub fn decompress_m5(data: &[u8]) -> std::io::Result<Vec<u8>> {
        // Read size header (little-endian u32)
        if data.len() < 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Input too short to read size header",
            ));
        }

        let expected_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let compressed_data = &data[4..];

        // Create predictor with m5 config
        let mut predictor = Predictor::new(23, 512, 65536); // 23 components, hh=9, hm=16
        configure_m5(&mut predictor, expected_size);

        // Create decoder
        let mut decoder = Decoder::new(compressed_data, predictor);
        decoder.fill_curr()?;

        // Decode data
        let mut output = Vec::with_capacity(expected_size);
        let mut buf_ptr = 0;
        for _ in 0..expected_size {
            match decoder.decode_byte() {
                Ok(byte) => {
                    output.push(byte);

                    // Run HCOMP after decoding to update contexts for next byte
                    let predictor = &mut decoder.predictor;
                    hcomp_m5(predictor, byte, buf_ptr);

                    buf_ptr = (buf_ptr + 1) & 0xFFFF;
                }
                Err(_) if !output.is_empty() => {
                    // EOF reached, return what we have
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(output)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_m5_configuration() {
            let mut predictor = Predictor::new(23, 512, 65536);
            configure_m5(&mut predictor, 1024);
            assert_eq!(predictor.components.len(), 23);
        }

        #[test]
        fn test_predictor_basic() {
            // Just test that predictor can be created and used
            let mut predictor = Predictor::new(1, 256, 256);
            let cm = Component::cm(8, 1000); // sizebits=8, not 256
            predictor.add_component(cm);

            // Test predict/update cycle
            for _ in 0..10 {
                let p = predictor.predict();
                assert!((0..=32767).contains(&p)); // squash returns 0..32767
                predictor.update(1);
                predictor.update(0);
            }
        }

        #[test]
        fn test_encoder_single_bit() {
            // Test encoding a single bit
            let output: Vec<u8> = Vec::new();
            let mut predictor = Predictor::new(1, 256, 256);
            let mut cm = Component::cm(1, 1000);
            // Initialize with stretched 0.5 probability
            // But CM stores unstretched, so we set it to (2048 << 17) for 0.5 probability
            if let Component::Cm { cm, .. } = &mut cm {
                cm[0] = (2048 << 17) | 1;
            }
            predictor.add_component(cm);

            struct TestWriter {
                data: Vec<u8>,
            }
            impl Write for TestWriter {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.data.extend_from_slice(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }

            let writer = TestWriter { data: output };
            let mut encoder = Encoder::new(writer, predictor);

            // Try to encode one bit
            let result = encoder.encode_bit(0);
            println!("Encode result: {:?}", result);
            assert!(result.is_ok());
        }

        #[test]
        fn test_arithmetic_coding_fixed_prob() {
            // Test full encode/decode cycle with fixed probability
            let input = b"AB";

            println!("=== Testing Full Arithmetic Coding ===");

            // Compress
            let mut output = Vec::new();
            let size = input.len() as u32;
            output.extend_from_slice(&size.to_le_bytes());

            {
                struct Writer<'a> {
                    output: &'a mut Vec<u8>,
                }

                impl<'a> Write for Writer<'a> {
                    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                        self.output.extend_from_slice(buf);
                        Ok(buf.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }

                let mut predictor = Predictor::new(1, 256, 256);
                let mut cm = Component::cm(1, 1000);
                if let Component::Cm { cm: cm_table, .. } = &mut cm {
                    cm_table[0] = (2048 << 17) | 1; // Fixed 0.5 probability (stretched 2048)
                }
                predictor.add_component(cm);

                let mut encoder = Encoder::new(
                    Writer {
                        output: &mut output,
                    },
                    predictor,
                );
                for &byte in input {
                    encoder.encode_byte(byte).unwrap();
                }
                encoder.flush().unwrap();
            }

            println!("Compressed: {} bytes", output.len());

            // Decompress
            let expected_size =
                u32::from_le_bytes([output[0], output[1], output[2], output[3]]) as usize;
            let compressed_data = &output[4..];

            let mut predictor = Predictor::new(1, 256, 256);
            let mut cm = Component::cm(1, 1000);
            if let Component::Cm { cm: cm_table, .. } = &mut cm {
                cm_table[0] = (2048 << 17) | 1;
            }
            predictor.add_component(cm);

            let mut decoder = Decoder::new(compressed_data, predictor);
            decoder.fill_curr().unwrap();

            let mut result = Vec::new();
            for _ in 0..expected_size {
                result.push(decoder.decode_byte().unwrap());
            }

            println!("Decompressed: {:?}", result);
            assert_eq!(input.to_vec(), result);
            println!("✓ Full arithmetic coding test passed!");
        }

        #[test]
        fn test_m5_compress_decompress() {
            let input = b"Hello, world! This is a test of the m5.zpaql \
                          compression and decompression implementation in Rust. \
                          Let's see how it performs with this sample text.";

            println!("Starting compression...");
            let compressed = compress_m5(input);
            println!("Compression result: {:?}", compressed.is_ok());

            if let Ok(comp) = compressed {
                println!(
                    "Original size: {}, Compressed size: {}",
                    input.len(),
                    comp.len()
                );
                println!("Starting decompression...");
                let decompressed = decompress_m5(&comp);
                println!("Decompression result: {:?}", decompressed.is_ok());

                if let Ok(dec) = decompressed {
                    assert_eq!(input.to_vec(), dec);
                }
            }
        }

        #[test]
        fn test_m5_simple_pattern() {
            // Test with a simple repetitive pattern that should compress well
            let input = vec![b'A'; 1024];
            println!("Input: {} bytes of 'A'", input.len());

            let compressed = compress_m5(&input).unwrap();
            println!(
                "Compressed: {} bytes (ratio: {:.2}%)",
                compressed.len(),
                (compressed.len() as f64 / input.len() as f64) * 100.0
            );

            let decompressed = decompress_m5(&compressed).unwrap();
            assert_eq!(input.to_vec(), decompressed);
            println!("✓ Simple pattern test passed!");
        }
    }
}
