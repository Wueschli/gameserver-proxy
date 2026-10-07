//! Generate or check a sniffer registry `index.json`. Used by the sniffers repo CI.
//!
//! `generate` reads one directory per sniffer version (`manifest.toml`, `<name>.wasm`,
//! optionally `<name>.wasm.minisig`), merges them into the previous index and writes
//! the result atomically; a failed run leaves the existing file untouched.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use wayhouse_registry::{generate, parse_index, parse_manifest, to_json, Artifact};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Merge built sniffers into an index and write it.
    Generate {
        /// Registry name stored in the index.
        #[arg(long)]
        name: String,
        /// Existing index to merge into (older versions are kept).
        #[arg(long)]
        previous: Option<PathBuf>,
        /// Where to write the index (may equal --previous).
        #[arg(long)]
        out: PathBuf,
        /// Download URL prefix; artifacts are at `<base-url>/<name>-v<version>/<name>.wasm`.
        #[arg(long)]
        base_url: String,
        /// One directory per sniffer version to publish.
        dirs: Vec<PathBuf>,
    },
    /// Parse an index with the real validator and print a summary.
    Verify { index: PathBuf },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Generate {
            name,
            previous,
            out,
            base_url,
            dirs,
        } => {
            let previous = previous
                .map(|p| -> Result<_> {
                    let bytes =
                        std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
                    parse_index(&bytes)
                        .with_context(|| format!("{} is not a valid index", p.display()))
                })
                .transpose()?;
            let artifacts = dirs
                .iter()
                .map(|d| load_artifact(d, &base_url))
                .collect::<Result<Vec<_>>>()?;
            let g = generate(&name, &artifacts, previous.as_ref())?;
            write_atomically(&out, to_json(&g.index).as_bytes())?;
            for (name, version) in &g.added {
                println!("added {name} {version}");
            }
            if g.added.is_empty() {
                println!("no changes");
            }
        }
        Command::Verify { index } => {
            let bytes =
                std::fs::read(&index).with_context(|| format!("reading {}", index.display()))?;
            let parsed =
                parse_index(&bytes).with_context(|| format!("{} is invalid", index.display()))?;
            let versions: usize = parsed.sniffers.iter().map(|s| s.versions.len()).sum();
            println!(
                "ok: {} sniffers, {versions} versions",
                parsed.sniffers.len()
            );
        }
    }
    Ok(())
}

fn load_artifact(dir: &Path, base_url: &str) -> Result<Artifact> {
    let text = std::fs::read_to_string(dir.join("manifest.toml"))
        .with_context(|| format!("reading {}/manifest.toml", dir.display()))?;
    let manifest = parse_manifest(&text).with_context(|| dir.display().to_string())?;
    let file = format!("{}.wasm", manifest.name);
    let bytes = std::fs::read(dir.join(&file))
        .with_context(|| format!("reading {}/{file}", dir.display()))?;
    let url = format!(
        "{}/{}-v{}/{file}",
        base_url.trim_end_matches('/'),
        manifest.name,
        manifest.version
    );
    let signature_url = dir
        .join(format!("{file}.minisig"))
        .exists()
        .then(|| format!("{url}.minisig"));
    Ok(Artifact {
        manifest,
        bytes,
        url,
        signature_url,
    })
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("index")
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
