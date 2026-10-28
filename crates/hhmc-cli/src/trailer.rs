//! `hhmc trailer`: a camera tour for a Flashback trailer from `config/trailer.toml`.
//!
//! Writes into `out/trailer/`:
//! - `datapack/hhmc_trailer/`: `/function hhmc:tour` switches to spectator and keeps the
//!   player halfway along every camera path for its window, so the recording holds the
//!   full-detail chunks around the camera before it runs;
//! - `editor_state.json`: Flashback's editor state (format of Flashback 0.43.6, as it
//!   saves `flashback/editor_states/<replay uuid>.json`) with one scene per shot: camera
//!   keyframes, time of day and the export range;
//! - `views.toml`: the first and last frame of every shot for `hhmc views`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use hhmc_core::{Paths, WorldConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

/// Data pack format of the pinned version (`pack_version.data_major` in the 26.3 jar's
/// version.json).
const PACK_FORMAT: u32 = 121;
const TICKS_PER_S: f64 = 20.0;

#[derive(Deserialize)]
struct TrailerConfig {
    lead_s: f64,
    tail_s: f64,
    /// Seconds of still camera exported before each shot and cut when joining: the
    /// export's first ticks deliver the chunks to the camera.
    preroll_s: f64,
    /// Camera paths longer than this (m) are scaled down about their midpoint.
    max_path_m: f64,
    time_of_day: i64,
    shot: Vec<Shot>,
}

#[derive(Deserialize)]
struct Shot {
    name: String,
    seconds: f64,
    #[serde(default)]
    time_of_day: Option<i64>,
    /// Camera positions (EPSG:25832 E, N, m NHN), spread evenly over the shot.
    camera: Vec<[f64; 3]>,
    /// One point to look at, or one per camera position.
    look_at: Vec<[f64; 3]>,
}

/// Length of a polyline.
fn path_len(p: &[[f64; 3]]) -> f64 {
    p.windows(2)
        .map(|w| {
            ((w[1][0] - w[0][0]).powi(2)
                + (w[1][1] - w[0][1]).powi(2)
                + (w[1][2] - w[0][2]).powi(2))
            .sqrt()
        })
        .sum()
}

/// The point halfway along a polyline.
fn halfway(p: &[[f64; 3]]) -> [f64; 3] {
    let mut left = path_len(p) / 2.0;
    for w in p.windows(2) {
        let l = path_len(w);
        if l >= left && l > 0.0 {
            let t = left / l;
            return [0, 1, 2].map(|i| w[0][i] + (w[1][i] - w[0][i]) * t);
        }
        left -= l;
    }
    p[0]
}

/// The path scaled about its halfway point to at most `max` meters.
fn limit_path(p: &[[f64; 3]], max: f64) -> Vec<[f64; 3]> {
    let len = path_len(p);
    if len <= max || len == 0.0 {
        return p.to_vec();
    }
    let (m, f) = (halfway(p), max / len);
    p.iter().map(|q| [0, 1, 2].map(|i| m[i] + (q[i] - m[i]) * f)).collect()
}

/// World block coordinates of an EPSG:25832 point.
fn world(w: &WorldConfig, p: [f64; 3]) -> [f64; 3] {
    [p[0] - w.origin.e0 as f64, p[2], w.origin.n0 as f64 - p[1]]
}

/// Minecraft yaw and pitch (degrees) looking from `from` to `to` (world coordinates):
/// yaw 0 faces +z (south), 90 faces −x (west); pitch is positive looking down.
fn yaw_pitch(from: [f64; 3], to: [f64; 3]) -> (f64, f64) {
    let (dx, dy, dz) = (to[0] - from[0], to[1] - from[1], to[2] - from[2]);
    let yaw = (-dx).atan2(dz).to_degrees();
    let pitch = -dy.atan2(dx.hypot(dz)).to_degrees();
    (yaw, pitch)
}

