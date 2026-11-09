# VoxelAtlas

Offline Rust renderer for **Minecraft Java 1.12.2**: incremental top-down and
isometric maps, WebP tiles, and a static browser viewer. No GPU or application
server required.

## Run

```sh
nix build
cp examples/minimal.toml render.toml
# Set world and texture paths in render.toml.
./result/bin/voxelatlas validate --config render.toml
./result/bin/voxelatlas render --config render.toml
```

Supply your own extracted `assets/minecraft/textures/` directory and a stable
world snapshot. Inputs are read-only; snapshots are not created automatically.
Non-Nix builds: Rust 1.89+ and a C toolchain, then `cargo build --release`.

Encoding: `fast` (default, exact), `compact` (exact, more CPU), or `indexed`
(smaller, 256-color quantization; changes colors and potentially alpha).
`skip_corrupt_chunks = true` warns and omits damaged chunks without editing worlds;
strict mode is the default. Changing inputs and I/O errors still stop publication.
Old tiles are retained until `voxelatlas gc --config render.toml`.

## NixOS

Import `inputs.voxelatlas.nixosModules.default`; adapt
[examples/production.nix](examples/production.nix) for services, timers, and nginx.
Use snapshots for world paths and grant the service read access. Set `workers`,
`memoryMax`, and optionally `cpuQuota` explicitly; low priority is not isolation.
Defaults: 3 GiB shared hard limit, 2.5 GiB pressure threshold, no swap, Nice 19, CPU/IO weight 1,
and idle I/O scheduling. The 128 MiB decoded-chunk cache is not a total RAM limit.

```sh
sudo systemctl start voxelatlas-render
journalctl -fu voxelatlas-render
```

**Preview:** complex models, lighting, and tile-entity appearances remain
incomplete. No legacy URL translation or MySQL identity importer. Optional player
data uses `update-data`; chunk activity is inferred, not live server telemetry.
Validate a separate preview before replacing an existing deployment.

## Check

```sh
nix develop --command cargo test --release --all-targets
nix develop --command cargo clippy --all-targets -- -D warnings
nix flake check
```

MIT; see [LICENSE](LICENSE). No Minecraft assets or worlds included.
Not affiliated with Mojang or Microsoft.
