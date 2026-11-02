//! 直连 UDP socket 的 quinn 适配（T-034 打洞配套）。
//!
//! 打洞竞速要求**用探测用的同一个本地端口**发 QUIC（端口受限型 NAT 下，
//! 被控端 NAT 放行的是「探测 socket 的 IP:端口」，换端口即失去刚开的洞）。
//! quinn 的 client socket 可以由外部提供——本模块把裸 [`tokio::net::UdpSocket`]
//! 包成 `quinn::AsyncUdpSocket`（原样收发，无打标、无过滤）。
//!
//! 另供 agent 侧 [`Server::bind_socket`](crate::Server::bind_socket)：先用
//! 本模块 bind 裸 socket，再包给 quinn——同一个 Arc 同时交给探测任务发
//! 探测包（并发 send 安全），探测与 QUIC 共端口。

use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::io::Interest;
use tokio::net::UdpSocket;

/// 绑定一个裸 UDP socket（打洞探测与 QUIC 共用）。
pub async fn bind_udp_socket(addr: SocketAddr) -> std::io::Result<Arc<UdpSocket>> {
    UdpSocket::bind(addr).await.map(Arc::new)
}

/// 裸 tokio UDP socket 的 quinn 视图（原样收发；目的地址取 transmit 自带值）。
#[derive(Debug)]
pub(crate) struct DirectUdpSocket {
    inner: Arc<UdpSocket>,
}

impl DirectUdpSocket {
    pub fn new(inner: Arc<UdpSocket>) -> Self {
        Self { inner }
    }
}

/// 写就绪 poller（与 relay.rs 的 RelayPoller 同型：tokio socket 自管读注册）。
#[derive(Debug)]
struct DirectPoller {
    inner: Arc<UdpSocket>,
}

impl UdpPoller for DirectPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().inner.poll_send_ready(cx)
    }
}

impl AsyncUdpSocket for DirectUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(DirectPoller {
            inner: Arc::clone(&self.inner),
        })
    }

    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        let dst = transmit.destination;
        self.inner.try_io(Interest::WRITABLE, || {
            self.inner.try_send_to(transmit.contents, dst).map(|_| ())
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        let mut got = 0usize;
        while got < bufs.len() {
            let mut rb = tokio::io::ReadBuf::new(&mut bufs[got]);
            let from = match self.inner.poll_recv_from(cx, &mut rb) {
                Poll::Ready(Ok(addr)) => addr,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => break,
            };
            let n = rb.filled().len();
            meta[got] = RecvMeta {
                addr: from,
                dst_ip: None,
                ecn: None,
                len: n,
                stride: n,
            };
            got += 1;
        }
        if got > 0 {
            Poll::Ready(Ok(got))
        } else {
            Poll::Pending
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn may_fragment(&self) -> bool {
        false // MTU 钳 1200，不 IP 分片
    }
}
