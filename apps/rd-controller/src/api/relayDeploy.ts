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

/**
 * 归一化用户在输入框粘贴的 `[user@]host[:port]` 与带引号私钥路径。
 */
export function normalizeSshInput(raw: {
  host: string;
  port: string;
  user: string;
  keyPath: string;
}): { host: string; port: number; user: string; keyPath: string } {
  let rest = raw.host.trim();
  let user = raw.user.trim() || "root";
  let port = Number(raw.port.trim()) || 22;

  const atIdx = rest.lastIndexOf("@");
  if (atIdx > 0) {
    const maybeUser = rest.slice(0, atIdx).trim();
    if (maybeUser) {
      user = maybeUser;
    }
    rest = rest.slice(atIdx + 1).trim();
  }

  if (rest.startsWith("[")) {
    const closeIdx = rest.indexOf("]");
    if (closeIdx > 0) {
      const ipv6 = rest.slice(1, closeIdx);
      const tail = rest.slice(closeIdx + 1);
      if (tail.startsWith(":")) {
        const parsedPort = Number(tail.slice(1));
        if (Number.isInteger(parsedPort) && parsedPort > 0 && parsedPort <= 65535) {
          port = parsedPort;
        }
      }
      rest = ipv6;
    }
  } else {
    const firstColon = rest.indexOf(":");
    const lastColon = rest.lastIndexOf(":");
    if (firstColon > 0 && firstColon === lastColon) {
      const parsedPort = Number(rest.slice(firstColon + 1));
      if (Number.isInteger(parsedPort) && parsedPort > 0 && parsedPort <= 65535) {
        port = parsedPort;
        rest = rest.slice(0, firstColon).trim();
      }
    }
  }

  const keyPath = raw.keyPath.trim().replace(/^["']+|["']+$/g, "").trim();
  return { host: rest, port, user, keyPath };
}

export function deployRelaySsh(req: SshDeployRequest): Promise<SshDeployResult> {
  return call<SshDeployResult>("deploy_relay_ssh", { req });
}
