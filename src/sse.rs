//! SSE 行解析器（纯函数，可夹具测试）。见 docs/architecture.md 二、四节。

/// 单行的字节上限（不含结尾换行符；完整行与尚未遇到换行的残留缓冲同样适用）。
/// 超出按协议错误收尾，避免异常上游用超长行把内存和 CPU 拖垮。
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// 有状态的 SSE 组装器：跨网络分片累积行，产出完整 `data:` 载荷。
/// 字节级缓冲（Vec<u8>）：TCP 分片可能落在 UTF-8 码点/行中间。
#[derive(Debug)]
pub struct SseParser {
    /// 行缓冲（原始字节；只在完整行边界做 UTF-8 转换）
    buf: Vec<u8>,
    /// 未终结行的字节上限（完整行与残留同样适用）
    max_line_bytes: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self { buf: Vec::new(), max_line_bytes: MAX_LINE_BYTES }
    }

    /// 喂入一段原始字节（可能是半个 UTF-8 字符/半行）。
    /// 返回本次新完成的 `data:` 载荷列表（注释/空行/非 data 行忽略；CRLF 兼容）。
    /// 任一单行（完整或未终结）超过 [`MAX_LINE_BYTES`] 时返回错误（缓冲同时清空）。
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(rel) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            // 长度检查用未剥离 CR 的原始行：换行随最后一片到达时，
            // 残留缓冲里的字节（含 CR）同样计入，两种分片方式判定一致。
            if end - start > self.max_line_bytes {
                let len = end - start;
                self.buf.clear();
                return Err(format!(
                    "SSE 单行超过上限（{len} 字节，上限 {} 字节）",
                    self.max_line_bytes
                ));
            }
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
        if self.buf.len() > self.max_line_bytes {
            let len = self.buf.len();
            self.buf.clear();
            return Err(format!(
                "SSE 单行超过上限（{len} 字节，上限 {} 字节）",
                self.max_line_bytes
            ));
        }
        Ok(out)
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
        assert!(p.feed(b"data: {\"a\"").unwrap().is_empty());
        let out = p.feed(b":1}\n\n").unwrap();
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn crlf_and_multiple_lines() {
        let mut p = SseParser::new();
        let out = p.feed(b"data: a\r\ndata: b\r\n\r\n").unwrap();
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn finish_flushes_last_line_without_newline() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: tail").unwrap().is_empty());
        assert_eq!(p.finish(), vec!["tail".to_string()]);
        assert!(p.finish().is_empty());
    }

    #[test]
    fn utf8_split_is_lossless_across_lines() {
        let mut p = SseParser::new();
        let bytes = "data: 你好\n".as_bytes();
        let (a, b) = bytes.split_at(8); // 落在多字节字符中间
        assert!(p.feed(a).unwrap().is_empty());
        let out = p.feed(b).unwrap();
        assert_eq!(out, vec!["你好".to_string()]);
    }

    #[test]
    fn overlong_unterminated_line_is_error() {
        let mut p = SseParser::new();
        let chunk = vec![b'x'; MAX_LINE_BYTES];
        assert!(p.feed(&chunk).unwrap().is_empty());
        let err = p.feed(b"x").unwrap_err();
        assert!(err.contains("单行超过上限"), "error was {err}");
        // 清空后可继续使用。
        assert_eq!(p.feed(b"data: ok\n").unwrap(), vec!["ok".to_string()]);
    }

    #[test]
    fn overlong_terminated_line_in_same_batch_is_error() {
        let mut p = SseParser::new();
        let mut line = b"data: ".to_vec();
        line.extend(std::iter::repeat(b'y').take(MAX_LINE_BYTES));
        line.push(b'\n');
        let err = p.feed(&line).unwrap_err();
        assert!(err.contains("单行超过上限"), "error was {err}");
        // 清空后可继续使用。
        assert_eq!(p.feed(b"data: ok\n").unwrap(), vec!["ok".to_string()]);
    }

    #[test]
    fn overlong_line_error_is_chunking_independent() {
        // 同一输入：换行随最后一片到达时由残留检查报错，整批到达时由完整行检查报错。
        let mut line = b"data: ".to_vec();
        line.extend(std::iter::repeat(b'y').take(MAX_LINE_BYTES));
        let (a, b) = line.split_at(line.len() / 2);
        let mut p = SseParser::new();
        assert!(p.feed(a).unwrap().is_empty());
        assert!(p.feed(b).is_err());

        let mut whole = line.clone();
        whole.push(b'\n');
        assert!(SseParser::new().feed(&whole).is_err());
    }

    #[test]
    fn overlong_crlf_line_is_error() {
        let mut p = SseParser::new();
        let mut line = b"data: ".to_vec();
        line.extend(std::iter::repeat(b'y').take(MAX_LINE_BYTES));
        line.extend_from_slice(b"\r\n");
        assert!(p.feed(&line).is_err());
    }

    #[test]
    fn line_at_exactly_limit_with_newline_is_accepted() {
        let mut p = SseParser::new();
        let mut line = b"data: ".to_vec();
        line.extend(std::iter::repeat(b'y').take(MAX_LINE_BYTES - 6));
        line.push(b'\n');
        let out = p.feed(&line).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), MAX_LINE_BYTES - 6);
    }

    #[test]
    fn long_line_within_limit_is_accepted() {
        let mut p = SseParser::new();
        let mut line = b"data: ".to_vec();
        line.extend(std::iter::repeat(b'y').take(MAX_LINE_BYTES - 16));
        line.push(b'\n');
        let out = p.feed(&line).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), MAX_LINE_BYTES - 16);
    }
}
