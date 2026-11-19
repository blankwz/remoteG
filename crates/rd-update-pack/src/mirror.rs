//! 国内渠道补传（改造项 B）：从 GitHub Releases 拉产物 → 校验 → 上传 COS。
//!
//! 上传幂等（可中断重跑）：内容寻址——每个对象上传前先查目标桶里是否已有
//! 同名对象：没有 → 上传；有 → 拉回逐字节 sha256 对比，相同跳过、不同拒绝
//! （不同内容同名 = 构建不可复现或桶被污染，人工介入，绝不静默覆盖）。
//!
//! latest 指针切换防回滚：`latest/update.json` 只在新版本 >= 当前指针版本时
//! 切，旧版本重跑直接拒绝——防止陈旧产物覆盖最新渠道指针。同版本重跑允许
//! （内容已被内容寻址校验过，等价于跳过）。
//!
//! 密钥纪律（对齐 T-036）：secretId/secretKey 只经 `--cos-keys @文件`（CI
//! secret 或凭据管理释放到临时文件）传入，绝不进仓库、不进日志——本模块
//! 打印只含对象 key 与字节数，无密钥无签名串。
//!
//! COS XML API 签名按官方文档（cloud.tencent.com/document/product/436/7778）：
//! HMAC-SHA1 双层链，Authorization 头携带；本工具只签 `host`（与 PUT 的
//! `content-length`），其余头不参与签名（文档允许按需筛选）。固定向量单测
//! 锚定算法不漂移；真渠道（真桶真密钥）验证归 T-039。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha1::Digest;

/// COS 桶配置（`--cos` 的 JSON 结构，或 `--cos @文件`）。
#[derive(Debug, Clone, Deserialize)]
pub struct CosConfig {
    /// 桶域名，如 `mybucket-1250000000.cos.ap-beijing.myqcloud.com`。
    pub host: String,
    /// 对象 key 前缀，如 `rd-update`（缺省 = 桶根）。键名兼容 `prefix`。
    #[serde(default, alias = "prefix")]
    pub prefix: String,
}

/// COS 密钥（`--cos-keys` 的 JSON 结构，或 `--cos-keys @文件`）。
#[derive(Debug, Clone, Deserialize)]
pub struct CosKeys {
    pub secret_id: String,
    pub secret_key: String,
}

