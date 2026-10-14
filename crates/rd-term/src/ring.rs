//! 环形缓冲：固定容量回放缓冲，断线重连后回放（03_architecture/架构方案.md §3.5，T-014）。
//!
//! 溢出时覆盖最旧数据；截断发生后立即把保留起点对齐到安全边界
//! （`\n` 行首或 ESC 序列起始），保证 `snapshot` 永不从半截转义序列开始回放。

/// 截断边界回扫的最大字节数；真实 VT 流中换行密集，很少触达该上限。
const SAFE_SCAN_LIMIT: usize = 4096;

/// 固定容量 FIFO 字节环，默认 1 MiB。
pub struct RingBuffer {
    buf: Vec<u8>,
    start: usize,
    len: usize,
}

impl RingBuffer {
    /// 默认回放缓冲容量：1 MiB。
    pub const CAPACITY: usize = 1024 * 1024;

    /// 以默认容量（1 MiB）创建。
    pub fn new() -> Self {
        Self::with_capacity(Self::CAPACITY)
    }

    /// 以指定容量创建（测试用小容量验证溢出截断）。
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity],
            start: 0,
            len: 0,
        }
    }

    /// 总容量（字节）。
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// 当前有效字节数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 追加数据；容量不足时覆盖最旧字节，并把保留起点对齐到安全边界。
    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() || self.buf.is_empty() {
            return;
        }
        let cap = self.buf.len();
        if data.len() >= cap {
            // 单次写入不小于容量：只保留最后 cap 字节。
            self.buf.copy_from_slice(&data[data.len() - cap..]);
            self.start = 0;
            self.len = cap;
            self.align_start();
            return;
        }
        let mut truncated = false;
        if self.len + data.len() > cap {
            let drop_n = self.len + data.len() - cap;
            self.start = (self.start + drop_n) % cap;
            self.len -= drop_n;
            truncated = true;
        }
        let write_at = (self.start + self.len) % cap;
        let head = usize::min(data.len(), cap - write_at);
        self.buf[write_at..write_at + head].copy_from_slice(&data[..head]);
        if head < data.len() {
            self.buf[..data.len() - head].copy_from_slice(&data[head..]);
        }
        self.len += data.len();
        if truncated {
            // 发生过覆盖丢弃：新旧数据拼齐后再对齐，安全边界可能落在新数据里。
            self.align_start();
        }
    }

    /// 线性化快照（起点已保证在安全边界上）。
    pub fn snapshot(&self) -> Vec<u8> {
        if self.len == 0 {
            return Vec::new();
        }
        let cap = self.buf.len();
        let mut out = vec![0u8; self.len];
        let head = usize::min(self.len, cap - self.start);
        out[..head].copy_from_slice(&self.buf[self.start..self.start + head]);
        if head < self.len {
            out[head..].copy_from_slice(&self.buf[..self.len - head]);
        }
        out
    }

    /// 把 `start` 前移到安全边界：行首（前一字节为 `\n`）、换行字节本身或
    /// ESC 序列起始；在 `SAFE_SCAN_LIMIT` 内找不到则保留原起点。
    fn align_start(&mut self) {
        let cap = self.buf.len();
        let limit = usize::min(self.len, SAFE_SCAN_LIMIT);
        let at = |i: usize| self.buf[(self.start + i) % cap];
        for i in 1..limit {
            let prev = self.buf[(self.start + i - 1) % cap];
            let cur = at(i);
            if prev == b'\n' || cur == b'\n' || cur == 0x1b {
                self.start = (self.start + i) % cap;
                self.len -= i;
                return;
            }
        }
    }
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self::new()
    }
}
