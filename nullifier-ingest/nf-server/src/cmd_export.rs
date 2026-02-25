use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use ff::{Field, PrimeField};
use pasta_curves::Fp;
use rayon::prelude::*;

use imt_tree::tree::build_nf_ranges;

#[derive(ClapArgs)]
pub struct Args {
    /// Directory containing nullifiers.bin and nullifiers.checkpoint.
    #[arg(long, default_value = ".")]
    data_dir: PathBuf,

    /// Output directory for tier files (tier0.bin, tier1.bin, tier2.bin, pir_root.json).
    #[arg(long, default_value = "./pir-data")]
    output_dir: PathBuf,
}

/// Load nullifiers from a raw binary file (32 bytes per element, no header).
fn load_nullifiers(path: &std::path::Path) -> Result<Vec<Fp>> {
    let data = std::fs::read(path).context("read nullifiers file")?;
    anyhow::ensure!(
        data.len() % 32 == 0,
        "corrupt nullifiers file: size {} is not a multiple of 32",
        data.len()
    );
    let nfs: Vec<Fp> = data
        .par_chunks_exact(32)
        .enumerate()
        .map(|(i, chunk)| -> Result<Fp> {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(chunk);
            Option::from(Fp::from_repr(arr)).ok_or_else(|| {
                anyhow::anyhow!("invalid non-canonical nullifier encoding at index {}", i)
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(nfs)
}

pub fn run(args: Args) -> Result<()> {
    let t_total = Instant::now();

    let nf_path = args.data_dir.join("nullifiers.bin");
    let cp_path = args.data_dir.join("nullifiers.checkpoint");

    // Load nullifiers
    eprintln!("Loading nullifiers from {:?}...", nf_path);
    let t0 = Instant::now();
    let mut nfs = load_nullifiers(&nf_path)?;
    eprintln!(
        "  Loaded {} nullifiers in {:.1}s",
        nfs.len(),
        t0.elapsed().as_secs_f64()
    );

    // Sort and build ranges
    eprintln!("Sorting and building gap ranges...");
    let t1 = Instant::now();
    nfs.sort();
    // Inject sentinels at k * 2^250 for k=0..16 (required by circuit gap-width constraint)
    let step = Fp::from(2u64).pow([250, 0, 0, 0]);
    let sentinels: Vec<Fp> = (0u64..=16).map(|k| step * Fp::from(k)).collect();
    nfs.extend(sentinels);
    nfs.sort();
    nfs.dedup();
    let ranges = build_nf_ranges(nfs);
    eprintln!(
        "  {} ranges built in {:.1}s",
        ranges.len(),
        t1.elapsed().as_secs_f64()
    );

    // Build PIR tree
    eprintln!("Building depth-{} PIR tree...", pir_export::PIR_DEPTH);
    let tree = pir_export::build_pir_tree(ranges)?;
    eprintln!(
        "  Root-{}: {}",
        pir_export::PIR_DEPTH,
        hex::encode(tree.root26.to_repr())
    );
    eprintln!(
        "  Root-{}: {}",
        pir_export::FULL_DEPTH,
        hex::encode(tree.root29.to_repr())
    );

    // Read sync height from checkpoint file if present.
    let height = if cp_path.exists() {
        let cp_data = std::fs::read(&cp_path)
            .with_context(|| format!("read checkpoint file {:?}", cp_path))?;
        anyhow::ensure!(
            cp_data.len() >= 8,
            "checkpoint file too small: {} bytes (expected at least 8)",
            cp_data.len()
        );
        let h = u64::from_le_bytes(cp_data[..8].try_into().map_err(|_| {
            anyhow::anyhow!("checkpoint height prefix must be exactly 8 bytes")
        })?);
        eprintln!("  Checkpoint sync height: {}", h);
        Some(h)
    } else {
        None
    };

    // Export tier files
    eprintln!("Exporting tier files to {:?}...", args.output_dir);
    pir_export::export_all(&tree, &args.output_dir, height)?;

    eprintln!(
        "\nDone! Total time: {:.1}s",
        t_total.elapsed().as_secs_f64()
    );
    Ok(())
}
