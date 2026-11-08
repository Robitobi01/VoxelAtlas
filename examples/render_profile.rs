//! Render-only benchmark; optional directory writes exact RGBA reference tiles.
use std::{hint::black_box, path::Path, time::Instant};
use voxelatlas::{
    assets::Assets,
    config::Config,
    render::{project, render_tile},
    world::World,
};

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let config = Config::load(Path::new(&args[1]))?;
    let map = &config.maps[0];
    let wc = &config.worlds[&map.world];
    let world = World::open(
        &wc.dimension.directory(&wc.path),
        config.cache_mib * 1024 * 1024,
    )?;
    let assets = Assets::load(&config.textures)?;
    let positions = world.chunk_positions();
    let mut tiles = Vec::new();
    for rotation in 0..4 {
        for i in 0..25 {
            let (x, z) = positions[i * (positions.len() - 1) / 24];
            let (sx, sy) = project(
                f64::from(x * 16 + 8),
                64.,
                f64::from(z * 16 + 8),
                map.view,
                rotation,
            );
            tiles.push((
                rotation,
                (sx / f64::from(config.tile_size)).floor() as i32,
                (sy / f64::from(config.tile_size)).floor() as i32,
            ));
        }
    }
    // Populate decoded-chunk cache before timing, without retaining image buffers.
    for &(r, x, y) in &tiles {
        black_box(render_tile(
            &world,
            &assets,
            map,
            r,
            &[],
            x,
            y,
            config.tile_size,
        ));
    }
    let start = Instant::now();
    let mut hash = blake3::Hasher::new();
    for &(r, x, y) in &tiles {
        let image = black_box(render_tile(
            &world,
            &assets,
            map,
            r,
            &[],
            x,
            y,
            config.tile_size,
        ));
        hash.update(image.as_raw());
    }
    println!(
        "{:?}: {} tiles {:.3}s RGBA {}",
        map.view,
        tiles.len(),
        start.elapsed().as_secs_f64(),
        hash.finalize()
    );
    if let Some(directory) = args.get(2) {
        let compare = args.get(3).is_some_and(|s| s == "compare");
        if !compare {
            std::fs::create_dir_all(directory)?;
        }
        for &(r, x, y) in &tiles {
            let image = render_tile(&world, &assets, map, r, &[], x, y, config.tile_size);
            let path = Path::new(directory).join(format!("{r}_{x}_{y}.png"));
            if compare {
                anyhow::ensure!(
                    image == image::open(&path)?.to_rgba8(),
                    "RGBA mismatch {}",
                    path.display()
                );
            } else {
                image.save(path)?;
            }
        }
        if compare {
            println!("All {} decoded RGBA tiles match reference", tiles.len());
        }
    }
    world.check_consistent()?;
    Ok(())
}
