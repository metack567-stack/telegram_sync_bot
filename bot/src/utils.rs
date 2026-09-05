use std::path::Path;

use anyhow::{Result, anyhow};
use rand::{Rng as _, distr::Alphanumeric};
use tokio::process;
use tracing::{info, warn};

pub async fn cp_from_container(
    container_manager: impl AsRef<str>,
    container_id: impl AsRef<str>,
    from: impl AsRef<Path>,
    to: impl AsRef<Path>,
) -> Result<()> {
    info!(">> IO: moving from container manager");
    let container_manager = container_manager.as_ref();
    let container_id = container_id.as_ref();
    let from = from.as_ref();
    let to = to.as_ref();
    // docker cp overwrites the target by default, podman needs --overwrite.
    // Try plain first, then retry with the flag in case the target already
    // exists under podman.
    let run = |overwrite: bool| async move {
        let mut cmd = process::Command::new(container_manager);
        cmd.arg("cp");
        if overwrite {
            cmd.arg("--overwrite");
        }
        cmd.arg(format!("{}:{}", container_id, from.display()));
        cmd.arg(to);
        cmd.status().await
    };
    if run(false).await?.success() {
        return Ok(());
    }
    if run(true).await?.success() {
        return Ok(());
    }
    Err(anyhow!("Failed to copy from local server container"))
}

const BYPASS_KEY_FILE: &str = "bypass.key";

/// load the bypass key: env BYPASSKEY wins, then the persisted key file,
/// otherwise generate a new one and persist it so restarts keep the same key
pub fn load_or_create_key(data_dir: &Path) -> String {
    let key_path = data_dir.join(BYPASS_KEY_FILE);
    if let Ok(key) = std::env::var("BYPASSKEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return key;
        }
    }
    if let Ok(key) = std::fs::read_to_string(&key_path) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return key;
        }
    }
    let key = gen_key();
    if let Err(e) = std::fs::write(&key_path, &key) {
        warn!(">> INIT: failed to persist bypass key: {}", e);
    }
    key
}

/// persist the bypass key after it is renewed
pub fn save_key(data_dir: &Path, key: &str) {
    let key_path = data_dir.join(BYPASS_KEY_FILE);
    if let Err(e) = std::fs::write(&key_path, key) {
        warn!(">> INIT: failed to persist bypass key: {}", e);
    }
}

/// available bytes on the filesystem containing `path`
pub fn available_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| anyhow!("invalid path: {}", e))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut s) } != 0 {
        return Err(anyhow!("statvfs failed for {}", path.display()));
    }
    Ok(s.f_bsize as u64 * s.f_bavail as u64)
}

pub fn gen_key() -> String {
    #[cfg(debug_assertions)]
    const KEY_LEN: usize = 1;
    #[cfg(not(debug_assertions))]
    const KEY_LEN: usize = 16;
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(KEY_LEN)
        .map(char::from)
        .collect()
}
