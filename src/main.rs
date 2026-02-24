use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};

mod agavra;
mod codec;
mod cometkim;
mod fabinout;
mod fulmicoton;
mod hachikuji;
mod jakedgy;
mod kjcao;
mod lifthrasiir;
mod naive;
mod natebrennand;
mod samsond;
mod xiangpenghao;
mod xinyuzeng;
mod yimingqiao;
mod zstd;

use agavra::AgavraCodec;
use codec::EventCodec;
use cometkim::CometkimCodec;
use fabinout::FabinoutCodec;
use hachikuji::HachikujiCodec;
use jakedgy::JakedgyCodec;
use kjcao::KjcaoCodec;
use lifthrasiir::LifthrasiirCodec;
use naive::NaiveCodec;
use natebrennand::NatebrennandCodec;
use samsond::SamsondCodec;
use xiangpenghao::XiangpengHaoCodec;
use xinyuzeng::XinyuzengCodec;
use yimingqiao::YimingQiaoCodec;
use zstd::ZstdCodec;

use crate::fulmicoton::FulmicotonCodec;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventKey {
    pub id: String,
    pub event_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventValue {
    pub repo: Repo,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub url: String,
}

#[derive(Deserialize)]
struct RawGitHubEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    repo: Repo,
    created_at: String,
}

fn load_events(path: &str) -> Result<Vec<(EventKey, EventValue)>, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let raw: RawGitHubEvent = serde_json::from_str(&line)?;
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

fn format_bytes(bytes: usize) -> String {
    let s = bytes.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

fn print_row(name: &str, size: usize, baseline: usize) {
    let improvement = if baseline > 0 {
        ((baseline as f64 - size as f64) / baseline as f64) * 100.0
    } else {
        0.0
    };

    let improvement_str = if improvement > 0.0 {
        format!("-{:.1}%", improvement)
    } else if improvement < 0.0 {
        format!("+{:.1}%", -improvement)
    } else {
        "baseline".to_string()
    };

    println!(
        "│ {:<22} │ {:>14} │ {:>10} │",
        name,
        format_bytes(size),
        improvement_str
    );
}

fn assert_events_eq(
    name: &str,
    expected: &[(EventKey, EventValue)],
    actual: &[(EventKey, EventValue)],
) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "{}: length mismatch (expected {}, got {})",
        name,
        expected.len(),
        actual.len()
    );
    let mismatches: Vec<_> = expected
        .iter()
        .zip(actual.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .take(3)
        .collect();
    assert!(
        mismatches.is_empty(),
        "{}: {} mismatches, first at index {}",
        name,
        expected
            .iter()
            .zip(actual.iter())
            .filter(|(a, b)| a != b)
            .count(),
        mismatches[0].0
    );
}

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

fn run_docker_encode(
    image: &str,
    events: &[(EventKey, EventValue)],
) -> Result<Vec<u8>, Box<dyn Error>> {
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

fn run_docker_decode(
    image: &str,
    data: &[u8],
) -> Result<Vec<(EventKey, EventValue)>, Box<dyn Error>> {
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

    // Auto-enable docker mode if targeting an external codec
    if let Some(ref filter) = codec_filter {
        let potential_path = format!("src/{}/Dockerfile", filter);
        if std::path::Path::new(&potential_path).exists() {
            docker_enabled = true;
        }
    }

    let events = load_events(&path)?;
    println!("Loaded {} events from {}\n", events.len(), path);

    let mut sorted_events: Vec<_> = events.clone();
    sorted_events.sort_by(|a, b| a.0.cmp(&b.0));

    // Table header
    println!("┌────────────────────────┬────────────────┬────────────┐");
    println!("│ Codec                  │           Size │ vs Naive   │");
    println!("├────────────────────────┼────────────────┼────────────┤");

    // Baseline for comparison
    let naive = NaiveCodec::new();
    let baseline = naive.encode(&events)?.len();

    let codecs: Vec<(Box<dyn EventCodec>, &[(EventKey, EventValue)])> = vec![
        (Box::new(NaiveCodec::new()), &events),
        (Box::new(ZstdCodec::new(9)), &events),
        // (Box::new(ZstdCodec::new(22)), &events), // commented out b/c it takes long to run
        (Box::new(AgavraCodec::new()), &sorted_events),
        (Box::new(FabinoutCodec::new()), &events),
        (Box::new(HachikujiCodec::new()), &sorted_events),
        (Box::new(XiangpengHaoCodec::new()), &sorted_events),
        (Box::new(SamsondCodec::new()), &events),
        (Box::new(JakedgyCodec::new()), &sorted_events),
        (Box::new(NatebrennandCodec::new()), &sorted_events),
        (Box::new(FulmicotonCodec), &sorted_events),
        (Box::new(XinyuzengCodec::new()), &sorted_events),
        (Box::new(KjcaoCodec::new()), &sorted_events),
        (Box::new(YimingQiaoCodec::new()), &sorted_events),
        (Box::new(CometkimCodec::new()), &events),
        (Box::new(LifthrasiirCodec::new()), &sorted_events),
    ];

    for (codec, expected) in codecs {
        // Skip if filter is set and doesn't match (always run Naive for baseline)
        if let Some(ref filter) = codec_filter {
            let name_lower = codec.name().to_lowercase();
            if !name_lower.contains(filter) && !name_lower.contains("naive") {
                continue;
            }
        }

        let encoded = codec.encode(&events)?;
        print_row(codec.name(), encoded.len(), baseline);
        let decoded = codec.decode(&encoded)?;
        assert_events_eq(codec.name(), expected, &decoded);
    }

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
