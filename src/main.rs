use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use pdf_shrink::{ImageOptions, Options, Output, Preset, compress, compress_to_target};
use serde_json::json;

/// Shrink a PDF: rewrite its structure compactly and, unless the preset is
/// `lossless`, downsample and re-encode its images.
///
/// The output is only written when it is smaller than the input.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// PDF to compress.
    input: PathBuf,

    /// Where to write the result [default: <input>.compressed.pdf].
    #[arg(short, long, conflicts_with = "in_place")]
    output: Option<PathBuf>,

    /// Replace the input file with the result.
    #[arg(long)]
    in_place: bool,

    /// lossless | high (200 dpi) | medium (150 dpi) | low (96 dpi).
    /// With --target-size this is the strongest preset allowed.
    #[arg(short, long, default_value = "medium")]
    preset: Preset,

    /// Use the gentlest preset that gets the file to this size or below,
    /// e.g. 25M, 800K, 1.5G (binary units) or a number of bytes.
    #[arg(short, long, value_parser = parse_size)]
    target_size: Option<usize>,

    /// Override the preset's image resolution limit.
    #[arg(long, conflicts_with = "target_size")]
    dpi: Option<f64>,

    /// Override the preset's JPEG quality (1-100).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100), conflicts_with = "target_size")]
    quality: Option<u8>,

    /// Print a machine-readable report on stdout.
    #[arg(long)]
    json: bool,
}

fn parse_size(text: &str) -> Result<usize, String> {
    let lower = text.trim().to_ascii_lowercase();
    let digits = lower.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let multiplier = match &lower[digits.len()..] {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        unit => return Err(format!("unknown size unit `{unit}`")),
    };
    match digits.trim().parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => Ok((n * multiplier) as usize),
        _ => Err(format!("`{text}` is not a size")),
    }
}

fn run(args: &Args) -> Result<serde_json::Value, String> {
    let input = std::fs::read(&args.input).map_err(|e| format!("{}: {e}", args.input.display()))?;

    let custom = args.dpi.is_some() || args.quality.is_some();
    let Output { bytes, mut report } = match args.target_size {
        Some(target) => compress_to_target(&input, target, args.preset),
        None => {
            let mut options = Options::from(args.preset);
            if custom {
                let base = options.images.unwrap_or(ImageOptions {
                    max_dpi: 150.0,
                    quality: 72,
                });
                options.images = Some(ImageOptions {
                    max_dpi: args.dpi.unwrap_or(base.max_dpi),
                    quality: args.quality.unwrap_or(base.quality),
                });
            }
            compress(&input, &options)
        }
    }
    .map_err(|e| e.to_string())?;
    if args.target_size.is_none() && !custom {
        report.preset = Some(args.preset);
    }

    let output = if args.in_place {
        args.input.clone()
    } else {
        args.output
            .clone()
            .unwrap_or_else(|| args.input.with_extension("compressed.pdf"))
    };
    let written = bytes.len() < input.len();
    if written {
        write_atomically(&output, &bytes).map_err(|e| format!("{}: {e}", output.display()))?;
    }

    let mut value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    value["input"] = json!(args.input);
    value["output"] = if written { json!(output) } else { json!(null) };
    value["written"] = json!(written);
    value["target_bytes"] = json!(args.target_size);
    value["target_met"] = json!(args.target_size.map(|t| bytes.len().min(input.len()) <= t));
    Ok(value)
}

/// Write beside the destination, then rename over it, so a crash never leaves a
/// half-written PDF where a good one was.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let temporary = path.with_file_name(name);
    std::fs::write(&temporary, bytes)
        .and_then(|()| std::fs::rename(&temporary, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(report) if args.json => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Ok(report) => {
            let n = |key: &str| report[key].as_u64().unwrap_or(0);
            let preset = report["preset"].as_str().unwrap_or("custom");
            if report["written"] == json!(true) {
                println!(
                    "{} -> {} ({:.0}% of original, {preset}, {}/{} images recompressed)\n{}",
                    mib(n("original_bytes")),
                    mib(n("compressed_bytes")),
                    100.0 * n("compressed_bytes") as f64 / n("original_bytes").max(1) as f64,
                    n("images_recompressed"),
                    n("images_total"),
                    report["output"].as_str().unwrap_or_default(),
                );
            } else {
                println!(
                    "{}: already as small as {preset} can make it; nothing written",
                    mib(n("original_bytes"))
                );
            }
            if report["target_met"] == json!(false) {
                eprintln!(
                    "warning: could not reach the target size of {}",
                    mib(n("target_bytes"))
                );
            }
            ExitCode::SUCCESS
        }
        Err(message) if args.json => {
            println!("{}", json!({ "error": message }));
            ExitCode::FAILURE
        }
        Err(message) => {
            eprintln!("pdf-shrink: {message}");
            ExitCode::FAILURE
        }
    }
}
