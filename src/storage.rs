use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::paper::Stored;

pub fn lock(path: &Path) -> Result<File> {
    let parent = path.parent().context("状态路径缺少父目录")?;
    fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    file.try_lock_exclusive()
        .context("该交易对已有运行中的模拟盘进程")?;
    Ok(file)
}

pub fn load(path: &Path) -> Result<Option<Stored>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("读取状态失败: {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("状态文件损坏，拒绝重置: {}", path.display()))
        .map(Some)
}

pub fn save(path: &Path, state: &Stored) -> Result<()> {
    let parent = path.parent().context("状态路径缺少父目录")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, state)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn persists_state_and_rejects_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ETHUSDC.json");
        let state = Stored::initial("ETHUSDC".to_string(), Utc::now());
        save(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().config.symbol, "ETHUSDC");
        fs::write(&path, b"not json").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn prevents_two_writers_for_one_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ETHUSDC.json");
        let first = lock(&path).unwrap();
        assert!(lock(&path).is_err());
        drop(first);
        assert!(lock(&path).is_ok());
    }
}
