# Hamburg 1:1 in Minecraft: common commands. `just --list` shows all recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

hhmc := "cargo run --quiet --release -p hhmc-cli --"

default:
    @just --list

# Download and verify datasets, e.g. `just fetch --area pilot`.
fetch *ARGS:
    {{hhmc}} fetch {{ARGS}}

# Normalize raw data to per-tile COG / FlatGeobuf, land-use and water rasters.
prep *ARGS:
    {{hhmc}} prep {{ARGS}}

# Derive config/block_colors.toml from the pinned client jar.
palette:
    {{hhmc}} palette

# Prep (cached) and build tiles, e.g. `just build --area pilot`.
build *ARGS:
    {{hhmc}} build {{ARGS}}

# QA report, e.g. `just qa --area pilot`.
qa *ARGS:
    {{hhmc}} qa {{ARGS}}

# Top-down web map of the built tiles into out/map/, e.g. `just map --area pilot`.
map *ARGS:
    {{hhmc}} map {{ARGS}}

# Regenerate ATTRIBUTION.md from data/sources.toml.
attribution:
    {{hhmc}} attribution

# Formatting, lints and tests, as in CI.
test:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    {{hhmc}} attribution --check
    {{hhmc}} golden check

# Golden tiles (tests/golden/): build and check them, e.g. `just golden --update` after an
# intended change.
golden *ARGS:
    {{hhmc}} golden check {{ARGS}}

# Headless vanilla server check of the test world (or `--world <dir> --tiles "rx,rz;…"`).
server-test *ARGS:
    {{hhmc}} server-test {{ARGS}}

# Alias of server-test.
server *ARGS:
    {{hhmc}} server-test {{ARGS}}

# Write the test world to out/test-world (for opening in a client).
test-world:
    {{hhmc}} world test-world
