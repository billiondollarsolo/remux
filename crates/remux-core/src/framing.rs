use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::RemuxError;

/// Read a framed message from an async stream.
/// Dev: newline-delimited JSON. Release: 4-byte LE length prefix + bincode.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncReadExt + Unpin),
    line_buf: &mut Vec<u8>,
) -> Result<Option<T>, RemuxError> {
    #[cfg(debug_assertions)]
    {
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte).await {
                Ok(0) => {
                    if line_buf.is_empty() {
                        return Ok(None);
                    }
                    let line = String::from_utf8_lossy(line_buf);
                    let msg: T = serde_json::from_str(line.trim())
                        .map_err(|e| RemuxError::ProtocolError(format!("json parse: {e}")))?;
                    line_buf.clear();
                    return Ok(Some(msg));
                }
                Ok(_) => {
                    for &b in &byte {
                        if b == b'\n' {
                            if line_buf.is_empty() {
                                continue;
                            }
                            let line = String::from_utf8_lossy(line_buf);
                            let msg: T = serde_json::from_str(line.trim()).map_err(|e| {
                                RemuxError::ProtocolError(format!("json parse: {e}"))
                            })?;
                            line_buf.clear();
                            return Ok(Some(msg));
                        }
                        line_buf.push(b);
                    }
                }
                Err(e) => {
                    return Err(RemuxError::IoError(format!("read error: {e}")));
                }
            }
        }
    }

    #[cfg(not(debug_assertions))]
    {
        let _ = line_buf;
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(RemuxError::IoError(format!("read length: {e}"))),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            return Err(RemuxError::ProtocolError(format!(
                "message too large: {len} bytes"
            )));
        }
        let mut data = vec![0u8; len];
        reader
            .read_exact(&mut data)
            .await
            .map_err(|e| RemuxError::IoError(format!("read payload: {e}")))?;
        let msg: T = bincode::deserialize(&data)
            .map_err(|e| RemuxError::ProtocolError(format!("bincode deserialize: {e}")))?;
        Ok(Some(msg))
    }
}

/// Write a framed message to an async stream.
/// Dev: newline-delimited JSON. Release: 4-byte LE length prefix + bincode.
pub async fn write_message<T: serde::Serialize>(
    writer: &mut (impl AsyncWriteExt + Unpin),
    msg: &T,
) -> Result<(), RemuxError> {
    #[cfg(debug_assertions)]
    {
        let mut json = serde_json::to_string(msg)
            .map_err(|e| RemuxError::ProtocolError(format!("json serialize: {e}")))?;
        json.push('\n');
        writer
            .write_all(json.as_bytes())
            .await
            .map_err(|e| RemuxError::IoError(format!("write error: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| RemuxError::IoError(format!("flush error: {e}")))?;
    }

    #[cfg(not(debug_assertions))]
    {
        let payload = bincode::serialize(msg)
            .map_err(|e| RemuxError::ProtocolError(format!("bincode serialize: {e}")))?;
        let len = (payload.len() as u32).to_le_bytes();
        writer
            .write_all(&len)
            .await
            .map_err(|e| RemuxError::IoError(format!("write length error: {e}")))?;
        writer
            .write_all(&payload)
            .await
            .map_err(|e| RemuxError::IoError(format!("write payload error: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| RemuxError::IoError(format!("flush error: {e}")))?;
    }

    Ok(())
}

/// Serialize a message to bytes using the current framing.
/// Dev: newline-delimited JSON. Release: 4-byte LE length prefix + bincode.
pub fn serialize_to_bytes<T: serde::Serialize>(msg: &T) -> Result<Vec<u8>, RemuxError> {
    #[cfg(debug_assertions)]
    {
        let mut json = serde_json::to_vec(msg)
            .map_err(|e| RemuxError::ProtocolError(format!("json serialize: {e}")))?;
        json.push(b'\n');
        Ok(json)
    }

    #[cfg(not(debug_assertions))]
    {
        let data = bincode::serialize(msg)
            .map_err(|e| RemuxError::ProtocolError(format!("bincode serialize: {e}")))?;
        let mut buf = Vec::with_capacity(4 + data.len());
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&data);
        Ok(buf)
    }
}

/// Read a framed message from a blocking reader (for spawn_blocking contexts).
pub fn read_message_blocking<T: serde::de::DeserializeOwned>(
    reader: &mut impl std::io::Read,
    line_buf: &mut Vec<u8>,
) -> Result<Option<T>, RemuxError> {
    #[cfg(debug_assertions)]
    {
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => {
                    if line_buf.is_empty() {
                        return Ok(None);
                    }
                    let line = String::from_utf8_lossy(line_buf);
                    let msg: T = serde_json::from_str(line.trim())
                        .map_err(|e| RemuxError::ProtocolError(format!("json parse: {e}")))?;
                    line_buf.clear();
                    return Ok(Some(msg));
                }
                Ok(_) => {
                    for &b in &byte {
                        if b == b'\n' {
                            if line_buf.is_empty() {
                                continue;
                            }
                            let line = String::from_utf8_lossy(line_buf);
                            let msg: T = serde_json::from_str(line.trim()).map_err(|e| {
                                RemuxError::ProtocolError(format!("json parse: {e}"))
                            })?;
                            line_buf.clear();
                            return Ok(Some(msg));
                        }
                        line_buf.push(b);
                    }
                }
                Err(e) => {
                    return Err(RemuxError::IoError(format!("read error: {e}")));
                }
            }
        }
    }

    #[cfg(not(debug_assertions))]
    {
        let _ = line_buf;
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(RemuxError::IoError(format!("read length: {e}"))),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            return Err(RemuxError::ProtocolError(format!(
                "message too large: {len} bytes"
            )));
        }
        let mut data = vec![0u8; len];
        reader
            .read_exact(&mut data)
            .map_err(|e| RemuxError::IoError(format!("read payload: {e}")))?;
        let msg: T = bincode::deserialize(&data)
            .map_err(|e| RemuxError::ProtocolError(format!("bincode deserialize: {e}")))?;
        Ok(Some(msg))
    }
}