pub fn trailer(paths: &Paths) -> Result<PathBuf> {
    let w = WorldConfig::load(paths)?;
    let cfg: TrailerConfig = hhmc_core::config::load_toml(&paths.config().join("trailer.toml"))?;
    let out = paths.out().join("trailer");
    let pack = out.join("datapack").join("hhmc_trailer");
    let fdir = pack.join("data").join("hhmc").join("function");
    let _ = std::fs::remove_dir_all(&pack);
    std::fs::create_dir_all(&fdir)?;
    std::fs::write(
        pack.join("pack.mcmeta"),
        serde_json::to_string_pretty(&json!({
            "pack": {
                "description": "Hamburg 1:1 trailer tour (hhmc trailer)",
                "min_format": PACK_FORMAT,
                "max_format": PACK_FORMAT,
            }
        }))? + "\n",
    )?;

    let lead = (cfg.lead_s * TICKS_PER_S).round() as i64;
    let tail = (cfg.tail_s * TICKS_PER_S).round() as i64;
    let preroll = (cfg.preroll_s * TICKS_PER_S).round() as i64;
    ensure!((preroll + TICKS_PER_S as i64) < lead, "lead_s must exceed preroll_s by over 1 s");
    let mut scenes = Vec::new();
    let mut views = String::from(
        "# First and last frame of every trailer shot (hhmc trailer); render with\n# `hhmc views --config out/trailer/views.toml --out out/trailer/views`.\n",
    );
    let mut start = 0i64;
    for (i, s) in cfg.shot.iter().enumerate() {
        ensure!(!s.camera.is_empty(), "shot `{}`: no camera positions", s.name);
        ensure!(
            s.look_at.len() == 1 || s.look_at.len() == s.camera.len(),
            "shot `{}`: give one look_at or one per camera position",
            s.name
        );
        let dur = (s.seconds * TICKS_PER_S).round() as i64;
        let camera = limit_path(&s.camera, cfg.max_path_m);
        let cam0 = start + lead;
        let look = |k: usize| s.look_at[if s.look_at.len() == 1 { 0 } else { k }];
        // Camera keyframes, evenly over the shot.
        let mut cam = serde_json::Map::new();
        let n = camera.len();
        for (k, &p) in camera.iter().enumerate() {
            let tick = if n == 1 { cam0 } else { cam0 + dur * k as i64 / (n as i64 - 1) };
            let (pos, at) = (world(&w, p), world(&w, look(k)));
            let (yaw, pitch) = yaw_pitch(pos, at);
            cam.insert(
                tick.to_string(),
                json!({
                    "position": pos,
                    "yaw": yaw,
                    "pitch": pitch,
                    "roll": 0.0,
                    "type": "camera",
                    "interpolation_type": "SMOOTH",
                }),
            );
        }
        // The camera holds its first pose through the pre-roll.
        if let Some(first) = cam.get(&cam0.to_string()).cloned() {
            let mut hold = first;
            hold["interpolation_type"] = json!("HOLD");
            cam.insert((cam0 - preroll).to_string(), hold);
        }
        let tod = s.time_of_day.unwrap_or(cfg.time_of_day);
        let mut time = serde_json::Map::new();
        time.insert(
            start.to_string(),
            json!({"time": tod, "type": "time", "interpolation_type": "HOLD"}),
        );
        scenes.push(json!({
            "name": format!("{:02} {}", i + 1, s.name),
            "keyframeTracks": [
                {"keyframeType": "CAMERA", "keyframesByTick": cam, "enabled": true, "customColour": 0},
                {"keyframeType": "TIME_OF_DAY", "keyframesByTick": time, "enabled": true, "customColour": 0},
            ],
            "exportStartTicks": cam0 - preroll,
            "exportEndTicks": cam0 + dur,
            "history": {"entries": [], "position": 0},
        }));

        // The player waits halfway along the camera path for the whole window: the chunks
        // within the render distance load during the lead and none stream in while the
        // camera runs (streamed chunks arrive dark and coarse in an export). Paths are at
        // most `max_path_m` long, so the camera stays within half of that of the player.
        let mid = world(&w, halfway(&camera));
        let next = if i + 1 < cfg.shot.len() {
            format!("schedule function hhmc:shot_{} {}t\n", i + 2, lead + dur + tail)
        } else {
            format!("schedule function hhmc:end {}t\n", lead + dur + tail)
        };
        std::fs::write(
            fdir.join(format!("shot_{}.mcfunction", i + 1)),
            format!(
                "# {:02} {}\ntp @a {:.1} {:.1} {:.1}\ntime set {tod}\ntellraw @a {{\"text\":\"Shot {}: {}\"}}\n{next}",
                i + 1,
                s.name,
                mid[0],
                mid[1],
                mid[2],
                i + 1,
                s.name
            ),
        )?;
        for (label, k) in [("start", 0), ("end", n - 1)] {
            let (c, a) = (camera[k], look(k));
            views.push_str(&format!(
                "\n[[viewpoint]]\nname = \"{:02} {} ({label})\"\ncamera = [{:.1}, {:.1}, {:.1}]\nlook_at = [{:.1}, {:.1}, {:.1}]\n",
                i + 1,
                s.name,
                c[0],
                c[1],
                c[2],
                a[0],
                a[1],
                a[2]
            ));
        }
        start += lead + dur + tail;
    }
    std::fs::write(
        fdir.join("tour.mcfunction"),
        "# Run right after starting the Flashback recording.\ngamemode spectator @a\nweather clear\n\
         gamerule advance_time false\nfunction hhmc:shot_1\n",
    )?;
    std::fs::write(
        fdir.join("end.mcfunction"),
        "gamerule advance_time true\ntellraw @a {\"text\":\"Tour finished: stop the recording.\"}\n",
    )?;
    std::fs::write(out.join("views.toml"), views)?;

    let state = editor_state(scenes);
    let state_path = out.join("editor_state.json");
    std::fs::write(&state_path, serde_json::to_string(&state)? + "\n")?;
    info!(
        shots = cfg.shot.len(),
        recording_s = start as f64 / TICKS_PER_S,
        datapack = %pack.display(),
        editor_state = %state_path.display(),
        "trailer written"
    );
    Ok(state_path)
}

