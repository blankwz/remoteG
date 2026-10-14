# T-004 QUIC 传输层
- 所属 Epic/Story：E3 传输 / US-02、US-06
- 负责 Agent：A1 基础设施与协议
- 优先级：P0
- 预估：2d
- 前置依赖：T-001
- 可并行组：第 2 批

## 目标
`rd-transport` 提供基于 quinn 的双向认证连接：控制流（长度前缀 protobuf）、单向数据流（带帧头）、datagram，以及自签身份与对端指纹获取。

## 上下文（只读）
- 05_api/协议契约.md §1、§3
- 11_security/安全方案.md

## 修改范围（可写）
- 修改：`crates/rd-transport/Cargo.toml`、`src/lib.rs`、`src/identity.rs`、`src/framing.rs`、`src/conn.rs`、`src/verify.rs`
- 新建：`crates/rd-transport/tests/loopback.rs`

## 实现步骤
1. `identity.rs`：`Identity::generate(name)` 用 `rcgen` 生成 Ed25519 自签证书；`fingerprint()` = SHA-256(cert DER)。序列化为 DER 字节（持久化由 T-013 负责）。
2. `verify.rs`：rustls 自定义 `ServerCertVerifier` / `ClientCertVerifier`：**TLS 层接受任意自签证书**（校验签名与有效期），把对端指纹交给应用层；客户端可传 `expected: Option<Fingerprint>`，不匹配则握手失败。
3. `conn.rs`：`Server::bind(addr, identity)` / `Server::accept() -> Conn`；`connect(addr, identity, expected) -> Conn`；ALPN `rd/0`；`keep_alive_interval = 5s`，`max_idle_timeout = 20s`；启用 datagram。
4. `framing.rs`：控制流 `u32 LE 长度 + protobuf Envelope`；数据流帧头按 05_api §3（视频/瓦片/终端三种 `StreamKind`，流开头先写 1 字节 kind）。
5. 拥塞控制使用 quinn 默认（Cubic）；暴露 `Conn::rtt()` 与 `Conn::stats()` 供码率自适应。

## 接口
```rust
pub struct Fingerprint(pub [u8; 32]);       // Display 为 hex，前 8 字节用于 UI
pub struct Identity { pub cert_der: Vec<u8>, pub key_der: Vec<u8> }
pub struct Server; impl Server {
    pub fn bind(addr: SocketAddr, id: &Identity) -> anyhow::Result<Self>;
    pub async fn accept(&self) -> anyhow::Result<Conn>;
    pub fn local_addr(&self) -> SocketAddr;
}
pub async fn connect(addr: SocketAddr, id: &Identity, expected: Option<Fingerprint>) -> anyhow::Result<Conn>;
pub struct Conn; impl Conn {
    pub fn peer_fingerprint(&self) -> Fingerprint;
    pub async fn open_control(&self) -> anyhow::Result<(ControlTx, ControlRx)>;   // 客户端调用
    pub async fn accept_control(&self) -> anyhow::Result<(ControlTx, ControlRx)>; // 服务端调用
    pub async fn open_data(&self, kind: StreamKind) -> anyhow::Result<DataTx>;                    // 单向：视频/瓦片
    pub async fn accept_data(&self) -> anyhow::Result<(StreamKind, u64 /*stream id*/, DataRx)>;
    pub async fn open_bidi(&self, kind: StreamKind) -> anyhow::Result<(DataTx, DataRx)>;         // 双向：终端
    pub async fn accept_bidi(&self) -> anyhow::Result<(StreamKind, u64, DataTx, DataRx)>;
    pub fn send_datagram(&self, b: bytes::Bytes) -> anyhow::Result<()>;
    pub async fn read_datagram(&self) -> anyhow::Result<bytes::Bytes>;
    pub fn rtt(&self) -> Duration;
    pub fn close(&self, code: u32, reason: &str);
}
pub enum StreamKind { Video = 1, Tile = 2, Terminal = 3 }
impl ControlTx { pub async fn send(&mut self, e: &rd_proto::v0::Envelope) -> anyhow::Result<()> }
impl ControlRx { pub async fn recv(&mut self) -> anyhow::Result<Option<rd_proto::v0::Envelope>> }
pub struct VideoHeader { pub window_id: u64, pub seq: u32, pub pts_us: u64, pub keyframe: bool }
pub struct TileHeader  { pub window_id: u64, pub frame_seq: u32, pub x: u16, pub y: u16, pub w: u16, pub h: u16, pub fmt: u8 }
impl DataTx { pub fn id(&self) -> u64; /* QUIC stream id，填入 OpenWindowAck/TerminalOpened */ pub async fn send_video(&mut self, h: &VideoHeader, au: &[u8]); pub async fn send_tile(&mut self, h: &TileHeader, d: &[u8]); pub async fn send_raw(&mut self, d: &[u8]); }
impl DataRx { pub async fn recv(&mut self) -> anyhow::Result<Option<DataFrame>> } // enum DataFrame { Video(VideoHeader, Bytes), Tile(TileHeader, Bytes), Raw(Bytes) }
```

## 测试要求
- 新增 `tests/loopback.rs`：127.0.0.1 上 server/client 握手；双方拿到正确对端指纹；`expected` 错误时连接失败；控制流往返 Envelope；视频帧 1 MB 往返；双向终端流往返；`DataTx::id()` 与对端 `accept_*` 返回的 stream id 一致；datagram 往返。
- 运行：`cargo test -p rd-transport`

## 验收标准 DoD
- [ ] 上述测试全部通过（Windows 与 macOS）
- [ ] 帧头编码有单测，字节布局与 05_api §3 一致
- [ ] 公开 API 与本卡签名一致（可增加，不可删改）

## 风险与回滚
- rustls 自定义验证器 API 随版本变化：以实现时 rustls 版本文档为准，仅接口内部调整。
- 回滚：revert。

## 禁止事项
- 不实现信任列表/配对（T-013）；不改 proto。