/// COS 的 UrlEncode：RFC3986 保留字全集（含 `/`）都编码；非 ASCII 按 UTF-8
/// 字节逐字节编码。与官方文档的编码表一致。
pub(crate) fn cos_uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 单层 HMAC-SHA1（hex 小写）。
fn hmac_sha1_hex(key: &str, msg: &str) -> String {
    let mut mac =
        <Hmac<sha1::Sha1> as Mac>::new_from_slice(key.as_bytes()).expect("HMAC 接受任意长度密钥");
    mac.update(msg.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 单层 SHA1（hex 小写）。
fn sha1_hex(msg: &str) -> String {
    let mut h = sha1::Sha1::new();
    h.update(msg.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 生成 `Authorization` 头的值。
///
/// `key_time` = `start;end`（Unix 秒）；`uri_path` 是已 UrlEncode 的请求路径
/// （以 `/` 开头）；`signed_headers` = 要签的 (头名, 值) 列表（名须小写）。
pub(crate) fn build_authorization(
    keys: &CosKeys,
    key_time: &str,
    method: &str,
    uri_path: &str,
    signed_headers: &[(&str, &str)],
) -> String {
    // HttpHeaders：键 UrlEncode 后排序，k=v&…；本工具无 URL 参数，参数恒为空串
    let mut pairs: Vec<(String, String)> = signed_headers
        .iter()
        .map(|(k, v)| (cos_uri_encode(&k.to_lowercase()), cos_uri_encode(v)))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let http_headers = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let header_list = pairs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let http_string = format!("{method}\n{uri_path}\n\n{http_headers}\n");
    let string_to_sign = format!("sha1\n{key_time}\n{}\n", sha1_hex(&http_string));
    let signature = hmac_sha1_hex(&hmac_sha1_hex(&keys.secret_key, key_time), &string_to_sign);

    format!(
        "q-sign-algorithm=sha1&q-ak={}&q-sign-time={key_time}&q-key-time={key_time}&q-header-list={header_list}&q-url-param-list=&q-signature={signature}",
        cos_uri_encode(&keys.secret_id)
    )
}

/// 当前 Unix 秒，签名有效期以它为起点。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// COS XML API 客户端（同步，发布端 CLI 专用）。
pub struct CosClient {
    http: reqwest::blocking::Client,
    config: CosConfig,
    keys: CosKeys,
}

impl CosClient {
    pub fn new(config: CosConfig, keys: CosKeys) -> anyhow::Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(600))
            .build()?;
        Ok(Self { http, config, keys })
    }

    /// 对象完整 key：前缀 + 相对 key。
    fn object_key(&self, key: &str) -> String {
        if self.config.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.config.prefix)
        }
    }

    fn request(&self, method: &str, key: &str) -> reqwest::blocking::RequestBuilder {
        let full_key = self.object_key(key);
        let uri_path = format!("/{}", cos_uri_encode(&full_key));
        let url = format!("https://{}{uri_path}", self.config.host);
        let key_time = format!("{};{}", unix_now(), unix_now() + 600);
        let signed: Vec<(&str, &str)> = vec![("host", self.config.host.as_str())];
        let auth = build_authorization(&self.keys, &key_time, method, &uri_path, &signed);
        self.http
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).expect("合法 HTTP 方法"),
                &url,
            )
            .header(reqwest::header::AUTHORIZATION, auth)
    }

    /// 上传对象（内容整体一次 PUT；产物最大几十 MB，无需分块上传 API）。
    pub fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        let full_key = self.object_key(key);
        let uri_path = format!("/{}", cos_uri_encode(&full_key));
        let url = format!("https://{}{uri_path}", self.config.host);
        let key_time = format!("{};{}", unix_now(), unix_now() + 600);
        let len_str = data.len().to_string();
        let signed: Vec<(&str, &str)> = vec![
            ("host", self.config.host.as_str()),
            ("content-length", len_str.as_str()),
        ];
        let auth = build_authorization(&self.keys, &key_time, "put", &uri_path, &signed);
        let resp = self
            .http
            .put(&url)
            .header(reqwest::header::AUTHORIZATION, auth)
            .header(reqwest::header::CONTENT_LENGTH, data.len())
            .body(data.to_vec())
            .send()
            .with_context(|| format!("COS PUT 请求失败: {key}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            let head = body.chars().take(200).collect::<String>();
            anyhow::bail!("COS PUT {key} 失败: HTTP {status}（{head}）");
        }
        Ok(())
    }

    /// 对象是否存在（存在返回字节数）。404 → None，其余错误 → Err。
    pub fn head(&self, key: &str) -> anyhow::Result<Option<u64>> {
        let resp = self.request("head", key).send()?;
        match resp.status() {
            s if s.is_success() => {
                let len = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(Some(len))
            }
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s => anyhow::bail!("COS HEAD {key} 失败: HTTP {s}"),
        }
    }

    /// 拉取对象内容。404 → None，其余错误 → Err。
    pub fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let resp = self.request("get", key).send()?;
        match resp.status() {
            s if s.is_success() => Ok(Some(resp.bytes()?.to_vec())),
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s => anyhow::bail!("COS GET {key} 失败: HTTP {s}"),
        }
    }
}

/// latest 指针防回滚判定：当前指针存在且比新版本还新 → 拒绝切换（Err）；
/// 同版本或指针不存在 → 允许；新版本更晚 → 允许。
pub fn should_switch_pointer(
    current_version: Option<&str>,
    new_version: &str,
) -> anyhow::Result<bool> {
    match current_version {
        None => Ok(true),
        Some(cur) => match crate::compare_versions(cur, new_version) {
            std::cmp::Ordering::Greater => {
                anyhow::bail!("拒绝回滚：latest 指针在 {cur}，新版本 {new_version} 更旧")
            }
            std::cmp::Ordering::Equal => Ok(false),
            std::cmp::Ordering::Less => Ok(true),
        },
    }
}