/// Flashback's editor state around the scenes (defaults as Flashback saves them; HUD
/// and spectators hidden).
fn editor_state(scenes: Vec<Value>) -> Value {
    json!({
        "replayVisuals": {
            "showChat": false, "showBossBar": false, "showTitleText": false,
            "showScoreboard": false, "showActionBar": false, "showHotbar": false,
            "renderBlocks": true, "renderEntities": true, "renderPlayers": true,
            "renderParticles": true, "renderSky": true, "skyColour": [0.0, 1.0, 0.0],
            "renderNametags": false, "renderBeaconBeams": true,
            "overrideFog": false, "overrideFogStart": 0.0, "overrideFogEnd": 256.0,
            "overrideFogColour": false, "fogColour": [0.0, 1.0, 0.0],
            "overrideFov": false, "overrideFovAmount": -1.0,
            "overrideCameraShake": false, "cameraShakeSplitParams": false,
            "cameraShakeYFrequency": 1.0, "cameraShakeYAmplitude": 1.0,
            "cameraShakeXFrequency": 1.0, "cameraShakeXAmplitude": 1.0,
            "overrideRoll": false, "overrideRollAmount": 0.0,
            "overrideWeatherMode": "NONE", "overrideTimeOfDay": -1,
            "overrideNightVision": false, "ruleOfThirdsGuide": false, "centerGuide": false,
            "cameraPath": true, "sizing": "KEEP_ASPECT_RATIO",
            "changeAspectRatio": "ASPECT_16_9", "disableServerResourcePack": false
        },
        "sceneLock": {},
        "scenes": scenes,
        "sceneIndex": 0,
        "zoomMin": 0.0,
        "zoomMax": 1.0,
        "usedByPaths": [],
        "hideDuringExport": [],
        "hideAllSpectators": true,
        "muteVoice": [], "hideNametags": [], "skinOverride": {}, "skinOverrideFromFile": {},
        "nameOverride": {}, "glowingOverride": {}, "hideTeamPrefix": [], "hideTeamSuffix": [],
        "hideBelowName": [], "hideCape": [], "filteredEntities": [], "filteredParticles": [],
        "hiddenEquipment": {}, "hiddenModelParts": {}
    })
}
/// Render distance (chunks) to export at: the game's maximum.
const EXPORT_RENDER_DISTANCE: f64 = 32.0;
/// Magic number at the start of a Flashback action file.
const FLASHBACK_MAGIC: u32 = 0xD780_E884;
/// Packets per file of a replay's `level_chunk_caches/`.
const CHUNK_CACHE_SIZE: usize = 10_000;

