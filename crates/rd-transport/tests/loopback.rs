//! 127.0.0.1 上的 server/client 集成测试（T-004 测试要求全量覆盖）：
//! 握手与对端指纹、expected 指纹拦截、控制流 Envelope 往返、1 MB 视频帧、
//! 双向终端流、stream id 一致性、datagram 往返。

use bytes::Bytes;
use rd_proto::v0::{envelope, Envelope, Hello, HelloAck, ListWindows, WindowInfo, WindowList};
use rd_transport::{connect, DataFrame, Fingerprint, Identity, Server, StreamKind};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn start_server() -> anyhow::Result<(Server, Fingerprint)> {
    let id = Identity::generate("loopback-server")?;
    let fingerprint = id.fingerprint();
    let server = Server::bind("127.0.0.1:0".parse()?, &id)?;
    Ok((server, fingerprint))
}

fn hello_envelope(req_id: u32, device: &str) -> Envelope {
    Envelope {
        req_id,
        msg: Some(envelope::Msg::Hello(Hello {
            proto_ver: 0,
            device_name: device.to_owned(),
            decode_caps: vec![],
            supports_444: false,
            media_fec: false,
        })),
    }
}

fn ack_envelope(req_id: u32, host: &str) -> Envelope {
    Envelope {
        req_id,
        msg: Some(envelope::Msg::HelloAck(HelloAck {
            proto_ver: 0,
            host_name: host.to_owned(),
            encode_caps: vec![],
            media_fec: false,
            host_version: "0.0.0-test".to_owned(),
            mac_addrs: String::new(),
        })),
    }
}

