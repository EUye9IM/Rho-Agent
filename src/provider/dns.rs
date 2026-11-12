//! DoH 解析器：配置了端点时优先查询，失败后回退系统解析器。
//!
//! 端点主机是 IP 字面量，查询不经过系统解析器，因此系统没有
//! `/etc/resolv.conf`（例如 Android/Termux 跑 musl 静态二进制）时域名仍可解析。
//! 成功结果按应答 TTL 缓存，失败不缓存。

use std::collections::HashMap;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 单次 DoH 请求的整体上限（连接 + 读写）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// DoH 响应体上限：一条 DNS 报文远小于此值。
const MAX_BODY_BYTES: u64 = 65535;
/// 缓存 TTL 的区间（秒）。
const MIN_TTL: u32 = 30;
const MAX_TTL: u32 = 600;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

/// 进程内唯一实例：`main` 在配置校验后安装，`http::build_agent` 取用。
static RESOLVER: OnceLock<Arc<Resolver>> = OnceLock::new();

/// 安装解析器；`endpoints` 是配置校验通过的 DoH URL。
pub fn init(endpoints: &[String]) {
    let _ = RESOLVER.set(Arc::new(Resolver::new(endpoints.to_vec())));
}

/// 当前解析器；未安装时只做系统解析。
pub(super) fn resolver() -> Arc<Resolver> {
    RESOLVER
        .get_or_init(|| Arc::new(Resolver::new(Vec::new())))
        .clone()
}

/// 解析结果缓存项：到期时间与地址。
struct CacheEntry {
    expires: Instant,
    addrs: Vec<SocketAddr>,
}

pub(super) struct Resolver {
    endpoints: Vec<String>,
    agent: OnceLock<ureq::Agent>,
    cache: Mutex<HashMap<(String, u16), CacheEntry>>,
}

impl Resolver {
    fn new(endpoints: Vec<String>) -> Self {
        Self {
            endpoints,
            agent: OnceLock::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 解析 `host:port`：IP 字面量直接返回；配置了 DoH 端点时先用它，
    /// 端点和系统解析都失败才报错。
    pub(super) fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let (host, port) = split_netloc(netloc).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("无法解析主机名：{netloc}"),
            )
        })?;
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }

        // DoH 优先：配置了端点就先按声明顺序查它。
        let doh_err = if self.endpoints.is_empty() {
            None
        } else {
            match self.lookup(host, port) {
                Ok(addrs) => return Ok(addrs),
                Err(e) => Some(e),
            }
        };

        // 端点全部失败（或未配置）时用系统解析器收底。
        let system_err = match (host, port).to_socket_addrs() {
            Ok(addrs) => {
                let addrs: Vec<SocketAddr> = addrs.collect();
                if !addrs.is_empty() {
                    return Ok(addrs);
                }
                "未返回地址".to_string()
            }
            Err(e) => e.to_string(),
        };

        Err(match doh_err {
            Some(doh) => io::Error::new(
                doh.kind(),
                format!("无法解析 {host}：DoH 解析失败（{doh}）；系统解析失败（{system_err}）"),
            ),
            None => io::Error::other(format!(
                "无法解析 {host}：系统解析失败（{system_err}）；未配置 dns 端点"
            )),
        })
    }

    /// 按端点顺序查询；命中缓存直接返回。
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        if let Some(addrs) = self.cached(host, port) {
            return Ok(addrs);
        }
        let mut last_err = None;
        for endpoint in &self.endpoints {
            let (a_ips, a_ttl) = match self.query(endpoint, host, TYPE_A) {
                Ok(answer) => answer,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let mut ttl = a_ttl;
            let mut addrs: Vec<SocketAddr> = a_ips
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect();
            // IPv6-only 主机靠 AAAA 拿地址；AAAA 查询失败不影响 A 的结果。
            if let Ok((ips, t)) = self.query(endpoint, host, TYPE_AAAA) {
                ttl = ttl.min(t);
                addrs.extend(ips.into_iter().map(|ip| SocketAddr::new(ip, port)));
            }
            if addrs.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{host} 没有 A/AAAA 记录（{endpoint}）"),
                ));
            }
            self.store(host, port, &addrs, ttl);
            return Ok(addrs);
        }
        Err(last_err.unwrap_or_else(|| io::Error::other("没有可用的 dns 端点")))
    }

    /// 向一个端点发一次 DoH 查询（POST，RFC 8484）。
    fn query(&self, endpoint: &str, host: &str, qtype: u16) -> io::Result<(Vec<IpAddr>, u32)> {
        let body = build_query(host, qtype)?;
        let agent = self.agent.get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout_connect(REQUEST_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
        });
        let response = agent
            .post(endpoint)
            .set("Content-Type", "application/dns-message")
            .set("Accept", "application/dns-message")
            .send_bytes(&body)
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => io::Error::other(format!("HTTP {code}")),
                ureq::Error::Transport(t) => io::Error::other(t.to_string()),
            })?;
        let mut raw = Vec::new();
        response
            .into_reader()
            .take(MAX_BODY_BYTES)
            .read_to_end(&mut raw)
            .map_err(|e| io::Error::other(format!("读取 DoH 响应失败：{e}")))?;
        parse_answer(&raw)
    }

    fn cached(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let key = (host.to_string(), port);
        match cache.get(&key) {
            Some(entry) if entry.expires > Instant::now() => Some(entry.addrs.clone()),
            Some(_) => {
                cache.remove(&key);
                None
            }
            None => None,
        }
    }

    fn store(&self, host: &str, port: u16, addrs: &[SocketAddr], ttl: u32) {
        let ttl = ttl.clamp(MIN_TTL, MAX_TTL);
        let entry = CacheEntry {
            expires: Instant::now() + Duration::from_secs(ttl as u64),
            addrs: addrs.to_vec(),
        };
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((host.to_string(), port), entry);
    }
}

