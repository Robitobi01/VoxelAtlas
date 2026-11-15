# VoxelAtlas

Offline Rust renderer for **Minecraft Java 1.12.2**: incremental top-down and
isometric maps, WebP tiles, and a static browser viewer. No GPU or application
server required.

## Run

```sh
nix-build package.nix  # or: cargo build --release
cp examples/minimal.toml render.toml
# Set world and texture paths in render.toml.
./result/bin/voxelatlas validate --config render.toml
./result/bin/voxelatlas render --config render.toml
```

Supply your own extracted `assets/minecraft/textures/` directory and a stable
world snapshot. Inputs are read-only; snapshots are not created automatically.
Use `mode = "plain"` for Nether and End maps, which have no skylight.
Non-Nix builds: Rust 1.89+ and a C toolchain, then `cargo build --release`.

Encoding: `fast` (default, exact), `compact` (exact, more CPU), or `indexed`
(smaller, 256-color quantization; changes colors and potentially alpha).
`skip_corrupt_chunks = true` warns and omits damaged chunks without editing worlds;
strict mode is the default. Changing inputs and I/O errors still stop publication.
Old tiles are retained until `voxelatlas gc --config render.toml`.

## Live chunk overlay

The viewer optionally loads `live-chunks.json` next to `current.json` (served by the deployment,
not written by the renderer) and shows loaded and lazy chunks of the selected world live:

```json
{ "survival_ow": { "url": "/mcprobe/survival/watch?chunks", "dimension": 0 } }
```

`url` is a server-sent event stream with `chunks` events
(`{"dimensions": [{"id": 0, "chunks": [[x, z], ...], "lazyChunks": [[x, z], ...]}]}`, as published by
mcprobe); `dimension` selects the entry (0 overworld, -1 nether, 1 end). Without the file there is no overlay.

## Nix

`package.nix` builds the renderer with the host's nixpkgs (`pkgs.callPackage ./package.nix { }`);
`nix-shell` gives a Rust development shell. Services, timers and nginx belong to the deployment
(scheduled `voxelatlas render`, read-only world snapshots, static hosting of the output directory).
Use stable snapshots as world paths. Low priority is not isolation: limit workers, memory and CPU.

**Preview:** complex models, lighting, and tile-entity appearances remain
incomplete. No legacy URL translation or MySQL identity importer. Optional player
data uses `update-data`; chunk activity is inferred, not live server telemetry.
Validate a separate preview before replacing an existing deployment.

## Check

```sh
nix-shell --run 'cargo test --release --all-targets'
nix-shell --run 'cargo clippy --all-targets -- -D warnings'
```

MIT; see [LICENSE](LICENSE). No Minecraft assets or worlds included.
Not affiliated with Mojang or Microsoft.
