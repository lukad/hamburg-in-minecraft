# Hamburg 1:1 in Minecraft

[![Hamburg 1:1 in Minecraft: trailer on YouTube](https://img.youtube.com/vi/SG9tozIGT20/maxresdefault.jpg)](https://www.youtube.com/watch?v=SG9tozIGT20)

[Watch the trailer on YouTube](https://www.youtube.com/watch?v=SG9tozIGT20)

A Rust pipeline that turns Hamburg's open geodata into a Minecraft Java Edition world at one block per meter. Nothing is built by hand: terrain, water, every building, trees, bridges and railways are generated from official data.

- Terrain comes from the 1 m terrain model (DGM1), and the beds of the Elbe and the harbor basins from the federal waterways survey (DGM-W). Water bodies are filled to their levels, with locks and weirs as barriers.
- Buildings come from the textured LoD3 city model where it exists and from LoD2 elsewhere, voxelized with facade materials and windows matched from the textures.
- Street trees come from the tree cadasters, park trees from the canopy height.
- Bridges, elevated railways and roads, rails and landing stages come from the cadaster (ALKIS).
- Ground cover follows the land use and the colors of the 20 cm orthophoto.

The full build covers the Hamburg mainland (747 km², 3082 region files, about 13 GB), clipped at the city boundary. It is a plain vanilla world: no mods, resource packs or data packs are needed.

## Limitations

This is a fully automated conversion, and it looks like one. It cannot hold a candle to hand-built Minecraft cities, whose builders spend years on facades, roofs and streets. One block per meter is coarse, and the world is only as good as the data and the rules that turn it into blocks:

- Buildings are shells with a floor every story and nothing inside. About 60 % come from the textured LoD3 model, which dates from 2020; the rest, including everything built since, are LoD2 models with idealized roofs. Facade blocks and windows are guessed from photo textures and colors, and roof blocks from the orthophoto.
- Ground cover follows the cadaster's land-use classes, which are coarse: a residential plot is one class, so lawns and paving inside it look the same.
- Trees are generic shapes placed from the tree cadasters and the canopy height; smaller vegetation is approximate at best.
- Water stands at fixed levels (mean high water on the Elbe), without tides or currents.
- Bridges and railways are reconstructed from footprints and surface heights. Complex places such as stations with tracks on several levels show steps and gaps.
- Signs, lamps, benches, cars, shop fronts and everything else that makes a street are missing, because the data does not have them.

The QA report flags tiles where the result disagrees with the data; in the full city about 7 % of tiles are flagged.

## Download and play

Download the world: [hamburg-1to1-full.zip](https://hhmc.jibbe.rs/hamburg-1to1-full.zip) (13.0 GB).

1. Unzip the download. It contains one folder, `hamburg-1to1/`.
2. Move that folder into your Minecraft `saves` folder:
   - Official launcher on Linux: `~/.minecraft/saves/`
   - Official launcher on macOS: `~/Library/Application Support/minecraft/saves/`
   - Official launcher on Windows: `%APPDATA%\.minecraft\saves\`
   - Prism Launcher and similar: the instance's `minecraft/saves/` (or `.minecraft/saves/`) folder, e.g. via *Folder* on the instance.
3. Start Minecraft Java Edition 26.3 and pick the world in *Singleplayer*. You spawn on the Rathausmarkt.

Give the game at least 4 GB of memory. The first visit to a district takes a moment while the game lights its chunks. Performance and level-of-detail mods such as Sodium, Voxy or Distant Horizons work with it as with any vanilla world.

## Building the world yourself

### Requirements

- Rust (stable; see `rust-toolchain.toml`) and [just](https://github.com/casey/just). With [mise](https://mise.jdx.dev), `mise install` sets up just and Java from `mise.toml`.
- GDAL command-line tools (`gdalwarp`, `gdalbuildvrt`, `ogr2ogr`) on the `PATH`.
- For the headless server test only: Java in the version the pinned Minecraft server needs (see below).
- Disk and memory for the full city: about 80 GB of downloads in `data/raw/`, 15 GB of intermediate data, 13 GB for the world; prep peaks at about 35 GB of RAM.

### Usage

Areas are defined in `config/areas.toml`: `pilot` (Speicherstadt, HafenCity and the Elbe front, a quick start) and `full` (the mainland).

```bash
just fetch --area pilot   # download and verify the datasets (pinned in data/sources.toml)
just build --area pilot   # prepare the data and build out/world/
just qa --area pilot      # QA checks and an HTML report in out/qa/
just map --area pilot     # top-down web map in out/map/
```

`cargo run --release -p hhmc-cli -- package --area pilot` zips the world with its attribution and a README for players. `hhmc --help` lists every command. Stages are cached per tile; a rebuild only redoes what changed.

The world uses one block per meter, sea level (NHN 0 m) at y = 0, and x = E − E₀, z = N₀ − N for ETRS89 / UTM 32N (EPSG:25832) coordinates, with the origin in `config/world.toml`.

### Tests

`just test` runs formatting, lints, unit tests, the attribution check and the golden tiles. The golden tiles are small clips of the input data with expected block counts and renders. Their data is not in the repository; `tests/golden/README.md` explains how to cut it from a build.

`just server-test` starts the official Minecraft server on a world and checks it over RCON. The server is downloaded from Mojang and only runs if you accept the [Minecraft EULA](https://aka.ms/MinecraftEULA) by setting `HHMC_ACCEPT_MINECRAFT_EULA=true`. `just palette` derives the average block colors in `config/block_colors.toml` from the Minecraft client. Both jars are kept in `tools/minecraft/`, which is never committed.

## Data and licenses

The code is licensed under the [MIT License](LICENSE). The data is not: every dataset is used under its own license, and anything built from it, including a generated world, must carry its attribution. [ATTRIBUTION.md](ATTRIBUTION.md) lists every dataset with its publisher, license and the attribution it requires; `hhmc package` includes it in every world zip.

- Freie und Hansestadt Hamburg (LGV, BUKEA, Hamburg Port Authority): Datenlizenz Deutschland – Namensnennung – 2.0 (dl-de/by-2-0).
- Wasserstraßen- und Schifffahrtsverwaltung des Bundes (WSV): GeoNutzV.
- © OpenStreetMap contributors, ODbL: used only for the positions of locks, weirs, sluices and dams.

Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.