/// `host:port` / `[v6]:port` → 主机与端口。ureq 调用时总是带端口。
fn split_netloc(netloc: &str) -> Option<(&str, u16)> {
    if let Some(rest) = netloc.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        return Some((host, tail.strip_prefix(':')?.parse().ok()?));
    }
    let (host, port) = netloc.rsplit_once(':')?;
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// 构造查询报文：12 字节头（id 固定 0，RD=1）+ QNAME + QTYPE + QCLASS。
fn build_query(host: &str, qtype: u16) -> io::Result<Vec<u8>> {
    let name = host.trim_end_matches('.');
    let mut qname = Vec::with_capacity(name.len() + 2);
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("主机名无法编码为 DNS 查询：{host}"),
            ));
        }
        qname.push(label.len() as u8);
        qname.extend_from_slice(label.as_bytes());
    }
    qname.push(0);
    if qname.len() > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("主机名超过 255 字节：{host}"),
        ));
    }
    let mut out = Vec::with_capacity(12 + qname.len() + 4);
    out.extend_from_slice(&[0, 0]); // id：RFC 8484 建议置 0
    out.extend_from_slice(&[0x01, 0x00]); // RD=1
    out.extend_from_slice(&[0, 1]); // QDCOUNT
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR
    out.extend_from_slice(&qname);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(out)
}

/// 解析应答：返回 A/AAAA 地址与其中最小的 TTL。
fn parse_answer(raw: &[u8]) -> io::Result<(Vec<IpAddr>, u32)> {
    let truncated = || io::Error::new(io::ErrorKind::InvalidData, "DoH 响应不是完整的 DNS 报文");
    if raw.len() < 12 {
        return Err(truncated());
    }
    let flags = u16::from_be_bytes([raw[2], raw[3]]);
    if flags & 0x8000 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DoH 响应缺少 QR 标记",
        ));
    }
    match flags & 0x000F {
        0 => {}
        3 => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "域名不存在（RCODE=3）",
            ))
        }
        rcode => return Err(io::Error::other(format!("DoH 响应错误码 RCODE={rcode}"))),
    }
    let qdcount = u16::from_be_bytes([raw[4], raw[5]]);
    let ancount = u16::from_be_bytes([raw[6], raw[7]]);

    let mut at = 12;
    for _ in 0..qdcount {
        at = skip_name(raw, at).ok_or_else(truncated)? + 4;
    }
    let mut ips = Vec::new();
    let mut ttl = MAX_TTL;
    for _ in 0..ancount {
        at = skip_name(raw, at).ok_or_else(truncated)?;
        let head = raw.get(at..at + 10).ok_or_else(truncated)?;
        let rtype = u16::from_be_bytes([head[0], head[1]]);
        let class = u16::from_be_bytes([head[2], head[3]]);
        let record_ttl = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
        let rdlen = u16::from_be_bytes([head[8], head[9]]) as usize;
        at += 10;
        let rdata = raw.get(at..at + rdlen).ok_or_else(truncated)?;
        at += rdlen;
        if class != CLASS_IN {
            continue;
        }
        match (rtype, rdlen) {
            (TYPE_A, 4) => ips.push(IpAddr::from([rdata[0], rdata[1], rdata[2], rdata[3]])),
            (TYPE_AAAA, 16) => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(rdata);
                ips.push(IpAddr::V6(Ipv6Addr::from(octets)));
            }
            _ => continue,
        }
        ttl = ttl.min(record_ttl);
    }
    Ok((ips, ttl))
}

