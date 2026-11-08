//! Verify full published maps have identical decoded pixels at every zoom.
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

fn inventory(root: &Path) -> Result<BTreeMap<String, String>> {
    let manifest: voxelatlas::output::Manifest =
        serde_json::from_slice(&std::fs::read(root.join("current.json"))?)?;
    let mut entries = BTreeMap::new();
    for map in manifest.maps {
        for hash in map.tile_indexes.values() {
            let index: serde_json::Value = serde_json::from_slice(&std::fs::read(
                root.join("indexes").join(format!("{hash}.json")),
            )?)?;
            for (key, value) in index["tiles"].as_object().context("invalid tile index")? {
                entries.insert(
                    format!("{}/{}/{key}", map.id, map.rotation),
                    value.as_str().context("invalid tile hash")?.to_owned(),
                );
            }
        }
    }
    Ok(entries)
}
fn digests(
    root: &Path,
    entries: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, blake3::Hash>> {
    let hashes: std::collections::BTreeSet<_> = entries.values().collect();
    let results: Vec<Result<_>> = hashes
        .par_iter()
        .map(|hash| {
            let rgba = image::open(root.join("tiles").join(format!("{hash}.webp")))?.into_rgba8();
            Ok(((*hash).clone(), blake3::hash(rgba.as_raw())))
        })
        .collect();
    results.into_iter().collect()
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let left = PathBuf::from(args.next().context("expected two output directories")?);
    let right = PathBuf::from(args.next().context("expected two output directories")?);
    let a = inventory(&left)?;
    let b = inventory(&right)?;
    ensure!(a.keys().eq(b.keys()), "tile coordinates differ");
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build()?;
    let (aa, bb) =
        pool.install(|| -> Result<_> { Ok((digests(&left, &a)?, digests(&right, &b)?)) })?;
    for (key, hash) in &a {
        ensure!(aa[hash] == bb[&b[key]], "decoded pixels differ at {key}");
    }
    println!(
        "All {} tiles match exactly, including every zoom ({} / {} unique payloads)",
        a.len(),
        aa.len(),
        bb.len()
    );
    Ok(())
}