/// Writes one replay per shot, cut from a Flashback recording of the tour, and installs
/// each one's editor state (its scene only). Flashback must not have them open.
///
/// Flashback's replay server keeps a viewer at the camera and sends it every chunk the
/// replay holds within the render distance; chunks entering that range while the camera
/// moves render a frame or more before their mesh and light (holes, black facades, LODs
/// popping), and a replay never forgets a chunk (it does not record unloads). So each
/// shot's replay holds only the chunks within `EXPORT_RENDER_DISTANCE − 1` chunks minus
/// half the longest camera path of the parked player, all delivered a second before the
/// export starts: at export render distance 32 every one of them is in range for the whole
/// shot, and Voxy draws the rest from its LODs. The recording itself is left as it is.
pub fn install(
    paths: &Paths,
    minecraft_dir: &Path,
    replay: &str,
    offset: Option<i64>,
) -> Result<()> {
    let w = WorldConfig::load(paths)?;
    let cfg: TrailerConfig = hhmc_core::config::load_toml(&paths.config().join("trailer.toml"))?;
    let src = paths.out().join("trailer").join("editor_state.json");
    let replays = minecraft_dir.join("flashback").join("replays");
    // Flashback shows the replay name (`2026-10-28T13:10:09`); the file has `_` for `:`.
    let file_stem = replay.replace(':', "_");
    let zip_path = replays.join(format!("{file_stem}.zip"));
    let mut archive = zip::ZipArchive::new(
        std::fs::File::open(&zip_path)
            .with_context(|| format!("opening {}", zip_path.display()))?,
    )?;
    let meta: Value = serde_json::from_reader(archive.by_name("metadata.json")?)?;
    let uuid = meta["uuid"].as_str().context("replay metadata has no uuid")?.to_string();
    let name = meta["name"].as_str().unwrap_or(replay).to_string();
    // The first action file holds the whole tour (a recording is split every 6000 ticks).
    let first = meta["chunks"]
        .as_object()
        .and_then(|c| c.keys().find(|k| k.as_str() == "c0.flashback"))
        .context("replay has no c0.flashback")?
        .clone();
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut archive.by_name(&first)?, &mut data)?;
    let stream = ActionFile::parse(&data)?;
    let shot_ticks = stream.shot_ticks(cfg.shot.len());
    let offset = match offset {
        Some(o) => o,
        None => shot_ticks[0].context(
            "no `Shot 1` chat message in the replay: start the tour during the recording, or \
             pass --offset",
        )?,
    };
    info!(offset, "ticks between the recording's start and the tour's");

    let state: Value = serde_json::from_str(&std::fs::read_to_string(&src)?)?;
    let scenes: Vec<Value> = state["scenes"]
        .as_array()
        .context("editor state has no scenes")?
        .iter()
        .map(|s| shift_scene(s, offset))
        .collect();
    ensure!(scenes.len() == cfg.shot.len(), "regenerate with `hhmc trailer` first");

    // Chunk positions of every cached chunk the stream sends (snapshot included).
    let positions = chunk_positions(&mut archive, &stream)?;
    let reach_m = (EXPORT_RENDER_DISTANCE - 1.0) * 16.0 - cfg.max_path_m / 2.0 - 16.0;
    let mut plans = Vec::new();
    for (i, s) in cfg.shot.iter().enumerate() {
        let shot_start = shot_ticks[i].with_context(|| {
            format!("no `Shot {}` chat message in the replay: record the whole tour", i + 1)
        })?;
        let window_end = shot_ticks.get(i + 1).copied().flatten().unwrap_or(stream.ticks);
        let export_start = scenes[i]["exportStartTicks"].as_i64().unwrap_or(0);
        let export_end = scenes[i]["exportEndTicks"].as_i64().unwrap_or(0);
        ensure!(
            export_end < window_end.min(stream.ticks),
            "scene {} ends at tick {export_end}, after its shot ({window_end}): wrong offset?",
            i + 1
        );
        let deliver = export_start - TICKS_PER_S as i64;
        ensure!(deliver > shot_start, "scene {} starts too early in its shot", i + 1);
        let mid = world(&w, halfway(&limit_path(&s.camera, cfg.max_path_m)));
        // Latest copy of every chunk within reach that the player had by the shot's end.
        let mut kept: HashMap<(i32, i32), u32> = HashMap::new();
        for &(tick, idx) in stream.chunk_sends().iter() {
            if tick >= window_end {
                break;
            }
            let (cx, cz) = positions[&idx];
            let d = ((cx * 16 + 8) as f64 - mid[0]).hypot((cz * 16 + 8) as f64 - mid[2]);
            if d <= reach_m {
                kept.insert((cx, cz), idx);
            }
        }
        let mut kept: Vec<u32> = kept.into_values().collect();
        kept.sort_unstable();
        plans.push(ShotReplay {
            n: i + 1,
            name: s.name.clone(),
            kept,
            deliver,
            end: window_end,
            scene: scenes[i].clone(),
        });
    }

    // Packets of every kept chunk, in one pass over the caches.
    let wanted: std::collections::HashSet<u32> =
        plans.iter().flat_map(|p| p.kept.iter().copied()).collect();
    let packets = chunk_packets(&mut archive, &wanted)?;
    let icon = archive.by_name("icon.png").ok().map(|mut f| {
        let mut b = Vec::new();
        let _ = std::io::Read::read_to_end(&mut f, &mut b);
        b
    });

    let states = minecraft_dir.join("flashback").join("editor_states");
    std::fs::create_dir_all(&states)?;
    for p in &plans {
        let new_uuid = derived_uuid(&uuid, p.n);
        let stem = format!("{file_stem}_shot_{:02}", p.n);
        let out_zip = replays.join(format!("{stem}.zip"));
        let actions = stream.rewrite(&p.kept, p.deliver, p.end)?;
        let mut cache = Vec::new();
        for idx in &p.kept {
            let pk = &packets[idx];
            cache.extend_from_slice(&(pk.len() as u32).to_be_bytes());
            cache.extend_from_slice(pk);
        }
        ensure!(p.kept.len() <= CHUNK_CACHE_SIZE, "shot {} keeps too many chunks", p.n);
        let mut m = meta.clone();
        m["uuid"] = json!(new_uuid);
        m["name"] = json!(format!("{name} shot {:02} {}", p.n, p.name));
        m["total_ticks"] = json!(p.end);
        m["chunks"] = json!({"c0.flashback": {"duration": p.end, "forcePlaySnapshot": false}});
        let tmp = out_zip.with_extension("zip.tmp");
        let mut zw = zip::ZipWriter::new(std::io::BufWriter::new(std::fs::File::create(&tmp)?));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("metadata.json", opts)?;
        std::io::Write::write_all(&mut zw, serde_json::to_string_pretty(&m)?.as_bytes())?;
        if let Some(icon) = &icon {
            zw.start_file("icon.png", opts)?;
            std::io::Write::write_all(&mut zw, icon)?;
        }
        zw.start_file("c0.flashback", opts)?;
        std::io::Write::write_all(&mut zw, &actions)?;
        zw.start_file("level_chunk_caches/0", opts)?;
        std::io::Write::write_all(&mut zw, &cache)?;
        std::io::Write::flush(&mut zw.finish()?)?;
        std::fs::rename(&tmp, &out_zip)?;

        let mut st = state.clone();
        st["scenes"] = json!([p.scene]);
        st["usedByPaths"] = json!([out_zip.to_string_lossy()]);
        let dst = states.join(format!("{new_uuid}.json"));
        std::fs::write(&dst, serde_json::to_string(&st)?)?;
        info!(
            replay = %m["name"].as_str().unwrap_or(""),
            chunks = p.kept.len(),
            file = %out_zip.display(),
            "shot replay written"
        );
    }
    Ok(())
}

