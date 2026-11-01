#![cfg(windows)]
//! 最小复现实验（T-032 排查）：纯 quinn + 内存管道 AsyncUdpSocket。
//!
//! 两个 Endpoint 经内存 datagram 通道互通，open_bi → send → accept_bi → read。
//! 若挂起 → quinn + 抽象 socket 的用法问题（quinn 侧）；若通过 → rd 侧封装问题。

use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, Endpoint, EndpointConfig, UdpPoller};
use rd_transport::Identity;
use tokio::sync::mpsc;

#[derive(Debug)]
struct PipeSocket {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: std::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    peer: SocketAddr,
}

impl AsyncUdpSocket for PipeSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysReady)
    }
    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        self.tx
            .send(transmit.contents.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "peer gone"))
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        let mut rx = self.rx.lock().expect("pipe 锁");
        let mut got = 0;
        while got < bufs.len() {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(d)) => {
                    let n = d.len();
                    bufs[got][..n].copy_from_slice(&d);
                    meta[got] = RecvMeta {
                        addr: self.peer,
                        dst_ip: None,
                        ecn: None,
                        len: n,
                        stride: n,
                    };
                    got += 1;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "closed",
                    )))
                }
                Poll::Pending => break,
            }
        }
        if got > 0 {
            Poll::Ready(Ok(got))
        } else {
            Poll::Pending
        }
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.peer)
    }
    fn may_fragment(&self) -> bool {
        false
    }
}

#[derive(Debug)]
struct AlwaysReady;

impl UdpPoller for AlwaysReady {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn make_pipe_pair(peer: SocketAddr) -> (Arc<PipeSocket>, Arc<PipeSocket>) {
    let (a_tx, a_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (b_tx, b_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let a = Arc::new(PipeSocket {
        tx: b_tx.clone(),
        rx: std::sync::Mutex::new(a_rx),
        peer,
    });
    let b = Arc::new(PipeSocket {
        tx: a_tx,
        rx: std::sync::Mutex::new(b_rx),
        peer,
    });
    (a, b)
}

/// 分层二分：真实 rd_relay（UDP 转发）+ 裸 quinn open_bi/accept_bi + 4B 读写。
/// 若本测试挂 → RelayUdpSocket/rd_relay 层问题；若过 → rd 的 Conn 层问题。
#[tokio::test(flavor = "multi_thread")]
async fn layered_relay_udp_raw_quinn() -> anyhow::Result<()> {
    use rd_relay as _;

    let relay_addr = rd_relay::spawn("127.0.0.1:0".parse()?).await?;
    let server_id = Identity::generate("layered-server")?;
    let client_id = Identity::generate("layered-client")?;
    let server_fp = server_id.fingerprint();

    // 复用 rd 的虚拟 socket（与失败 e2e 完全同路径）
    let server = rd_transport::bind_via_relay(relay_addr, &server_id).await?;
    let accepter = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept 失败");
        let quinn_conn = conn.raw_conn();
        let (mut tx, mut rx) = quinn_conn
            .accept_bi()
            .await
            .map_err(|e| anyhow::anyhow!("accept_bi: {e}"))?;
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rx.read_exact(&mut len_buf),
        )
        .await
        .expect("server 读长度前缀超时——流数据不可读！")
        .map_err(|e| anyhow::anyhow!("读失败: {e}"))?;
        println!("layered: server 读到 {} 字节", len_buf.len());
        tx.write_all(b"ack").await?;
        anyhow::Ok(())
    });

    let client = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rd_transport::connect_via_relay(relay_addr, server_fp, &client_id, Some(server_fp)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("连接超时（20s）"))?
    .map_err(|e| anyhow::anyhow!("连接失败: {e:#}"))?;

    // 裸 quinn API（绕过 rd 的 ControlRx，排除 rd 层流封装）
    let (mut tx, _rx) = client.raw_conn().open_bi().await?;
    tx.write_all(&[1u8; 19]).await?;

    accepter.await.map_err(|e| anyhow::anyhow!("{e}"))??;
    println!("layered relay + raw quinn: PASS");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn minimal_quinn_over_pipe() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "quinn=debug,quinn_proto=debug",
        ))
        .with_writer(std::io::stderr)
        .try_init();

    let fake_peer: SocketAddr = "127.0.0.1:1".parse()?;
    let (server_sock, client_sock) = make_pipe_pair(fake_peer);
    let server_id = Identity::generate("minimal-server")?;
    let client_id = Identity::generate("minimal-client")?;
    let server_fp = server_id.fingerprint();

    let server = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(rd_transport::server_config_for_test(&server_id)?),
        server_sock,
        Arc::new(quinn::TokioRuntime),
    )?;
    let mut client = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        client_sock,
        Arc::new(quinn::TokioRuntime),
    )?;
    client.set_default_client_config(rd_transport::client_config_for_test(
        &client_id,
        Some(server_fp),
    )?);

    let accepter = tokio::spawn(async move {
        let incoming = server.accept().await.expect("incoming");
        let conn = incoming
            .await
            .map_err(|e| anyhow::anyhow!("握手失败: {e}"))?;
        let (mut tx, mut rx) = conn
            .accept_bi()
            .await
            .map_err(|e| anyhow::anyhow!("accept_bi 失败: {e}"))?;
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rx.read_exact(&mut len_buf),
        )
        .await
        .expect("server 读长度前缀超时——流数据不可读！")
        .map_err(|e| anyhow::anyhow!("读失败: {e}"))?;
        println!("server 读到 {} 字节", len_buf.len());
        tx.write_all(b"ack")
            .await
            .map_err(|e| anyhow::anyhow!("写失败: {e}"))?;
        anyhow::Ok(())
    });

    let connect_fut = async {
        let connecting = client
            .connect(fake_peer, "rd.local")
            .map_err(|e| anyhow::anyhow!("connect: {e}"))?;
        connecting.await.map_err(|e| anyhow::anyhow!("握手: {e}"))
    };
    let conn = tokio::time::timeout(std::time::Duration::from_secs(10), connect_fut)
        .await
        .expect("QUIC 握手超时")
        .map_err(|e| anyhow::anyhow!("握手失败: {e}"))?;

    let (mut tx, _rx) = conn.open_bi().await?;
    tx.write_all(&[1u8; 19]).await?;

    accepter
        .await
        .map_err(|e| anyhow::anyhow!("accepter 任务失败: {e}"))??;
    println!("minimal quinn over pipe: PASS");
    Ok(())
}
