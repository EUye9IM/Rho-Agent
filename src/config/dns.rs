//! 顶层 `dns` 配置项：DoH 端点解析与校验。
//!
//! 端点形如 `https://223.5.5.5/dns-query`：scheme 取 http 或 https，主机必须是
//! IP 字面量（端点用域名会让解析器先解析域名），路径必填。IPv6 主机写成 `[addr]`。

use std::net::{IpAddr, Ipv6Addr};

const PATH_HINT: &str = "缺少路径或路径为空（形如 https://223.5.5.5/dns-query）";
const IP_HINT: &str = "必须是 IP 字面量（端点用域名会让解析器解析自己）";

/// 解析一个 DoH 端点，返回去掉首尾空白后的 URL。
pub fn parse_endpoint(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("不能为空".to_string());
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "需要是 DoH 端点 URL，形如 https://223.5.5.5/dns-query".to_string())?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(format!("不支持的 scheme `{scheme}`（支持 http、https）"));
    }
    let (authority, path) = rest.split_once('/').ok_or_else(|| PATH_HINT.to_string())?;
    if path.is_empty() {
        return Err(PATH_HINT.to_string());
    }
    if authority.is_empty() {
        return Err("缺少主机".to_string());
    }
    if authority.contains('@') {
        return Err("端点 URL 不带用户信息".to_string());
    }
    let host = host_of_authority(authority)?;
    if host.parse::<IpAddr>().is_err() {
        return Err(format!("主机 `{host}` {IP_HINT}"));
    }
    Ok(url.to_string())
}

/// 从 `host[:port]` 取出主机；端口在此校验，IPv6 必须带方括号。
fn host_of_authority(authority: &str) -> Result<&str, String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| "IPv6 主机缺少 `]`".to_string())?;
        if !tail.is_empty() {
            let port = tail
                .strip_prefix(':')
                .ok_or_else(|| "`]` 之后只能是端口".to_string())?;
            parse_port(port)?;
        }
        return Ok(host);
    }
    // 裸 IPv6：URL 解析会把它拆成主机加端口，直接指出去加方括号。
    if authority.parse::<Ipv6Addr>().is_ok() {
        return Err("IPv6 主机需要方括号，形如 https://[2400:3200::1]/dns-query".to_string());
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            parse_port(port)?;
            Ok(host)
        }
        _ => Ok(authority),
    }
}

fn parse_port(port: &str) -> Result<u16, String> {
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("端口 `{port}` 需要是 1-65535 的整数"));
    }
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("端口 `{port}` 需要是 1-65535 的整数")),
        Ok(p) => Ok(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_accepts_mainland_doh_endpoints() {
        // 阿里 DoH、DNSPod DoH（IPv4 与 IPv6 各一），以及本机调试地址。
        for url in [
            "https://223.5.5.5/dns-query",
            "https://120.53.53.53/dns-query",
            "https://[2400:3200::1]/dns-query",
            "http://127.0.0.1:8053/dns-query",
        ] {
            assert_eq!(parse_endpoint(url).unwrap(), url);
        }
        // 首尾空白去掉后再返回。
        assert_eq!(
            parse_endpoint("  https://1.12.12.12/dns-query ").unwrap(),
            "https://1.12.12.12/dns-query"
        );
    }

    #[test]
    fn parse_endpoint_rejects_domains_and_malformed_urls() {
        let err = |raw: &str| parse_endpoint(raw).unwrap_err();
        assert!(err("").contains("不能为空"));
        assert!(err("223.5.5.5:53").contains("DoH 端点 URL"));
        assert!(err("ftp://223.5.5.5/dns-query").contains("不支持的 scheme"));
        assert!(err("https://223.5.5.5").contains("缺少路径"));
        assert!(err("https://223.5.5.5/").contains("缺少路径"));
        assert!(err("https:///dns-query").contains("缺少主机"));
        assert!(err("https://u:p@223.5.5.5/dns-query").contains("用户信息"));
        // 域名端点：引导问题。
        assert!(err("https://dns.alidns.com/dns-query").contains("IP 字面量"));
        // 端口越界 / 非数字。
        assert!(err("https://223.5.5.5:0/dns-query").contains("1-65535"));
        assert!(err("https://223.5.5.5:70000/dns-query").contains("1-65535"));
        assert!(err("https://223.5.5.5:53x/dns-query").contains("1-65535"));
        // IPv6 必须带方括号。
        assert!(err("https://2400:3200::1/dns-query").contains("方括号"));
        assert!(err("https://[2400:3200::1/dns-query").contains("缺少 `]`"));
    }
}
