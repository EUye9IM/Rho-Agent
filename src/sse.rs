//! SSE 行解析器（纯函数，可夹具测试）。见 docs/design.md 六节。

/// 有状态的 SSE 组装器：跨网络分片累积行，产出完整 `data:` 载荷。
/// 字节级缓冲（Vec<u8>）：TCP 分片可能落在 UTF-8 码点/行中间。
#[derive(Debug, Default)]
pub struct SseParser {
    /// 行缓冲（原始字节；只在完整行边界做 UTF-8 转换）
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段原始字节（可能是半个 UTF-8 字符/半行）。
    /// 返回本次新完成的 `data:` 载荷列表（注释/空行/非 data 行忽略；CRLF 兼容）。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(rel) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            let mut line = &self.buf[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let text = String::from_utf8_lossy(line);
            if let Some(payload) = parse_line(&text) {
                out.push(payload);
            }
            start = end + 1;
        }
        if start > 0 {
            self.buf.drain(..start);
        }
        out
    }

    /// EOF：把缓冲中未以换行结尾的最后一行也产出。
    pub fn finish(&mut self) -> Vec<String> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let mut line = self.buf.as_slice();
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let text = String::from_utf8_lossy(line);
        let out = parse_line(&text).into_iter().collect();
        self.buf.clear();
        out
    }
}

/// 单行解析：`data: ` 前缀剥离。None 表示非 data 行。
pub fn parse_line(line: &str) -> Option<String> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let rest = line.strip_prefix("data:")?;
    // 规范里冒号后可有一个可选空格；`data:X` 与 `data: X` 都接受。
    Some(rest.strip_prefix(' ').unwrap_or(rest).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_data_line() {
        assert_eq!(parse_line("data: {\"a\":1}").as_deref(), Some("{\"a\":1}"));
        assert_eq!(parse_line("data:{\"a\":1}").as_deref(), Some("{\"a\":1}"));
        assert_eq!(parse_line(": keep-alive"), None);
    }

    #[test]
    fn split_across_feeds() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"a\"").is_empty());
        let out = p.feed(b":1}\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn crlf_and_multiple_lines() {
        let mut p = SseParser::new();
        let out = p.feed(b"data: a\r\ndata: b\r\n\r\n");
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn finish_flushes_last_line_without_newline() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: tail").is_empty());
        assert_eq!(p.finish(), vec!["tail".to_string()]);
        assert!(p.finish().is_empty());
    }

    #[test]
    fn utf8_split_is_lossless_across_lines() {
        let mut p = SseParser::new();
        let bytes = "data: 你好\n".as_bytes();
        let (a, b) = bytes.split_at(8); // 落在多字节字符中间
        assert!(p.feed(a).is_empty());
        let out = p.feed(b);
        assert_eq!(out, vec!["你好".to_string()]);
    }
}
