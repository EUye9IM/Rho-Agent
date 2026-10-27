//! HTTP 基础设施：TLS 根配置（SSL_CERT_FILE 覆盖）、代理环境变量分流（NO_PROXY）、
//! 按 base_url 缓存 ureq::Agent（TLS 根与连接池复用）、错误体截断与读取上限。
//! 见 docs/architecture.md 四、五节。

use std::collections::HashMap;
use std::io::BufReader;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 单次响应体读取的空闲上限：连续无数据超过它就按流错误收尾。
/// 它同时是 socket 层的 `timeout_read`——ureq 只有一个读超时旋钮，
/// 且它同样作用于读取响应头阶段，因此不能用短超时来轮询取消标志：
/// 上游“思考”数秒才回头是常态，短超时会把正常请求当失败。
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 单次响应体累计字节上限，防止异常上游无界堆内存。
pub(super) const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
/// HTTP 错误体最多读取的字节数（仅供展示，之后还会按字符截断）。
pub(super) const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// 自建 TLS 配置：默认编入 webpki-roots；设置 SSL_CERT_FILE 时**覆盖**默认根。
fn tls_config() -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    match std::env::var("SSL_CERT_FILE") {
        Ok(path) => {
            let file = std::fs::File::open(&path)
                .map_err(|e| format!("无法读取 SSL_CERT_FILE={path}：{e}"))?;
            let mut reader = BufReader::new(file);
            let certs = rustls_pemfile::certs(&mut reader)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("解析 SSL_CERT_FILE={path} 失败：{e}"))?;
            if certs.is_empty() {
                return Err(format!("SSL_CERT_FILE={path} 中没有可用证书"));
            }
            for cert in certs {
                roots
                    .add(cert)
                    .map_err(|e| format!("加载 SSL_CERT_FILE={path} 的证书失败：{e}"))?;
            }
        }
        Err(_) => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }

    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn host_of_url(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    // IPv6 字面量：`[::1]:8080` → `::1`
    if let Some(rest) = host_port.strip_prefix('[') {
        return rest.split_once(']').map(|(host, _)| host);
    }
    // 去掉端口。
    host_port.split(':').next()
}

fn no_proxy_match(host: &str) -> bool {
    let value = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .ok();
    let Some(value) = value else { return false };
    let host = host.to_ascii_lowercase();
    value.split(',').any(|raw| {
        let entry = raw.trim().trim_start_matches('.').to_ascii_lowercase();
        if entry.is_empty() {
            false
        } else if entry == "*" {
            true
        } else {
            host == entry || host.ends_with(&format!(".{entry}"))
        }
    })
}

fn proxy_for(url: &str) -> Result<Option<ureq::Proxy>, String> {
    let Some(host) = host_of_url(url) else { return Ok(None) };
    if no_proxy_match(host) {
        return Ok(None);
    }
    let names: &[&str] = if url.starts_with("https://") {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    let Some((name, value)) = names.iter().find_map(|n| {
        std::env::var(n)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| (*n, v))
    }) else {
        return Ok(None);
    };
    ureq::Proxy::new(value.trim())
        .map(Some)
        .map_err(|e| format!("代理环境变量 {name} 配置无效：{e}"))
}

/// 按 `base_url` 缓存 `ureq::Agent`：TLS 根与连接池都随之复用，
/// 避免每轮请求重读 `SSL_CERT_FILE`、重解析 PEM、丢掉 keep-alive 连接。
pub(super) fn agent_for(base_url: &str) -> Result<Arc<ureq::Agent>, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<ureq::Agent>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(agent) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(base_url)
    {
        return Ok(agent.clone());
    }
    // 构建时不持锁：并发重复构建只会多出一个被丢弃的 Agent。
    let agent = Arc::new(build_agent(base_url)?);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(base_url.to_string(), agent.clone());
    Ok(agent)
}

fn build_agent(base_url: &str) -> Result<ureq::Agent, String> {
    let tls = tls_config()?;
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        // 空闲读超时：等待响应头与读取响应体共用这一个上限。
        .timeout_read(IDLE_TIMEOUT)
        .tls_config(Arc::new(tls));
    if let Some(proxy) = proxy_for(base_url)? {
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

/// 错误体展示截断（HTTP 错误路径专用）。
pub(super) fn shorten(s: &str) -> String {
    const MAX: usize = 4000;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX).collect();
        out.push_str("…（截断）");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_url_handles_ports_credentials_and_ipv6() {
        assert_eq!(host_of_url("https://api.example.com/chat"), Some("api.example.com"));
        assert_eq!(host_of_url("https://api.example.com:8443/chat"), Some("api.example.com"));
        assert_eq!(host_of_url("https://[::1]:8443/chat"), Some("::1"));
        assert_eq!(host_of_url("http://user:pw@host:80"), Some("host"));
    }
}