struct ShotReplay {
    n: usize,
    name: String,
    /// Cache indices of the chunks the shot's replay holds.
    kept: Vec<u32>,
    /// Tick at which they all arrive.
    deliver: i64,
    /// Ticks in the shot's replay.
    end: i64,
    scene: Value,
}

/// A scene with its export range and keyframes moved by `offset` ticks.
fn shift_scene(scene: &Value, offset: i64) -> Value {
    let mut s = scene.clone();
    for k in ["exportStartTicks", "exportEndTicks"] {
        s[k] = json!(s[k].as_i64().unwrap_or(0) + offset);
    }
    for track in s["keyframeTracks"].as_array_mut().into_iter().flatten() {
        let by_tick = track["keyframesByTick"].as_object().cloned().unwrap_or_default();
        let shifted: serde_json::Map<String, Value> = by_tick
            .into_iter()
            .map(|(t, v)| ((t.parse::<i64>().unwrap_or(0) + offset).to_string(), v))
            .collect();
        track["keyframesByTick"] = Value::Object(shifted);
    }
    s
}

/// A stable UUID for shot `n` of a replay.
fn derived_uuid(uuid: &str, n: usize) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(format!("{uuid} hhmc shot {n}"));
    let mut b: [u8; 16] = h[..16].try_into().expect("16 bytes");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

