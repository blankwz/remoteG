/**
 * SSH 一键部署自建中转服务器前端 API（宪法 #3）。
 */
import { call } from "./bridge";

export interface SshDeployRequest {
  host: string;
  port: number;
  user: string;
  keyPath: string;
  relayPort: number;
  mode: "udp" | "docker";
  domain: string;
}

export interface SshDeployResult {
  ok: boolean;
  relayAddr: string;
  summary: string;
  log: string;
}

export function deployRelaySsh(req: SshDeployRequest): Promise<SshDeployResult> {
  return call<SshDeployResult>("deploy_relay_ssh", { req });
}
