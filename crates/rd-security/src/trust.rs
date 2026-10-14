//! 信任列表：对端证书指纹的增删查（05_api §1「应用层校验对端证书 SHA-256 指纹」，T-013）。
//!
//! 持久化为 `trusted.json`（裸 JSON 数组 `[{fingerprint_hex, name, added_at, last_seen}]`）。
//! 写入先写同目录临时文件（fsync）再 rename，保证崩溃/断电时不产生半截文件。
//! 时间用 Unix 秒（`added_at`/`last_seen`）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rd_transport::Fingerprint;
use serde::{Deserialize, Serialize};

/// 一台受信任的对端设备。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    /// SHA-256(证书 DER) 的 64 字符小写 hex。
    pub fingerprint_hex: String,
    /// 设备显示名（配对时对端 Hello 里的 device_name / 控制端名）。
    pub name: String,
    /// 加入时间（Unix 秒）。
    pub added_at: u64,
    /// 最近一次成功连接时间（Unix 秒）。
    pub last_seen: u64,
}

/// 指纹信任列表。内存中持有全量条目，每次修改后原子落盘。
pub struct TrustStore {
    path: PathBuf,
    peers: Vec<TrustedPeer>,
}

impl TrustStore {
    /// 打开（或新建）信任列表。文件不存在视为空列表；存在但解析失败则报错。
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let peers = if path.exists() {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("读取信任列表失败：{}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("解析信任列表失败：{}", path.display()))?
        } else {
            Vec::new()
        };
        Ok(Self {
            path: path.to_path_buf(),
            peers,
        })
    }

    /// 指纹是否受信任。
    pub fn is_trusted(&self, fp: &Fingerprint) -> bool {
        self.by_hex(&fp.hex()).is_some()
    }

    /// 加入信任列表；已存在时仅更新显示名（首次加入时间保留）。
    pub fn add(&mut self, fp: Fingerprint, name: &str) -> anyhow::Result<()> {
        let hex = fp.hex();
        let now = unix_now();
        if let Some(peer) = self.by_hex_mut(&hex) {
            peer.name = name.to_string();
        } else {
            self.peers.push(TrustedPeer {
                fingerprint_hex: hex,
                name: name.to_string(),
                added_at: now,
                last_seen: now,
            });
        }
        self.save()
    }

    /// 移除信任；不存在时静默成功。
    pub fn remove(&mut self, fp: &Fingerprint) -> anyhow::Result<()> {
        let hex = fp.hex();
        self.peers.retain(|peer| peer.fingerprint_hex != hex);
        self.save()
    }

    /// 全部条目快照。
    pub fn list(&self) -> Vec<TrustedPeer> {
        self.peers.clone()
    }

    /// 刷新最近连接时间；不存在时静默成功。
    pub fn touch(&mut self, fp: &Fingerprint) -> anyhow::Result<()> {
        let hex = fp.hex();
        if let Some(peer) = self.by_hex_mut(&hex) {
            peer.last_seen = unix_now();
            self.save()?;
        }
        Ok(())
    }

    /// 临时文件 + rename 原子写入（fsync 后替换）。
    fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&self.peers).context("序列化信任列表失败")?;
        // 必须持有写入句柄再 fsync：Windows 上 FlushFileBuffers 对只读句柄返回拒绝访问
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("写入临时文件失败：{}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .with_context(|| format!("写入临时文件失败：{}", tmp.display()))?;
        file.sync_all().context("fsync 临时文件失败")?;
        drop(file);
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("原子替换信任列表失败：{}", self.path.display()))
    }

    fn by_hex(&self, hex: &str) -> Option<&TrustedPeer> {
        self.peers.iter().find(|peer| peer.fingerprint_hex == hex)
    }

    fn by_hex_mut(&mut self, hex: &str) -> Option<&mut TrustedPeer> {
        self.peers
            .iter_mut()
            .find(|peer| peer.fingerprint_hex == hex)
    }
}

/// 当前 Unix 秒；系统时钟早于 epoch 时按 0 处理（只影响时间戳，不影响安全性）。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_now_is_sane() {
        // 2024-01-01 及以后
        assert!(unix_now() > 1_704_067_200);
    }
}