/// One action of a Flashback action file.
struct Action<'a> {
    kind: u32,
    body: &'a [u8],
}

/// A Flashback action file (`cN.flashback`, Flashback 0.43.6): magic, varint count of
/// action names, the names as varint-length strings, an i32-length snapshot of actions,
/// then the actions: varint name index, i32 length, payload. `next_tick` advances the
/// tick; `level_chunk_cached` sends the chunk at a varint index into
/// `level_chunk_caches/`; chat arrives as `game_packet`.
struct ActionFile<'a> {
    names: Vec<String>,
    /// Bytes from the magic to the end of the names.
    header: &'a [u8],
    snapshot: Vec<Action<'a>>,
    /// Actions with the tick they run at.
    actions: Vec<(i64, Action<'a>)>,
    ticks: i64,
}

impl<'a> ActionFile<'a> {
    fn parse(d: &'a [u8]) -> Result<Self> {
        let mut r = Reader { d, p: 0 };
        ensure!(r.u32()? == FLASHBACK_MAGIC, "not a Flashback action file");
        let names: Vec<String> = (0..r.varint()?)
            .map(|_| {
                let n = r.varint()? as usize;
                Ok(String::from_utf8_lossy(r.take(n)?).into_owned())
            })
            .collect::<Result<_>>()?;
        let header = &d[..r.p];
        let snap_len = r.u32()? as usize;
        let mut s = Reader { d: r.take(snap_len)?, p: 0 };
        let mut snapshot = Vec::new();
        while s.p < s.d.len() {
            snapshot.push(s.action()?);
        }
        let next_tick = names.iter().position(|n| n.ends_with("/next_tick"));
        let (mut actions, mut tick) = (Vec::new(), 0i64);
        while r.p < d.len() {
            let a = r.action()?;
            if Some(a.kind as usize) == next_tick {
                tick += 1;
            }
            actions.push((tick, a));
        }
        Ok(Self { names, header, snapshot, actions, ticks: tick })
    }

    fn is(&self, a: &Action, suffix: &str) -> bool {
        self.names.get(a.kind as usize).is_some_and(|n| n.ends_with(suffix))
    }

    /// Tick of the tour's `Shot N: ` chat message, for N = 1..=n.
    fn shot_ticks(&self, n: usize) -> Vec<Option<i64>> {
        (1..=n)
            .map(|k| {
                let tag = format!("Shot {k}: ");
                self.actions
                    .iter()
                    .find(|(_, a)| {
                        self.is(a, "/game_packet")
                            && a.body.windows(tag.len()).any(|w| w == tag.as_bytes())
                    })
                    .map(|(t, _)| *t)
            })
            .collect()
    }

    /// (tick, cache index) of every chunk sent, the snapshot's at tick 0, in order.
    fn chunk_sends(&self) -> Vec<(i64, u32)> {
        let snap = self.snapshot.iter().map(|a| (0, a));
        let rest = self.actions.iter().map(|(t, a)| (*t, a));
        snap.chain(rest)
            .filter(|(_, a)| self.is(a, "/level_chunk_cached"))
            .filter_map(|(t, a)| Some((t, Reader { d: a.body, p: 0 }.varint().ok()?)))
            .collect()
    }

