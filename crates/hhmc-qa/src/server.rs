//! Headless vanilla dedicated server for tests: start on a world, talk RCON, scan the log.
//!
//! server.properties keys and the EULA mechanism were verified against the 26.3 jar
//! (`DedicatedServerProperties`, `Eula`) and <https://minecraft.wiki/w/Server.properties>.
//! The server only starts when the user accepts the Minecraft EULA
//! (<https://aka.ms/MinecraftEULA>) by setting `HHMC_ACCEPT_MINECRAFT_EULA=true`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use tracing::{debug, info, warn};

use hhmc_world::layout::copy_dir_filtered;

use crate::rcon::Rcon;

/// Environment variable with which the user accepts the Minecraft EULA for the test server.
pub const EULA_ENV: &str = "HHMC_ACCEPT_MINECRAFT_EULA";

/// Flat generator settings for an empty void world: one air layer, the void biome, no
/// features (vanilla `flat_level_generator_preset/the_void.json` with `features: false`,
/// which avoids the start platform).
pub const VOID_GENERATOR: &str = r#"{"layers":[{"block":"minecraft:air","height":1}],"biome":"minecraft:the_void","features":false,"lakes":false,"structure_overrides":[]}"#;

#[derive(Clone, Debug)]
pub struct ServerSetup {
    pub jar: PathBuf,
    /// Directory holding server.properties, eula.txt, logs and libraries.
    pub work_dir: PathBuf,
    pub java: String,
    pub java_major: u32,
    pub max_heap: String,
}

pub struct Running {
    child: Child,
    pub rcon: Rcon,
    lines: mpsc::Receiver<String>,
    log: Vec<String>,
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn java_major(java: &str) -> Result<u32> {
    let out =
        Command::new(java).arg("-version").output().with_context(|| format!("running {java}"))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let v = text
        .split('"')
        .nth(1)
        .with_context(|| format!("cannot parse `{java} -version`: {text}"))?;
    let major = v.split('.').next().unwrap_or("0");
    Ok(major.parse()?)
}

impl ServerSetup {
    /// Starts the server on `<universe>/<world>`; extra server.properties may be given.
    pub fn start(&self, universe: &Path, world: &str, extra: &[(&str, &str)]) -> Result<Running> {
        let have = java_major(&self.java)?;
        if have < self.java_major {
            bail!("the server needs Java {} but `{}` is Java {have}", self.java_major, self.java);
        }
        if std::env::var(EULA_ENV).as_deref() != Ok("true") {
            bail!(
                "the test server needs the Minecraft EULA (https://aka.ms/MinecraftEULA): \
                 read it, and set {EULA_ENV}=true to accept it"
            );
        }
        fs::create_dir_all(&self.work_dir)?;
        fs::write(
            self.work_dir.join("eula.txt"),
            format!(
                "# Accepted by the user via {EULA_ENV}=true.\n\
                 # https://aka.ms/MinecraftEULA\neula=true\n"
            ),
        )?;
        let rcon_port = free_port()?;
        let seed = format!("{:?}{}", SystemTime::now(), std::process::id());
        let password = hex::encode(&Sha1::digest(seed.as_bytes())[..12]);
        let mut props = vec![
            ("server-port".to_owned(), free_port()?.to_string()),
            ("server-ip".to_owned(), "127.0.0.1".to_owned()),
            ("enable-rcon".to_owned(), "true".to_owned()),
            ("rcon.port".to_owned(), rcon_port.to_string()),
            ("rcon.password".to_owned(), password.clone()),
            ("online-mode".to_owned(), "false".to_owned()),
            ("white-list".to_owned(), "true".to_owned()),
            ("enforce-whitelist".to_owned(), "true".to_owned()),
            ("view-distance".to_owned(), "2".to_owned()),
            ("simulation-distance".to_owned(), "2".to_owned()),
            ("spawn-protection".to_owned(), "0".to_owned()),
            // The server pauses ticking (and chunk loading) after this long without players;
            // a non-positive value disables that (https://minecraft.wiki/w/Server.properties).
            ("pause-when-empty-seconds".to_owned(), "-1".to_owned()),
            ("motd".to_owned(), "hhmc test server".to_owned()),
        ];
        for (k, v) in extra {
            props.retain(|(pk, _)| pk != k);
            props.push(((*k).to_owned(), (*v).to_owned()));
        }
        let text: String = props.iter().map(|(k, v)| format!("{k}={}\n", escape_prop(v))).collect();
        fs::write(self.work_dir.join("server.properties"), text)?;

        info!(world = %universe.join(world).display(), "starting vanilla server");
        let mut child = Command::new(&self.java)
            .arg(format!("-Xmx{}", self.max_heap))
            .arg("-jar")
            .arg(fs::canonicalize(&self.jar)?)
            .arg("--nogui")
            .arg("--universe")
            .arg(fs::canonicalize(universe)?)
            .arg("--world")
            .arg(world)
            .current_dir(&self.work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting java")?;
        let (tx, rx) = mpsc::channel();
        for pipe in [
            Box::new(child.stdout.take().expect("piped")) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().expect("piped")),
        ] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        let mut log = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(line) => {
                    debug!(target: "mc", "{line}");
                    let done = line.contains("]: Done (");
                    log.push(line);
                    if done {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.wait();
                    bail!("server exited during startup:\n{}", tail(&log, 30));
                }
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("server did not finish starting in time:\n{}", tail(&log, 30));
            }
        }
        // RCON starts listening just after "Done"; retry briefly.
        let mut attempt = 0;
        let rcon = loop {
            match Rcon::connect(rcon_port, &password) {
                Ok(r) => break r,
                Err(e) if attempt < 60 => {
                    debug!(error = %e, "RCON not ready yet");
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => {
                    let _ = child.kill();
                    return Err(e.context(format!("connecting RCON:\n{}", tail(&log, 30))));
                }
            }
        };
        Ok(Running { child, rcon, lines: rx, log })
    }
}

/// Escapes a value for a Java properties file (backslashes only; `:`/`=` are fine in values).
fn escape_prop(v: &str) -> String {
    v.replace('\\', "\\\\")
}

fn tail(log: &[String], n: usize) -> String {
    log[log.len().saturating_sub(n)..].join("\n")
}

impl Running {
    fn drain(&mut self) {
        while let Ok(l) = self.lines.try_recv() {
            debug!(target: "mc", "{l}");
            self.log.push(l);
        }
    }