/// 把 JSON 文本或 `@文件` 引用解析成 T。
fn load_json<T: serde::de::DeserializeOwned>(spec: &str, what: &str) -> anyhow::Result<T> {
    let raw = if let Some(path) = spec.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("读 {what} 文件失败: {path}"))?
    } else {
        spec.to_string()
    };
    serde_json::from_str(raw.trim()).with_context(|| format!("{what} JSON 解析失败"))
}

/// mirror 子命令入口（rd-pack-cli 调用）。
///
/// 从 GitHub Release 拉文件清单，逐一内容寻址上传 COS；全部成功后按防回滚
/// 规则切换 `latest/update.json`（与 `.sig`）。可中断重跑：已传且一致的跳过。
pub fn cmd_mirror(
    tag: &str,
    github_base: Option<&str>,
    files: Option<&[String]>,
    cos_spec: &str,
    cos_keys_spec: &str,
) -> anyhow::Result<()> {
    let config: CosConfig = load_json(cos_spec, "COS 配置")?;
    let keys: CosKeys = load_json(cos_keys_spec, "COS 密钥")?;
    let version = tag
        .strip_prefix('v')
        .filter(|v| !v.is_empty())
        .context("tag 须形如 v0.3.0")?;

    // 默认镜像全套：zip + pack + 清单 + 签名
    let zip_name = format!("rd-agent-{version}-win64.zip");
    let default_files = [
        zip_name.clone(),
        zip_name.replace(".zip", ".pack"),
        "update.json".to_string(),
        "update.json.sig".to_string(),
    ];
    let file_names: Vec<&str> = match files {
        Some(f) => f.iter().map(|s| s.as_str()).collect(),
        None => default_files.iter().map(|s| s.as_str()).collect(),
    };

    let base = match github_base {
        Some(b) => b.trim_end_matches('/').to_string(),
        None => {
            // CI 里 GITHUB_REPOSITORY=owner/repo；本地手动补传需显式传
            let repo = std::env::var("GITHUB_REPOSITORY").context(
                "未传 --github-base 且环境无 GITHUB_REPOSITORY，无法定位 GitHub 下载地址",
            )?;
            format!("https://github.com/{repo}/releases/download/{tag}")
        }
    };
    let client = CosClient::new(config, keys)?;

    let mut uploaded: Vec<(String, usize)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut pack_name: Option<String> = None;
    let mut pack_bytes: Option<Vec<u8>> = None;
    for name in &file_names {
        let url = format!("{base}/{name}");
        let data = reqwest::blocking::get(&url)
            .with_context(|| format!("GitHub 下载失败: {url}"))?
            .error_for_status()
            .with_context(|| format!("GitHub 返回错误状态: {url}"))?
            .bytes()?
            .to_vec();
        println!("拉取 {name}: {} bytes", data.len());
        if name.ends_with(".pack") {
            pack_name = Some((*name).to_string());
            pack_bytes = Some(data.clone());
        }

        let key = format!("v{version}/{name}");
        match client.head(&key)? {
            Some(_) => {
                // 已存在：拉回逐字节比对（内容寻址，绝不静默覆盖）
                let remote = client
                    .get(&key)?
                    .context("HEAD 有但 GET 没有，桶状态异常")?;
                if crate::sha256_hex(&remote) == crate::sha256_hex(&data) {
                    println!("跳过 {key}（内容一致）");
                    skipped.push(key);
                } else {
                    anyhow::bail!("COS 已有 {key} 但内容不同（sha256 不一致），拒绝覆盖——请人工核查构建可复现性");
                }
            }
            None => {
                client.put(&key, &data)?;
                println!("上传 {key}: {} bytes", data.len());
                uploaded.push((key, data.len()));
            }
        }
    }

    // latest 指针：update.json + .sig + pack 复制到 latest/（渠道目录，Agent
    // 端 RD_UPDATE_ENDPOINT 直接指这里，三类文件同 base 可取）；防回滚
    let manifest_bytes = reqwest::blocking::get(format!("{base}/update.json"))?
        .error_for_status()?
        .bytes()?
        .to_vec();
    let manifest = crate::Manifest::decode(&manifest_bytes)?;
    if manifest.version != version {
        anyhow::bail!(
            "update.json 里的版本 {} 与 tag {tag} 不一致",
            manifest.version
        );
    }
    let current = client
        .get("latest/update.json")?
        .and_then(|b| crate::Manifest::decode(&b).ok())
        .map(|m| m.version);
    if should_switch_pointer(current.as_deref(), version)? {
        client.put("latest/update.json", &manifest_bytes)?;
        if let Some(sig) = file_names.iter().find(|n| **n == "update.json.sig") {
            let sig_url = format!("{base}/{sig}");
            let sig_bytes = reqwest::blocking::get(sig_url)?
                .error_for_status()?
                .bytes()?
                .to_vec();
            client.put("latest/update.json.sig", &sig_bytes)?;
        }
        // pack 传双份：latest/ 给 Agent 下载，v{version}/ 留历史存档
        if let (Some(name), Some(data)) = (pack_name.as_deref(), pack_bytes.as_deref()) {
            client.put(&format!("latest/{name}"), data)?;
        }
        println!("latest 指针已切换到 v{version}");
    } else {
        println!("latest 指针已是 v{version}，保持不变");
    }

    println!(
        "mirror 完成：上传 {} 个对象（{} bytes），跳过 {} 个",
        uploaded.len(),
        uploaded.iter().map(|(_, n)| n).sum::<usize>(),
        skipped.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方签名算法固定向量（手算锚定，防算法漂移）。
    /// 输入：secretKey=AKIDexample, KeyTime=1700000000;1700000600,
    /// put /rd-update/v0.3.0/update.json，仅签 host。
    #[test]
    fn signature_matches_reference_vector() {
        let keys = CosKeys {
            secret_id: "AKIDtest".to_string(),
            secret_key: "AKIDexample".to_string(),
        };
        let key_time = "1700000000;1700000600";
        let signed_headers = [("host", "test-bucket-1250000000.cos.ap-beijing.myqcloud.com")];
        let auth = build_authorization(
            &keys,
            key_time,
            "put",
            "/rd-update/v0.3.0/update.json",
            &signed_headers,
        );
        assert!(auth.starts_with("q-sign-algorithm=sha1&q-ak=AKIDtest&q-sign-time=1700000000;1700000600&q-key-time=1700000000;1700000600&q-header-list=host&q-url-param-list=&q-signature="));
        assert!(
            auth.ends_with("q-signature=8c9ff5fb440876123b12c9edb487be13ec8d4cdc"),
            "签名与参考向量不符: {auth}"
        );
    }

    #[test]
    fn uri_encode_covers_reserved_chars() {
        assert_eq!(cos_uri_encode("a b/c.d~e_f-g"), "a%20b%2Fc.d~e_f-g");
        assert_eq!(cos_uri_encode("云"), "%E4%BA%91");
    }

    #[test]
    fn pointer_rollback_rejected() {
        // 旧版本重跑 → 拒绝
        assert!(should_switch_pointer(Some("0.4.0"), "0.3.0").is_err());
        // 同版本重跑 → 允许（内容寻址已保证一致）
        assert!(matches!(
            should_switch_pointer(Some("0.3.0"), "0.3.0"),
            Ok(false)
        ));
        // 新版本 → 允许
        assert!(matches!(
            should_switch_pointer(Some("0.3.0"), "0.4.0"),
            Ok(true)
        ));
        // 首次发布 → 允许
        assert!(matches!(should_switch_pointer(None, "0.3.0"), Ok(true)));
    }
}
