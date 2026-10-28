//! Minimal Source RCON client as implemented by the Minecraft server: little-endian
//! packets `len:i32, id:i32, type:i32, body\0\0`; type 3 = login, 2 = command,
//! 0 = response. <https://minecraft.wiki/w/RCON>

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Result, bail, ensure};

pub struct Rcon {
    stream: TcpStream,
    next_id: i32,
}

const LOGIN: i32 = 3;
const COMMAND: i32 = 2;

impl Rcon {
    pub fn connect(port: u16, password: &str) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        let mut r = Self { stream, next_id: 1 };
        let id = r.send(LOGIN, password)?;
        let (rid, _) = r.recv()?;
        if rid == -1 || rid != id {
            bail!("RCON authentication failed");
        }
        Ok(r)
    }

    fn send(&mut self, kind: i32, body: &str) -> Result<i32> {
        let id = self.next_id;
        self.next_id += 1;
        let len = 4 + 4 + body.len() + 2;
        let mut pkt = Vec::with_capacity(len + 4);
        pkt.extend_from_slice(&(len as i32).to_le_bytes());
        pkt.extend_from_slice(&id.to_le_bytes());
        pkt.extend_from_slice(&kind.to_le_bytes());
        pkt.extend_from_slice(body.as_bytes());
        pkt.extend_from_slice(&[0, 0]);
        self.stream.write_all(&pkt)?;
        Ok(id)
    }

    fn recv(&mut self) -> Result<(i32, String)> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let len = i32::from_le_bytes(len);
        ensure!((10..=1 << 20).contains(&len), "bad RCON packet length {len}");
        let mut buf = vec![0u8; len as usize];
        self.stream.read_exact(&mut buf)?;
        let id = i32::from_le_bytes(buf[0..4].try_into()?);
        let body = String::from_utf8_lossy(&buf[8..buf.len() - 2]).into_owned();
        Ok((id, body))
    }

    /// Runs a command (without leading `/`) and returns the server's reply.
    pub fn command(&mut self, cmd: &str) -> Result<String> {
        let id = self.send(COMMAND, cmd)?;
        let (rid, body) = self.recv()?;
        ensure!(rid == id, "RCON reply id {rid} does not match request {id}");
        Ok(body)
    }
}
