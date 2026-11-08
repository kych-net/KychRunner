//! 日志打码与分片。任何来自 claim.secrets 的值都不会以明文回报给主服务。

use std::collections::BTreeMap;

pub struct Redactor {
    values: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: &BTreeMap<String, String>) -> Self {
        let mut values: Vec<String> = secrets
            .values()
            .map(|v| v.trim().to_string())
            // 太短的值会把日志打成一片 ***，只保留有区分度的。
            .filter(|v| v.len() >= 4)
            .collect();
        values.sort_by_key(|b| std::cmp::Reverse(b.len()));
        values.dedup();
        Self { values }
    }

    pub fn redact(&self, line: &str) -> String {
        let mut out = line.to_string();
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), "***");
            }
        }
        out
    }
}

/// 按行数或字节数攒批，减少 /logs 请求次数。
pub struct LogBatcher {
    buf: Vec<String>,
    bytes: usize,
    max_bytes: usize,
    max_lines: usize,
}

impl LogBatcher {
    pub fn new(max_bytes: usize, max_lines: usize, _total_limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            bytes: 0,
            max_bytes: max_bytes.max(1),
            max_lines: max_lines.max(1),
        }
    }

    pub fn push(&mut self, line: String) -> Option<Vec<String>> {
        self.bytes += line.len() + 1;
        self.buf.push(line);
        if self.bytes >= self.max_bytes || self.buf.len() >= self.max_lines {
            return Some(self.take());
        }
        None
    }

    pub fn take(&mut self) -> Vec<String> {
        self.bytes = 0;
        std::mem::take(&mut self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_values() {
        let mut s = BTreeMap::new();
        s.insert("TOKEN".into(), "super-secret-value".into());
        s.insert("EMPTY".into(), "x".into());
        let r = Redactor::new(&s);
        assert_eq!(r.redact("using super-secret-value now"), "using *** now");
        assert_eq!(r.redact("nothing here"), "nothing here");
    }

    #[test]
    fn batcher_flushes_on_size_and_lines() {
        let mut b = LogBatcher::new(10, 3, 1000);
        assert!(b.push("a".into()).is_none());
        assert!(b.push("b".into()).is_none());
        let chunk = b.push("c".into()).unwrap();
        assert_eq!(chunk, vec!["a", "b", "c"]);
        assert!(b.take().is_empty());

        let mut b2 = LogBatcher::new(4, 100, 1000);
        assert_eq!(b2.push("abcdefgh".into()).unwrap().len(), 1);
    }

    #[test]
    fn batches_by_byte_threshold() {
        let mut b = LogBatcher::new(6, 100, 1000);
        assert!(b.push("ab".into()).is_none());
        let chunk = b.push("cdef".into()).unwrap();
        assert_eq!(chunk.len(), 2);
    }
}
