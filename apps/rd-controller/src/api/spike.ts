/**
 * spike 连接封装（T-005）。
 *
 * Rust 侧建立 QUIC 并后台泵数据流；每帧以 InvokeResponseBody::Raw 经 Channel
 * 到达前端，message 为 ArrayBuffer（25B 帧头 + Annex-B AU 原样）。
 */
import { Channel, invoke } from "@tauri-apps/api/core";

/**
 * 连接 rd-agent spike 端。
 *
 * resolve 表示连接与 Hello 握手成功；帧随后异步到达 `onFrame`。
 * 对同一地址重复调用会替换旧连接（Rust 侧关闭旧的）。
 */
export function spikeConnect(
  addr: string,
  onFrame: (frame: ArrayBuffer) => void,
): Promise<void> {
  const on_frame = new Channel<ArrayBuffer>();
  on_frame.onmessage = onFrame;
  return invoke("spike_connect", { addr, onFrame: on_frame });
}
