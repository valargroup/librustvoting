use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use ff::PrimeField;
use pasta_curves::Fp;
use rayon::prelude::*;

use imt_tree::tree::build_nf_ranges;

#[derive(Parser)]
#[command(name = "pir-export", about = "Build PIR tier databases from nullifier data")]
struct Args {
    /// Path to nullifiers.bin (sorted 32-byte Fp elements).
    #[arg(long)]
    nullifiers: PathBuf,

    /// Output directory for tier files.
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
        .map(|chunk| {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(chunk);
            Fp::from_repr(arr).unwrap()
        })
        .collect();
    Ok(nfs)
}

fn main() -> Result<()> {
    let args = Args::parse();

    let t_total = Instant::now();

    // Load nullifiers
    eprintln!("Loading nullifiers from {:?}...", args.nullifiers);
    let t0 = Instant::now();
    let mut nfs = load_nullifiers(&args.nullifiers)?;
    eprintln!(
        "  Loaded {} nullifiers in {:.1}s",
        nfs.len(),
        t0.elapsed().as_secs_f64()
    );

    // Sort and build ranges
    eprintln!("Sorting and building gap ranges...");
    let t1 = Instant::now();
    nfs.sort();
    let ranges = build_nf_ranges(nfs);
    eprintln!(
        "  {} ranges built in {:.1}s",
        ranges.len(),
        t1.elapsed().as_secs_f64()
    );

    // Build PIR tree
    eprintln!("Building depth-{} PIR tree...", pir_export::PIR_DEPTH);
    let tree = pir_export::build_pir_tree(ranges);
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

    // Export tier files
    eprintln!("Exporting tier files to {:?}...", args.output_dir);
    pir_export::export_all(&tree, &args.output_dir, None)?;

    eprintln!(
        "\nDone! Total time: {:.1}s",
        t_total.elapsed().as_secs_f64()
    );
    Ok(())
}
