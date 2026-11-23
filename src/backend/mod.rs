use anyhow::{bail, Result};
use std::path::PathBuf;

use crate::config::Instance;
use crate::transport::Ssh;
use crate::util;

pub mod daytona;
pub mod instabox;
pub mod sshhost;

pub struct Status {
    /// Normalised lifecycle word: running | stopped | suspended | missing | unreachable | ...
    pub state: String,
    /// One line of specs / details.
    pub detail: String,
}

pub trait Backend {
    /// Make the machine reachable (create / start / resume as needed) and return an ssh handle.
    fn ensure(&self) -> Result<Ssh>;
    /// Cheap state query that never creates or starts anything.
    fn status(&self) -> Result<Status>;
    /// Pause / stop to save money. Idempotent.
    fn stop(&self) -> Result<()>;
    /// Permanently delete the machine and forget local caches. Returns a human message.
    fn destroy(&self) -> Result<String>;
}

pub fn ssh_cache_dir() -> PathBuf {
    util::cache_dir().join("ssh")
}

pub fn control_dir() -> PathBuf {
    util::cache_dir().join("cm")
}

pub fn make(name: &str, inst: &Instance) -> Result<Box<dyn Backend>> {
    Ok(match inst.backend.as_str() {
        "instabox" => Box::new(instabox::Instabox::new(name, inst)),
        "daytona" => Box::new(daytona::Daytona::new(name, inst)),
        "ssh" => Box::new(sshhost::SshHost::new(name, inst)?),
        other => bail!("unknown backend `{other}`"),
    })
}