    /// The file with no chunks but `kept` (re-indexed 0, 1, … in that order), all sent at
    /// tick `deliver`, cut after `end` ticks.
    fn rewrite(&self, kept: &[u32], deliver: i64, end: i64) -> Result<Vec<u8>> {
        let chunk = self
            .names
            .iter()
            .position(|n| n.ends_with("/level_chunk_cached"))
            .context("no level_chunk_cached action")? as u32;
        let mut out = self.header.to_vec();
        let mut snap = Vec::new();
        for a in &self.snapshot {
            if a.kind != chunk {
                write_action(&mut snap, a.kind, a.body);
            }
        }
        out.extend_from_slice(&(snap.len() as u32).to_be_bytes());
        out.extend_from_slice(&snap);
        let mut sent = false;
        let send = |out: &mut Vec<u8>| {
            for i in 0..kept.len() {
                let mut body = Vec::new();
                write_varint(&mut body, i as u32);
                write_action(out, chunk, &body);
            }
        };
        for (t, a) in &self.actions {
            // Actions of tick `t` come before the `next_tick` that ends it (which is
            // counted as tick `t`).
            if !sent && *t > deliver {
                send(&mut out);
                sent = true;
            }
            if *t > end {
                break;
            }
            if a.kind != chunk {
                write_action(&mut out, a.kind, a.body);
            }
        }
        ensure!(sent, "the recording ends before the shot");
        Ok(out)
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        if v < 0x80 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
}

fn write_action(out: &mut Vec<u8>, kind: u32, body: &[u8]) {
    write_varint(out, kind);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
}

/// Chunk (x, z) of every cache index the stream sends. A cache entry is an i32 length and
/// a `ClientboundLevelChunkWithLightPacket`: packet id (one byte), x, z (i32 each), ….
fn chunk_positions<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    stream: &ActionFile,
) -> Result<HashMap<u32, (i32, i32)>> {
    let wanted: std::collections::HashSet<u32> =
        stream.chunk_sends().into_iter().map(|(_, i)| i).collect();
    let mut out = HashMap::new();
    for_each_cached(archive, &wanted, |idx, pk| {
        ensure!(pk.len() >= 9, "short chunk packet {idx}");
        let x = i32::from_be_bytes(pk[1..5].try_into()?);
        let z = i32::from_be_bytes(pk[5..9].try_into()?);
        out.insert(idx, (x, z));
        Ok(())
    })?;
    ensure!(out.len() == wanted.len(), "chunk cache misses {} chunks", wanted.len() - out.len());
    Ok(out)
}

fn chunk_packets<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    wanted: &std::collections::HashSet<u32>,
) -> Result<HashMap<u32, Vec<u8>>> {
    let mut out = HashMap::new();
    for_each_cached(archive, wanted, |idx, pk| {
        out.insert(idx, pk.to_vec());
        Ok(())
    })?;
    Ok(out)
}

/// Calls `f` with every wanted cache entry, reading only the cache files that hold them.
fn for_each_cached<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    wanted: &std::collections::HashSet<u32>,
    mut f: impl FnMut(u32, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut files: Vec<u32> = wanted.iter().map(|i| i / CHUNK_CACHE_SIZE as u32).collect();
    files.sort_unstable();
    files.dedup();
    let mut buf = Vec::new();
    for file in files {
        let mut r =
            std::io::BufReader::new(archive.by_name(&format!("level_chunk_caches/{file}"))?);
        for k in 0..CHUNK_CACHE_SIZE as u32 {
            let mut len = [0u8; 4];
            if std::io::Read::read_exact(&mut r, &mut len).is_err() {
                break;
            }
            buf.resize(u32::from_be_bytes(len) as usize, 0);
            std::io::Read::read_exact(&mut r, &mut buf)?;
            let idx = file * CHUNK_CACHE_SIZE as u32 + k;
            if wanted.contains(&idx) {
                f(idx, &buf)?;
            }
        }
    }
    Ok(())
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.d.get(self.p..self.p + n).context("truncated Flashback file")?;
        self.p += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn varint(&mut self) -> Result<u32> {
        let mut v = 0u32;
        for shift in (0..35).step_by(7) {
            let b = self.take(1)?[0];
            v |= u32::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        anyhow::bail!("bad varint")
    }
    fn action(&mut self) -> Result<Action<'a>> {
        let kind = self.varint()?;
        let len = self.u32()? as usize;
        Ok(Action { kind, body: self.take(len)? })
    }
}
