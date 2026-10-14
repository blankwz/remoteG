//! 连接管理：全局 [`ConnManager`]（tauri::State），M0 临时身份（05_api §1）。
//!
//! 指纹持久化与信任列表由 T-013 负责；本模块只维护内存态。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rd_transport::{Conn, Identity};

/// 全局连接管理：device_id → 已认证 QUIC 连接。
#[derive(Debug)]
pub struct ConnManager {
    identity: Arc<Identity>,
    conns: Mutex<HashMap<String, Arc<Conn>>>,
}

impl ConnManager {
    /// M0：identity 每次启动临时生成（不自持、不与历史指纹比对）。
    pub fn new() -> anyhow::Result<Self> {
        let identity = Identity::generate("rd-controller")?;
        Ok(Self {
            identity: Arc::new(identity),
            conns: Mutex::new(HashMap::new()),
        })
    }

    /// 本机临时身份（spike 握手用）。
    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    /// 登记连接，返回被替换的旧连接（调用方负责关闭）。
    pub fn insert(&self, device_id: String, conn: Arc<Conn>) -> Option<Arc<Conn>> {
        self.conns
            .lock()
            .expect("conn map must not be poisoned")
            .insert(device_id, conn)
    }
}
