use anyhow::Result;
use orchard::vote::{calculate_merkle_paths, OrchardHash};
use pasta_curves::{group::ff::PrimeField as _, Fp};
use rusqlite::Connection;

pub fn list_nf_ranges(connection: &Connection) -> Result<Vec<Fp>> {
    let mut s = connection.prepare("SELECT hash FROM nfs")?;
    let rows = s.query_map([], |r| {
        let v = r.get::<_, [u8; 32]>(0)?;
        let v = Fp::from_repr(v).unwrap();
        Ok(v)
    })?;
    let mut nfs = rows.collect::<Result<Vec<_>, _>>()?;
    nfs.sort();
    let nf_tree = build_nf_ranges(nfs);
    Ok(nf_tree)
}

pub fn compute_nf_root(connection: &Connection) -> Result<OrchardHash> {
    let nf_tree = list_nf_ranges(connection)?;
    let (nf_root, _) = calculate_merkle_paths(0, &[], &nf_tree);

    Ok(OrchardHash(nf_root.to_repr()))
}

pub fn build_nf_ranges(nfs: impl IntoIterator<Item = Fp>) -> Vec<Fp> {
    let mut prev = Fp::zero();
    let mut leaves = vec![];
    for r in nfs {
        // Skip empty ranges when nfs are consecutive
        // (with statistically negligible odds)
        if prev < r {
            // Ranges are inclusive of both ends
            let a = prev;
            let b = r - Fp::one();

            leaves.push(a);
            leaves.push(b);
        }
        prev = r + Fp::one();
    }
    if prev != Fp::zero() {
        // overflow when a nullifier == max
        let a = prev;
        let b = Fp::one().neg();

        leaves.push(a);
        leaves.push(b);
    }
    leaves
}
