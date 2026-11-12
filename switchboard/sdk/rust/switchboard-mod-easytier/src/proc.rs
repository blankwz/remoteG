//! easytier-core 进程托管：生成配置文件 → spawn → 等 rpc-portal 就绪 →
//! Drop 终止。失败语义：spawn 或就绪超时即报错并清理，不留孤儿进程。
//!
//! 不用 `taskkill`（用户全局禁令 + 按句柄终止才不误伤同映像其它进程）；
//! Windows 下 `CREATE_NO_WINDOW` 防托盘应用弹控制台。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;

/// rpc-portal 就绪等待上限（easytier-core 冷启动含 crypto 初始化，留足）。
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// 就绪轮询间隔。
const READY_POLL: Duration = Duration::from_millis(200);

/// 一个在跑的 easytier-core 节点。Drop 即终止进程（句柄式 kill）。
#[derive(Debug)]
pub struct EasyTierNode {
    child: Option<Child>,
    rpc_portal: SocketAddr,
    config_path: PathBuf,
    /// Windows：Job Object 句柄（保持打开 = kill-on-close 生效窗口）。
    /// 宿主进程崩溃/`process::exit(0)` 时句柄由内核关闭 → 子进程被收割，
    /// 不依赖 Drop 执行（托盘退出路径不走 Drop）。
    #[cfg(windows)]
    job_handle: Option<windows::Win32::Foundation::HANDLE>,
}

impl EasyTierNode {
    /// 写配置 TOML 到 `work_dir/easytier-<network_name>.toml` 并启动节点，
    /// 阻塞至 rpc-portal 可连接（`READY_TIMEOUT` 内）。提前退出视为失败，
    /// 带上 stderr 摘要报错。
    pub fn launch(
        binary: &Path,
        cfg: &super::config::EasyTierNetConfig,
        work_dir: &Path,
    ) -> anyhow::Result<Self> {
        if cfg.network_name.trim().is_empty() {
            anyhow::bail!("EasyTier network_name 不能为空");
        }
        std::fs::create_dir_all(work_dir)
            .with_context(|| format!("创建目录失败: {}", work_dir.display()))?;
        let config_path = work_dir.join(format!(
            "easytier-{}.toml",
            sanitize_name(&cfg.network_name)
        ));
        std::fs::write(&config_path, cfg.to_toml())
            .with_context(|| format!("写 EasyTier 配置失败: {}", config_path.display()))?;

        let mut cmd = Command::new(binary);
        cmd.arg("--config-file")
            .arg(&config_path)
            .args(cfg.core_args())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW = 0x0800_0000：后台进程不闪控制台
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 easytier-core 失败: {}", binary.display()))?;

        // Windows 孤儿防护（best-effort）：进 Job，KILL_ON_JOB_CLOSE。
        // 句柄存入 Self——宿主进程死亡内核关句柄触发收割；正常 Drop 先 kill 子进程再关句柄。
        #[cfg(windows)]
        let job_handle = match assign_kill_on_close_job(&child) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!(err = %e, "EasyTier 子进程未能加入 Job（孤儿防护降级）");
                None
            }
        };

        // 就绪等待 + 提前退出检测。stderr 先 hold（失败时取摘要）；
        // 成功路径直接丢弃管道避免缓冲满阻塞子进程。
        let portal = cfg.rpc_portal;
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut stderr_hold = child.stderr.take();
        loop {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let msg = stderr_hold
                    .take()
                    .and_then(|mut s| read_stderr_tail(&mut s));
                anyhow::bail!(
                    "easytier-core rpc-portal {portal} 就绪超时（{}s）：{}",
                    READY_TIMEOUT.as_secs(),
                    msg.unwrap_or_else(|| "无 stderr 输出".into())
                );
            }
            // 子进程提前退出 = 配置/网络错误，尽快失败
            if let Some(status) = child.try_wait()? {
                let msg = stderr_hold
                    .take()
                    .and_then(|mut s| read_stderr_tail(&mut s));
                anyhow::bail!(
                    "easytier-core 提前退出（{status}）：{}",
                    msg.unwrap_or_else(|| "无 stderr 输出".into())
                );
            }
            if std::net::TcpStream::connect(portal).is_ok() {
                break;
            }
            std::thread::sleep(READY_POLL);
        }
        drop(stderr_hold);

        Ok(Self {
            child: Some(child),
            rpc_portal: portal,
            config_path,
            #[cfg(windows)]
            job_handle,
        })
    }

    /// 本节点 rpc-portal（easytier-cli / [`super::discover::EasyTierDiscoverer`] 查询入口）。
    pub fn rpc_portal(&self) -> SocketAddr {
        self.rpc_portal
    }

    /// 配置文件路径（诊断用）。
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// 进程是否仍在运行（无阻塞等待）。
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => c.try_wait().map(|s| s.is_none()).unwrap_or(false),
            None => false,
        }
    }

    /// 终止进程（幂等）。
    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // 显式放掉 Job 句柄（子进程已死，kill-on-close 只剩句柄释放）
        #[cfg(windows)]
        {
            self.job_handle.take();
        }
    }
}

impl Drop for EasyTierNode {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 网络名 → 文件名安全段（文件系统非法字符替换）。
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 读 stderr 末尾若干行（失败诊断；不完整读取不算错）。
fn read_stderr_tail(stderr: &mut impl std::io::Read) -> Option<String> {
    let mut buf = Vec::new();
    let _ = stderr.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let tail: Vec<&str> = text.lines().rev().take(5).collect();
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join(" | ");
    (!tail.is_empty()).then_some(tail)
}

/// Windows：把子进程加入 KILL_ON_JOB_CLOSE 的 Job（孤儿防护）。
/// 返回的 HANDLE 必须随 [`EasyTierNode`] 保存——宿主进程死亡时内核关闭
/// 该句柄即触发收割；正常 Drop 先 kill 子进程再放掉句柄。
#[cfg(windows)]
fn assign_kill_on_close_job(child: &Child) -> anyhow::Result<windows::Win32::Foundation::HANDLE> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(None, None)?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;
        let child_handle = HANDLE(child.as_raw_handle());
        AssignProcessToJobObject(job, child_handle)?;
        Ok(job)
    }
}

// ── 单元测试（不 spawn 真实进程；launch 的 e2e 见 lib.rs ignore 测试） ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_替换非法字符() {
        assert_eq!(sanitize_name("rd-mesh_1"), "rd-mesh_1");
        assert_eq!(sanitize_name("我的网络/2026"), "_____2026");
        assert_eq!(sanitize_name("a b.c"), "a_b_c");
    }

    #[test]
    fn launch_空网络名报错() {
        let cfg = super::super::config::EasyTierNetConfig::default();
        let err = EasyTierNode::launch(Path::new("nonexistent"), &cfg, Path::new("."))
            .expect_err("空网络名必须拒绝");
        assert!(err.to_string().contains("network_name"));
    }

    #[test]
    fn launch_二进制不存在_报错且不产孤儿() {
        let cfg = super::super::config::EasyTierNetConfig {
            network_name: "t".into(),
            rpc_portal: "127.0.0.1:1".parse().unwrap(), // 不可达端口
            ..Default::default()
        };
        let work = std::env::temp_dir().join(format!("sb-et-test-{}", std::process::id()));
        let err = EasyTierNode::launch(Path::new("easytier-core-绝不存在的路径.exe"), &cfg, &work)
            .expect_err("不存在的二进制必须报错");
        assert!(err.to_string().contains("启动 easytier-core 失败"));
        let _ = std::fs::remove_dir_all(&work);
    }
}
