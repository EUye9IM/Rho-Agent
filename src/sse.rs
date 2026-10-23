//! SSE 行解析器（纯函数，可夹具测试）。见 design.md 六节。

/// 有状态的 SSE 组装器：跨网络分片累积行，产出完整 `data:` 载荷。
#[derive(Debug, Default)]
pub struct SseParser {
    /// 行缓冲（TCP 分片可能把一行拆多次 read）
    buf: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段原始字节（可能是半个 UTF-8 字符/半行）。
    /// 返回本次新完成的 `data:` 载荷列表（注释/空行/非 data 行忽略；CRLF 兼容）。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        // TODO: 追加到 buf（注意 UTF-8 边界）；按 \n 切行；行尾 \r 剥离；
        //       "data: X" / "data:X" → 收集；"data: [DONE]" 作为普通载荷返回由上层识别；
        //       多条 data: 属同一事件时拼接（OpenAI 每事件单条 data，可简化）
        let _ = bytes;
        todo!()
    }
}

/// 单行解析：`data: ` 前缀剥离。None 表示非 data 行。
pub fn parse_line(line: &str) -> Option<String> {
    // TODO: 兼容 "data:" 与 "data: " 两种前缀
    let _ = line;
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_data_line() {
        assert_eq!(parse_line("data: {\"a\":1}").as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn split_across_feeds() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"a\"").is_empty());
        let out = p.feed(b":1}\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
    }
}