/// 跳过一个（可能带压缩指针的）域名，返回其后第一个字节的下标。
fn skip_name(raw: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *raw.get(at)? as usize;
        match len & 0xC0 {
            // 压缩指针：本函数只做跳过，不跟随。
            0xC0 if raw.get(at + 1).is_some() => return Some(at + 2),
            0x00 if len == 0 => return Some(at + 1),
            0x00 => at += 1 + len,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::thread;

    /// 假 DoH 服务：按请求里的 QTYPE 应答；`max_requests` 用完后不再接受连接。
    fn spawn_doh<F>(max_requests: usize, respond: F) -> String
    where
        F: Fn(&[u8]) -> (u16, Vec<u8>) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut left = max_requests;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                while left > 0 {
                    let Some(body) = read_http_request(&mut stream) else {
                        break;
                    };
                    left -= 1;
                    let (status, payload) = respond(&body);
                    let _ = write_http_response(&mut stream, status, &payload);
                }
                if left == 0 {
                    return;
                }
            }
        });
        format!("http://{addr}/dns-query")
    }

    fn read_http_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            if let Some(end) = find(&buf, b"\r\n\r\n") {
                let len = content_length(&buf[..end])?;
                if buf.len() >= end + 4 + len {
                    return Some(buf[end + 4..end + 4 + len].to_vec());
                }
            }
            let n = stream.read(&mut tmp).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn content_length(head: &[u8]) -> Option<usize> {
        let text = String::from_utf8_lossy(head);
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, body: &[u8]) -> io::Result<()> {
        let reason = if status == 200 { "OK" } else { "Error" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()
    }

    /// 按 QTYPE 返回一条记录：A → 93.184.216.34，AAAA → 2606:2800:220:1:248:1893:25c8:1946。
    fn answer(body: &[u8]) -> (u16, Vec<u8>) {
        let qtype = qtype_of(body);
        let rdata = match qtype {
            TYPE_AAAA => Ipv6Addr::new(0x2606, 0x2800, 0x220, 0x1, 0x248, 0x1893, 0x25c8, 0x1946)
                .octets()
                .to_vec(),
            _ => Ipv4Addr::new(93, 184, 216, 34).octets().to_vec(),
        };
        (200, packet(&body[12..], 0, &[(qtype, rdata, 60)]))
    }

    fn qtype_of(body: &[u8]) -> u16 {
        u16::from_be_bytes([body[body.len() - 4], body[body.len() - 3]])
    }

    /// 用请求里的 question 段拼应答；answer 名字用指向 offset 12 的压缩指针。
    fn packet(question: &[u8], rcode: u8, records: &[(u16, Vec<u8>, u32)]) -> Vec<u8> {
        let mut out = vec![0, 0, 0x81, 0x80 | rcode];
        out.extend_from_slice(&[0, 1]);
        out.extend_from_slice(&(records.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(question);
        for (rtype, rdata, ttl) in records {
            out.extend_from_slice(&[0xC0, 0x0C]);
            out.extend_from_slice(&rtype.to_be_bytes());
            out.extend_from_slice(&CLASS_IN.to_be_bytes());
            out.extend_from_slice(&ttl.to_be_bytes());
            out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            out.extend_from_slice(rdata);
        }
        out
    }

    const V4: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
    const V6: Ipv6Addr = Ipv6Addr::new(0x2606, 0x2800, 0x220, 0x1, 0x248, 0x1893, 0x25c8, 0x1946);

    #[test]
    fn split_netloc_handles_ports_and_ipv6() {
        assert_eq!(
            split_netloc("api.deepseek.com:443"),
            Some(("api.deepseek.com", 443))
        );
        assert_eq!(
            split_netloc("[2400:3200::1]:443"),
            Some(("2400:3200::1", 443))
        );
        assert_eq!(split_netloc("223.5.5.5:53"), Some(("223.5.5.5", 53)));
        assert_eq!(split_netloc("api.deepseek.com"), None);
        assert_eq!(split_netloc("2400:3200::1:443"), None);
        assert_eq!(split_netloc("[2400:3200::1]"), None);
    }

    #[test]
    fn resolve_returns_ip_literal_directly() {
        let resolver = Resolver::new(Vec::new());
        assert_eq!(
            resolver.resolve("223.5.5.5:443").unwrap(),
            vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(223, 5, 5, 5)),
                443
            )]
        );
    }

    #[test]
    fn resolve_prefers_configured_doh() {
        // localhost 系统也能解析；配置端点后以 DoH 结果为准。
        let url = spawn_doh(2, answer);
        let resolver = Resolver::new(vec![url]);
        assert_eq!(
            resolver.resolve("localhost:443").unwrap(),
            vec![
                SocketAddr::new(IpAddr::V4(V4), 443),
                SocketAddr::new(IpAddr::V6(V6), 443),
            ]
        );
    }

    #[test]
    fn resolve_falls_back_to_system_when_doh_fails() {
        let failing = spawn_doh(1, |_| (502, Vec::new()));
        let resolver = Resolver::new(vec![failing]);
        let addrs = resolver.resolve("localhost:443").unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback()), "{addrs:?}");
    }

    #[test]
    fn build_query_encodes_question() {
        let query = build_query("api.deepseek.com", TYPE_A).unwrap();
        let mut expected = vec![0, 0, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in ["api", "deepseek", "com"] {
            expected.push(label.len() as u8);
            expected.extend_from_slice(label.as_bytes());
        }
        expected.push(0);
        expected.extend_from_slice(&[0, 1, 0, 1]);
        assert_eq!(query, expected);
        // 结尾的点按绝对域名处理，不产生空标签。
        assert_eq!(build_query("api.deepseek.com.", TYPE_A).unwrap(), expected);
    }

    #[test]
    fn build_query_rejects_bad_labels() {
        assert!(build_query(&"a".repeat(64), TYPE_A).is_err());
        assert!(build_query("a..b", TYPE_A).is_err());
    }

    #[test]
    fn parse_answer_reads_records_and_skips_others() {
        let query = build_query("www.example.com", TYPE_A).unwrap();
        let cname = b"\x02cn\x07example\x03com\x00".to_vec();
        let body = packet(
            &query[12..],
            0,
            &[(5, cname, 300), (TYPE_A, V4.octets().to_vec(), 60)],
        );
        let (ips, ttl) = parse_answer(&body).unwrap();
        assert_eq!(ips, vec![IpAddr::V4(V4)]);
        assert_eq!(ttl, 60);
    }

    #[test]
    fn parse_answer_reports_nxdomain_and_truncation() {
        let query = build_query("nx.example.com", TYPE_A).unwrap();
        let nxdomain = packet(&query[12..], 3, &[]);
        assert_eq!(
            parse_answer(&nxdomain).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            parse_answer(&nxdomain[..10]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn lookup_queries_local_doh_server() {
        let url = spawn_doh(2, answer);
        let resolver = Resolver::new(vec![url]);
        let addrs = resolver.lookup("api.example.com", 443).unwrap();
        assert_eq!(
            addrs,
            vec![
                SocketAddr::new(IpAddr::V4(V4), 443),
                SocketAddr::new(IpAddr::V6(V6), 443),
            ]
        );
    }

    #[test]
    fn lookup_uses_aaaa_when_a_has_no_records() {
        // A 返回 NOERROR/NODATA，只有 AAAA 有记录。
        let url = spawn_doh(2, |body| match qtype_of(body) {
            TYPE_AAAA => (
                200,
                packet(&body[12..], 0, &[(TYPE_AAAA, V6.octets().to_vec(), 60)]),
            ),
            _ => (200, packet(&body[12..], 0, &[])),
        });
        let resolver = Resolver::new(vec![url]);
        assert_eq!(
            resolver.lookup("v6.example.com", 443).unwrap(),
            vec![SocketAddr::new(IpAddr::V6(V6), 443)]
        );
    }

    #[test]
    fn lookup_caches_until_ttl_expires() {
        // 只服务一次 lookup 的两个请求（A + AAAA）；第二次必须命中缓存。
        let url = spawn_doh(2, answer);
        let resolver = Resolver::new(vec![url]);
        let first = resolver.lookup("cached.example.com", 443).unwrap();
        let second = resolver.lookup("cached.example.com", 443).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn lookup_falls_over_to_next_endpoint() {
        let failing = spawn_doh(1, |_| (502, Vec::new()));
        let working = spawn_doh(2, answer);
        let resolver = Resolver::new(vec![failing, working]);
        assert!(!resolver
            .lookup("failover.example.com", 443)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn lookup_reports_http_status() {
        let url = spawn_doh(1, |_| (502, Vec::new()));
        let resolver = Resolver::new(vec![url]);
        let err = resolver.lookup("status.example.com", 443).unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
    }

    #[test]
    fn lookup_reports_nxdomain() {
        let url = spawn_doh(1, |body| (200, packet(&body[12..], 3, &[])));
        let resolver = Resolver::new(vec![url]);
        let err = resolver.lookup("nx.example.com", 443).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
