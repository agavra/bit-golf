# compression-golf

**Can you beat 5,093,619 bytes?**

A compression challenge: encode 1,000,000 GitHub events into the smallest possible binary format.

## Leaderboards

There are **two leaderboards** for this challenge:

### Training Dataset Leaderboard

This leaderboard uses the `data.json.gz` dataset included in the repo. Use this to develop and test your codec.

| Rank | Who                                | Size (Bytes) |
|------|------------------------------------|--------------|
| 1    | [lifthrasiir](src/lifthrasiir.rs)  | 5,002,122    |
| 2    | [yimingqiao](src/yimingqiao.rs)    | 5,093,619    |
| 3    | [kjcao](src/kjcao.rs)              | 5,188,464    |
| 4    | [fulmicoton](src/fulmicoton.rs)    | 5,677,291    |
| 5    | [xinyuzeng](src/xinyuzeng.rs)      | 5,784,827    |
| 6    | [cometkim](src/cometkim.rs)        | 5,789,637    |
| 7    | [natebrennand](src/natebrennand.rs)| 5,996,236    |
| 8    | [jakedgy](src/jakedgy.rs)          | 6,402,499    |
| 9    | [hachikuji](src/hachikuji.rs)      | 6,524,516    |
| 10   | [XiangpengHao](src/xiangpenghao.rs)| 6,847,283    |
| 11   | [agavra](src/agavra.rs)            | 7,273,680    |
| 12   | [fabinout](src/fabinout.rs)        | 7,283,778    |
| 13   | [samsond](src/samsond.rs)          | 7,564,554    |
| 13   | *[Zstd(22)](src/zstd.rs)*          | 11,917,798   |
| 14   | *[Zstd(9)](src/zstd.rs)*           | 17,869,403   |
|      | *[Naive (baseline)](src/naive.rs)* | 210,727,389  |

### Evaluation Dataset Leaderboard

To prevent overfitting to the training data, a separate **evaluation dataset** will be announced on **March 1st, 2026** when the challenge ends. All submitted codecs will be run against this hidden dataset.

**Two winners will be announced:**
1. Best compression on the **training dataset**
2. Best compression on the **evaluation dataset**

*[Submit a PR](https://github.com/agavra/compression-golf/pulls) to claim your spot!*

## The Challenge

Your codec must:

1. Implement the `EventCodec` trait
2. Perfectly reconstruct the original data (lossless)
3. Beat the Naive codec (210,727,389 bytes)

## Quick Start

```bash
git clone https://github.com/agavra/compression-golf
cd compression-golf
gunzip -k data.json.gz  # decompress the dataset
cargo run --release
```

The dataset is distributed as `data.json.gz` to keep the repo size manageable.

To run only your codec:

```bash
cargo run --release -- --codec yourname
```

To test against a different dataset:

```bash
cargo run --release -- path/to/your/data.json
```

## How to Compete

1. Fork this repo
2. Create `src/<your-github-username>.rs` implementing `EventCodec`
3. Add it to `main.rs` (see [Adding Your Codec](#adding-your-codec))
4. Run `cargo run --release` to verify it beats the current best
5. **Submit a PR** with only your single codec file to claim your spot on the leaderboard

**Important:** Your PR should only add one file: `src/<your-github-username>.rs`. Do not modify other files (except the necessary `main.rs` imports). This keeps submissions clean and easy to review.

## The Data

Each of the 11,351 events contains:

```rust
pub struct EventKey {
    pub id: String,          // numeric string, e.g., "2489651045"
    pub event_type: String,  // 14 unique types (e.g., "PushEvent", "WatchEvent")
}

pub struct EventValue {
    pub repo: Repo,
    pub created_at: String,  // ISO 8601, e.g., "2015-01-01T15:00:00Z"
}

pub struct Repo {
    pub id: u64,             // 6,181 unique repos
    pub name: String,        // e.g., "owner/repo"
    pub url: String,         // e.g., "https://api.github.com/repos/owner/repo"
}
```

## The Interface

```rust
pub trait EventCodec {
    fn name(&self) -> &str;
    fn encode(&self, events: &[(EventKey, EventValue)]) -> Result<Bytes, Box<dyn Error>>;
    fn decode(&self, bytes: &[u8]) -> Result<Vec<(EventKey, EventValue)>, Box<dyn Error>>;
}
```

## Adding Your Codec

1. Create `src/yourname.rs`:

```rust
use bytes::Bytes;
use std::error::Error;
use crate::codec::EventCodec;
use crate::{EventKey, EventValue};

pub struct YournameCodec;

impl YournameCodec {
    pub fn new() -> Self {
        Self
    }
}

impl EventCodec for YournameCodec {
    fn name(&self) -> &str {
        "yourname"
    }

    fn encode(&self, events: &[(EventKey, EventValue)]) -> Result<Bytes, Box<dyn Error>> {
        todo!()
    }

    fn decode(&self, bytes: &[u8]) -> Result<Vec<(EventKey, EventValue)>, Box<dyn Error>> {
        todo!()
    }
}
```

2. Add to `src/main.rs`:

```rust
mod yourname;
use yourname::YournameCodec;
```

3. Add your codec to the `codecs` vec in `main()`:

```rust
let codecs: Vec<(Box<dyn EventCodec>, &[(EventKey, EventValue)])> = vec![
    // ... existing codecs ...
    (Box::new(YournameCodec::new()), &sorted_events),
];
```

## Rules

- Codec must be deterministic
- No external data or pretrained models
- Must compile with stable Rust
- Decode must produce byte-identical output to sorted input
- PRs must add a single file: `src/<your-github-username>.rs`
- **Submission deadline: March 1st, 2025** — evaluation dataset revealed and winners announced

## Generating Your Own Evaluation Dataset

Want to test your codec against different data? You can generate your own dataset
from [GitHub Archive](https://www.gharchive.org/), which provides hourly dumps of all public GitHub
events.

### Download Raw Data

GitHub Archive files are available at `https://data.gharchive.org/{YYYY-MM-DD-H}.json.gz`:

```bash
# Download a single hour
curl -O https://data.gharchive.org/2024-01-15-12.json.gz

# Download a full day (24 files)
for hour in {0..23}; do
  curl -O "https://data.gharchive.org/2024-01-15-${hour}.json.gz"
done
```

### Extract Required Fields

The raw GitHub Archive data contains many fields, but this challenge only uses a subset. Use `jq` to extract the required fields:

```bash
# Extract fields and combine into a single file
gunzip -c 2024-01-15-*.json.gz | jq -c '{
  id,
  type,
  repo: {id: .repo.id, name: .repo.name, url: .repo.url},
  created_at
}' > my_data.json
```

### Limit to a Specific Size (Optional)

```bash
# Take the first N events
head -n 100000 my_data.json > my_data_100k.json
```

### Run Against Your Dataset

```bash
cargo run --release -- my_data_100k.json
```

## Resources

- [Strategies for beating the current best](#) *(blog post coming soon)*
- [GitHub Archive](https://www.gharchive.org/) — source of the dataset

## License

MIT
