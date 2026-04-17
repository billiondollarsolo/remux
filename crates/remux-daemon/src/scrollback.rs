use std::collections::VecDeque;

/// Ring buffer for storing scrollback lines from PTY output.
pub struct ScrollbackBuffer {
    buffer: VecDeque<Vec<u8>>,
    max_lines: usize,
}

impl ScrollbackBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(max_lines.min(1024)),
            max_lines,
        }
    }

    /// Add a line to the buffer. Evicts the oldest line if at capacity.
    pub fn push(&mut self, data: Vec<u8>) {
        if self.buffer.len() >= self.max_lines {
            self.buffer.pop_front();
        }
        self.buffer.push_back(data);
    }

    /// Append raw output bytes, splitting on newline boundaries.
    /// Partial lines are accumulated until a newline is seen.
    pub fn append_bytes(&mut self, data: &[u8], partial: &mut Vec<u8>) {
        partial.extend_from_slice(data);
        while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
            let mut line = partial.split_off(pos + 1);
            std::mem::swap(&mut line, partial);
            // line now contains everything before and including the newline
            // Remove the trailing newline
            line.pop();
            // Remove trailing \r if present (CRLF line endings)
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.push(line);
        }
    }

    /// Get the last N lines from the buffer.
    pub fn read_last(&self, lines: usize) -> Vec<Vec<u8>> {
        self.buffer
            .iter()
            .rev()
            .take(lines)
            .rev()
            .cloned()
            .collect()
    }

    /// Get all lines from the buffer.
    #[allow(dead_code)]
    pub fn read_all(&self) -> Vec<Vec<u8>> {
        self.buffer.iter().cloned().collect()
    }

    /// Get all lines as a single concatenated byte vector.
    pub fn read_all_bytes(&self) -> Vec<u8> {
        let mut result = Vec::new();
        for line in &self.buffer {
            result.extend_from_slice(line);
            result.push(b'\n');
        }
        result
    }

    /// Clear the buffer.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Number of lines currently in the buffer.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_read_basic() {
        let mut buf = ScrollbackBuffer::new(100);
        buf.push(b"line1".to_vec());
        buf.push(b"line2".to_vec());
        buf.push(b"line3".to_vec());

        let all = buf.read_all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], b"line1");
        assert_eq!(all[1], b"line2");
        assert_eq!(all[2], b"line3");
    }

    #[test]
    fn read_last_limits_output() {
        let mut buf = ScrollbackBuffer::new(100);
        buf.push(b"line1".to_vec());
        buf.push(b"line2".to_vec());
        buf.push(b"line3".to_vec());

        let last2 = buf.read_last(2);
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0], b"line2");
        assert_eq!(last2[1], b"line3");
    }

    #[test]
    fn eviction_at_capacity() {
        let mut buf = ScrollbackBuffer::new(3);
        buf.push(b"line1".to_vec());
        buf.push(b"line2".to_vec());
        buf.push(b"line3".to_vec());
        buf.push(b"line4".to_vec());

        assert_eq!(buf.len(), 3);
        let all = buf.read_all();
        assert_eq!(all[0], b"line2");
        assert_eq!(all[1], b"line3");
        assert_eq!(all[2], b"line4");
    }

    #[test]
    fn append_bytes_splits_on_newline() {
        let mut buf = ScrollbackBuffer::new(100);
        let mut partial = Vec::new();

        buf.append_bytes(b"hello\nworld\n", &mut partial);

        assert_eq!(buf.len(), 2);
        let all = buf.read_all();
        assert_eq!(all[0], b"hello");
        assert_eq!(all[1], b"world");
        assert!(partial.is_empty());
    }

    #[test]
    fn append_bytes_handles_partial_lines() {
        let mut buf = ScrollbackBuffer::new(100);
        let mut partial = Vec::new();

        buf.append_bytes(b"hel", &mut partial);
        assert_eq!(buf.len(), 0);

        buf.append_bytes(b"lo\n", &mut partial);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.read_all()[0], b"hello");
    }

    #[test]
    fn append_bytes_strips_cr() {
        let mut buf = ScrollbackBuffer::new(100);
        let mut partial = Vec::new();

        buf.append_bytes(b"line1\r\nline2\r\n", &mut partial);

        assert_eq!(buf.len(), 2);
        let all = buf.read_all();
        assert_eq!(all[0], b"line1");
        assert_eq!(all[1], b"line2");
    }

    #[test]
    fn clear_empties_buffer() {
        let mut buf = ScrollbackBuffer::new(100);
        buf.push(b"line1".to_vec());
        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn read_all_bytes_concatenates() {
        let mut buf = ScrollbackBuffer::new(100);
        buf.push(b"hello".to_vec());
        buf.push(b"world".to_vec());

        let bytes = buf.read_all_bytes();
        assert_eq!(bytes, b"hello\nworld\n");
    }
}