#[tokio::test]
async fn handshake_and_peer_fingerprints() -> anyhow::Result<()> {
    let fut = async {
        let (server, server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let client_fp = client_id.fingerprint();

        let (client, server_conn) = tokio::join!(
            connect(server.local_addr(), &client_id, Some(server_fp)),
            async { server.accept().await }
        );
        let client = client?;
        let server_conn = server_conn?;

        assert_eq!(client.peer_fingerprint(), server_fp);
        assert_eq!(server_conn.peer_fingerprint(), client_fp);
        assert_ne!(server_fp, client_fp);
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn wrong_expected_fingerprint_fails_to_connect() -> anyhow::Result<()> {
    let fut = async {
        let (server, _server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let wrong = Fingerprint([0xEE; 32]);

        let (client_res, _server_res) = tokio::join!(
            connect(server.local_addr(), &client_id, Some(wrong)),
            async { server.accept().await }
        );
        let err = client_res.expect_err("connect with wrong expected fingerprint must fail");
        assert!(
            err.to_string().contains("fingerprint mismatch")
                || format!("{err:#}").contains("fingerprint mismatch"),
            "unexpected error: {err:#}"
        );
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn control_stream_envelope_roundtrip() -> anyhow::Result<()> {
    let fut = async {
        let (server, server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let (client, server_conn) = tokio::join!(
            connect(server.local_addr(), &client_id, Some(server_fp)),
            async { server.accept().await }
        );
        let client = client?;
        let server_conn = server_conn?;

        // 注意：QUIC 流的 STREAM 帧要等首字节写入才发出，accept_control 会阻塞到
        // 客户端发出首帧——与真实应用一样，两侧必须并发推进（join!）。
        let client_fut = async {
            let (mut c_tx, mut c_rx) = client.open_control().await?;
            let hello = hello_envelope(1, "controller");
            c_tx.send(&hello).await?;
            let got = c_rx.recv().await?.expect("envelope");
            match got.msg {
                Some(envelope::Msg::HelloAck(a)) => assert_eq!(a.host_name, "agent"),
                other => panic!("expected HelloAck, got {other:?}"),
            }

            // 空消息体（ListWindows{}）与列表消息也要能走通
            let list = Envelope {
                req_id: 2,
                msg: Some(envelope::Msg::ListWindows(ListWindows {})),
            };
            c_tx.send(&list).await?;
            let got = c_rx.recv().await?.expect("envelope");
            match got.msg {
                Some(envelope::Msg::WindowList(w)) => assert_eq!(w.windows.len(), 1),
                other => panic!("expected WindowList, got {other:?}"),
            }
            Ok::<(), anyhow::Error>(())
        };
        let server_fut = async {
            let (mut s_tx, mut s_rx) = server_conn.accept_control().await?;
            let got = s_rx.recv().await?.expect("envelope");
            assert_eq!(got.req_id, 1);
            assert!(matches!(got.msg, Some(envelope::Msg::Hello(_))));

            let ack = ack_envelope(1, "agent");
            s_tx.send(&ack).await?;

            let got = s_rx.recv().await?.expect("envelope");
            assert!(matches!(got.msg, Some(envelope::Msg::ListWindows(_))));

            let window_list = Envelope {
                req_id: 2,
                msg: Some(envelope::Msg::WindowList(WindowList {
                    windows: vec![WindowInfo::default()],
                })),
            };
            s_tx.send(&window_list).await?;
            Ok::<(), anyhow::Error>(())
        };
        let (c, s) = tokio::join!(client_fut, server_fut);
        c?;
        s?;
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn video_1mb_frame_roundtrip() -> anyhow::Result<()> {
    let fut = async {
        let (server, server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let (client, server_conn) = tokio::join!(
            connect(server.local_addr(), &client_id, Some(server_fp)),
            async { server.accept().await }
        );
        let client = client?;
        let server_conn = server_conn?;

        // 视频 = Agent→控制端 单向流：被控端(server)开流，控制端(client)收
        let mut video_tx = server_conn.open_data(StreamKind::Video).await?;

        let (kind, stream_id, mut video_rx) = client.accept_data().await?;
        assert_eq!(kind, StreamKind::Video);
        assert_eq!(
            stream_id,
            video_tx.id(),
            "accept_data must report same stream id"
        );

        // 1 MB，含内容校验的伪随机图案
        let payload: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();
        let header = rd_transport::VideoHeader {
            window_id: 42,
            seq: 7,
            pts_us: 123_456,
            keyframe: true,
        };

        let send_fut = async {
            video_tx.send_video(&header, &payload).await?;
            video_tx.finish()
        };
        let recv_fut = async {
            let frame = video_rx.recv().await?.expect("video frame");
            let n = match frame {
                DataFrame::Video(h, data) => {
                    assert_eq!(h, header);
                    assert_eq!(data.len(), payload.len());
                    assert_eq!(&data[..], &payload[..]);
                    data.len()
                }
                other => panic!("expected video frame, got {other:?}"),
            };
            assert!(
                video_rx.recv().await?.is_none(),
                "stream must end after finish"
            );
            Ok::<usize, anyhow::Error>(n)
        };
        let (sent, received) = tokio::join!(send_fut, recv_fut);
        sent?;
        assert_eq!(received?, 1024 * 1024);
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn tile_frame_roundtrip() -> anyhow::Result<()> {
    let fut = async {
        let (server, server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let (client, server_conn) = tokio::join!(
            connect(server.local_addr(), &client_id, Some(server_fp)),
            async { server.accept().await }
        );
        let client = client?;
        let server_conn = server_conn?;

        let mut tile_tx = server_conn.open_data(StreamKind::Tile).await?;
        let (kind, stream_id, mut tile_rx) = client.accept_data().await?;
        assert_eq!(kind, StreamKind::Tile);
        assert_eq!(stream_id, tile_tx.id());

        let header = rd_transport::TileHeader {
            window_id: 9,
            frame_seq: 3,
            x: 320,
            y: 240,
            w: 128,
            h: 64,
            fmt: 1, // zstd-BGRA
        };
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 7) as u8).collect();

        let send_fut = async {
            tile_tx.send_tile(&header, &payload).await?;
            // fmt=255 失效清除帧（空负载）
            tile_tx
                .send_tile(&rd_transport::TileHeader { fmt: 255, ..header }, &[])
                .await?;
            tile_tx.finish()
        };
        let recv_fut = async {
            let frame = tile_rx.recv().await?.expect("tile frame");
            match frame {
                DataFrame::Tile(h, data) => {
                    assert_eq!(h, header);
                    assert_eq!(&data[..], &payload[..]);
                }
                other => panic!("expected tile frame, got {other:?}"),
            }
            match tile_rx.recv().await?.expect("invalidate frame") {
                DataFrame::Tile(h, data) => {
                    assert_eq!(h.fmt, 255);
                    assert!(data.is_empty());
                }
                other => panic!("expected invalidate tile, got {other:?}"),
            }
            assert!(tile_rx.recv().await?.is_none());
            Ok::<(), anyhow::Error>(())
        };
        let (sent, received) = tokio::join!(send_fut, recv_fut);
        sent?;
        received?;
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn terminal_bidi_roundtrip_and_stream_ids() -> anyhow::Result<()> {
    let fut = async {
        let (server, server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let (client, server_conn) = tokio::join!(
            connect(server.local_addr(), &client_id, Some(server_fp)),
            async { server.accept().await }
        );
        let client = client?;
        let server_conn = server_conn?;

        // 终端 = 双向流：控制端(client)开，被控端(server)收
        let (mut c_tx, mut c_rx) = client.open_bidi(StreamKind::Terminal).await?;
        let (kind, stream_id, mut s_tx, mut s_rx) = server_conn.accept_bidi().await?;
        assert_eq!(kind, StreamKind::Terminal);
        assert_eq!(
            stream_id,
            c_tx.id(),
            "accept_bidi must report same stream id"
        );
        assert_eq!(c_rx.kind(), StreamKind::Terminal);
        assert_eq!(s_tx.kind(), StreamKind::Terminal);

        let send_fut = async {
            c_tx.send_raw(b"ls -la\r\n").await?;
            let out: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
            s_tx.send_raw(&out).await?;
            c_tx.finish()?;
            s_tx.finish()
        };
        let recv_fut = async {
            match s_rx.recv().await?.expect("input frame") {
                DataFrame::Raw(data) => assert_eq!(&data[..], b"ls -la\r\n"),
                other => panic!("expected raw input, got {other:?}"),
            }
            match c_rx.recv().await?.expect("output frame") {
                DataFrame::Raw(data) => assert_eq!(data.len(), 64 * 1024),
                other => panic!("expected raw output, got {other:?}"),
            }
            assert!(s_rx.recv().await?.is_none(), "client input must end");
            assert!(c_rx.recv().await?.is_none(), "server output must end");
            Ok::<(), anyhow::Error>(())
        };
        let (sent, received) = tokio::join!(send_fut, recv_fut);
        sent?;
        received?;
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn datagram_roundtrip() -> anyhow::Result<()> {
    let fut = async {
        let (server, server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let (client, server_conn) = tokio::join!(
            connect(server.local_addr(), &client_id, Some(server_fp)),
            async { server.accept().await }
        );
        let client = client?;
        let server_conn = server_conn?;

        // 05_api §3：datagram 用于鼠标移动（此处传输层只保证字节往返）
        let mouse_move: Vec<u8> = {
            let mut b = vec![1u8]; // type=1
            b.extend_from_slice(&42u64.to_le_bytes()); // window_id
            b.extend_from_slice(&100i32.to_le_bytes()); // x
            b.extend_from_slice(&200i32.to_le_bytes()); // y
            b.extend_from_slice(&1u32.to_le_bytes()); // seq
            b
        };
        assert_eq!(mouse_move.len(), 21);

        let (sent, received) = tokio::join!(
            async { client.send_datagram(Bytes::from(mouse_move.clone())) },
            async { server_conn.read_datagram().await }
        );
        sent?;
        assert_eq!(received?, Bytes::from(mouse_move));
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}

#[tokio::test]
async fn connection_close_and_rtt_available() -> anyhow::Result<()> {
    let fut = async {
        let (server, server_fp) = start_server().await?;
        let client_id = Identity::generate("loopback-client")?;
        let (client, server_conn) =
            tokio::join!(connect(server.local_addr(), &client_id, None), async {
                server.accept().await
            });
        let client = client?;
        let server_conn = server_conn?;

        // rtt()/stats() 可用（码率自适应输入）
        let _rtt = client.rtt();
        let _stats = client.stats();
        assert_eq!(client.peer_fingerprint(), server_fp);

        client.close(7, "bye");
        // 对端应观察到关闭（读操作以错误收场）
        let _closed = server_conn.read_datagram().await.expect_err("closed");
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("test timed out")
}
