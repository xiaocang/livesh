use std::{
    fs::File,
    io::Write,
    os::{fd::FromRawFd, unix::io::RawFd},
};

use livesh_protocol::ShellId;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct StateJson<'a> {
    schema: u16,
    id: &'a ShellId,
    name: &'a str,
    status: &'a str,
    restore: &'a [String],
}

pub fn write(fd: RawFd, id: &ShellId, name: &str, restore_argv: &[String]) -> anyhow::Result<()> {
    let value = StateJson {
        schema: 1,
        id,
        name,
        status: "running",
        restore: restore_argv,
    };
    let bytes = serde_json::to_vec_pretty(&value)?;

    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}