    pub fn command(&mut self, cmd: &str) -> Result<String> {
        let r = self.rcon.command(cmd);
        self.drain();
        r
    }

    /// Kills the server without saving and returns the log so far. For runs on a
    /// throwaway world copy: a clean `stop` re-saves every loaded (relit) chunk, which
    /// takes minutes on a large world.
    pub fn abort(mut self) -> Result<Vec<String>> {
        // Let the log catch up with the last commands before killing.
        std::thread::sleep(Duration::from_millis(500));
        self.drain();
        self.child.kill()?;
        self.child.wait()?;
        while let Ok(l) = self.lines.recv_timeout(Duration::from_millis(200)) {
            self.log.push(l);
        }
        Ok(std::mem::take(&mut self.log))
    }

    /// Stops the server (saving the world) and returns the full log.
    pub fn stop(mut self) -> Result<Vec<String>> {
        let _ = self.rcon.command("stop");
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            self.drain();
            if let Some(status) = self.child.try_wait()? {
                debug!(?status, "server exited");
                break;
            }
            if Instant::now() > deadline {
                warn!("server did not stop in time; killing it");
                self.child.kill()?;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        while let Ok(l) = self.lines.recv_timeout(Duration::from_millis(200)) {
            self.log.push(l);
        }
        Ok(std::mem::take(&mut self.log))
    }
}

impl Drop for Running {
    /// Never leave a server behind: kill it if `stop` was not called or did not finish.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Log lines that indicate a problem with our world data: every ERROR, and WARN lines
/// mentioning chunks, regions or NBT.
pub fn problem_lines(log: &[String]) -> Vec<String> {
    log.iter()
        .filter(|l| {
            let lower = l.to_lowercase();
            l.contains("/ERROR]")
                || l.contains("/FATAL]")
                || (l.contains("/WARN]")
                    && ["chunk", "region", "nbt", "couldn't", "failed", "invalid", "missing"]
                        .iter()
                        .any(|k| lower.contains(k)))
        })
        .cloned()
        .collect()
}

/// Creates the template world (void flat generator) once; returns its directory. Region,
/// entity and POI files are dropped, so only `level.dat` and saved data remain.
pub fn create_template(setup: &ServerSetup, out_dir: &Path) -> Result<PathBuf> {
    let universe = setup.work_dir.join("template-universe");
    let _ = fs::remove_dir_all(&universe);
    fs::create_dir_all(&universe)?;
    let srv = setup.start(
        &universe,
        "template",
        &[
            ("level-type", "minecraft:flat"),
            ("generator-settings", VOID_GENERATOR),
            ("generate-structures", "false"),
        ],
    )?;
    let log = srv.stop()?;
    let problems = problem_lines(&log);
    if !problems.is_empty() {
        warn!(?problems, "template server reported problems");
    }
    let _ = fs::remove_dir_all(out_dir);
    copy_dir_filtered(&universe.join("template"), out_dir, &|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        !matches!(name, "region" | "entities" | "poi" | "session.lock")
    })?;
    fs::remove_dir_all(&universe)?;
    Ok(out_dir.to_path_buf())
}
