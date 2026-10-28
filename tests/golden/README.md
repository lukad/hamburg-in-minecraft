# Golden tiles

Small clips of the input data, built from a clean root and checked with `hhmc qa`.

The repository holds each clip's definition (`golden.toml`: tile, 64 × 64 m window, description) and its expectations (`expected.json`, `expected.png`). The clip data itself (`data/`) is not committed. Cut it locally from the prepared inputs of a full build of the area that contains the clip.

## What a clip holds

- One tile's prepared inputs in `data/prepared/`, with the actual data within the window, the 32 m tile buffer around it and the footprints of the buildings that reach into it.
- Flat, neutral fill everywhere else in the tile.
- The LoD3 textures and the cadaster trees it needs, in `data/raw/`.

## Cutting, checking and adding clips

`hhmc golden extract --name <name>` cuts a clip from its `golden.toml`. It needs `data/prepared/` (and the LoD3 and tree files in `data/raw/`) of a build that covers the tile.

`hhmc golden check` is part of `just test` and skips clips without data. For each clip it:

1. copies the repository's `config/`, `data/sources.toml` and the clip into `out/golden/<name>/`;
2. runs the stages and `hhmc qa` on the tile;
3. compares the window's block counts (per block, within 5 % or 16 blocks) and its top-down render (at most 2 % of pixels visibly different) with `expected.json` and `expected.png`.

After an intended change, `just golden --update` rewrites the expectations. A new clip is cut with `hhmc golden extract --name <name> --tile=<rx>,<rz> --at <x>,<z> --description "…"`.

| Clip | Area | Shows |
| --- | --- | --- |
| `fleet-bridge` | `pilot` | A road bridge over a Fleet: deck, lanes carried across, abutments over the dipped bank, railings. |
| `u3-baumwall` | `pilot` | The U3 viaduct near Baumwall: elevated deck, rails, supports, the street below. |
| `wasserschloss` | `pilot` | Speicherstadt warehouses with LoD3 facades and textures beside a Fleet. |

The clips and expectations are derived from the datasets listed in [ATTRIBUTION.md](../../ATTRIBUTION.md), under their licenses.
