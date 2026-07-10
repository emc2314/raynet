use crate::{CloseReason, Metadata, Target};

const ENVELOPE_MAGIC: &[u8; 4] = b"RNE1";
const SESSION_FRAME_MAGIC: &[u8; 4] = b"RNF1";
const MAX_ENVELOPE_SIZE: usize = 64 * 1024;
const MAX_SESSION_FRAME_SIZE: usize = 64 * 1024;
const MAX_ROUTE_PLAN_LEN: usize = 64;
const MAX_METADATA_ITEMS: usize = 64;
const MAX_STRING_LEN: usize = 4096;

pub type RoutePlan = Vec<crate::ChannelId>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub route_plan: RoutePlan,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFrame {
    OpenConnection { target: Target, metadata: Metadata },
    ConnectionBytes { bytes: Vec<u8> },
    CloseConnection { reason: CloseReason },
    ResetConnection { reason: CloseReason },
    KeepAlive,
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("buffer is too short")]
    ShortBuffer,
    #[error("buffer is too large")]
    TooLarge,
    #[error("invalid magic")]
    InvalidMagic,
    #[error("invalid frame type {0}")]
    InvalidFrameType(u8),
    #[error("declared length exceeds buffer")]
    LengthOutOfBounds,
    #[error("field count exceeds limit")]
    CountOutOfBounds,
    #[error("string is not valid UTF-8")]
    InvalidUtf8,
}

impl Envelope {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        if self.route_plan.len() > MAX_ROUTE_PLAN_LEN {
            return Err(WireError::CountOutOfBounds);
        }
        if self.payload.len() > MAX_ENVELOPE_SIZE {
            return Err(WireError::TooLarge);
        }

        let mut out = Vec::with_capacity(10 + self.route_plan.len() * 8 + self.payload.len());
        out.extend_from_slice(ENVELOPE_MAGIC);
        put_u16(&mut out, self.route_plan.len() as u16);
        put_u32(&mut out, self.payload.len() as u32);
        for channel_id in &self.route_plan {
            put_u64(&mut out, *channel_id);
        }
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() > MAX_ENVELOPE_SIZE {
            return Err(WireError::TooLarge);
        }

        let mut cursor = Cursor::new(bytes);
        if cursor.take(4)? != ENVELOPE_MAGIC {
            return Err(WireError::InvalidMagic);
        }
        let route_plan_len = cursor.u16()? as usize;
        let payload_len = cursor.u32()? as usize;

        if route_plan_len > MAX_ROUTE_PLAN_LEN {
            return Err(WireError::CountOutOfBounds);
        }

        let mut route_plan = Vec::with_capacity(route_plan_len);
        for _ in 0..route_plan_len {
            route_plan.push(cursor.u64()?);
        }

        let payload = cursor.take(payload_len)?.to_vec();
        cursor.finish()?;

        Ok(Self {
            route_plan,
            payload,
        })
    }
}

impl SessionFrame {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut out = Vec::new();
        out.extend_from_slice(SESSION_FRAME_MAGIC);

        match self {
            SessionFrame::OpenConnection { target, metadata } => {
                out.push(0);
                put_string(&mut out, &target.host)?;
                put_u16(&mut out, target.port);
                put_metadata(&mut out, metadata)?;
            }
            SessionFrame::ConnectionBytes { bytes } => {
                out.push(1);
                put_bytes(&mut out, bytes)?;
            }
            SessionFrame::CloseConnection { reason } => {
                out.push(2);
                put_close_reason(&mut out, reason)?;
            }
            SessionFrame::ResetConnection { reason } => {
                out.push(3);
                put_close_reason(&mut out, reason)?;
            }
            SessionFrame::KeepAlive => out.push(4),
        }

        if out.len() > MAX_SESSION_FRAME_SIZE {
            return Err(WireError::TooLarge);
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() > MAX_SESSION_FRAME_SIZE {
            return Err(WireError::TooLarge);
        }

        let mut cursor = Cursor::new(bytes);
        if cursor.take(4)? != SESSION_FRAME_MAGIC {
            return Err(WireError::InvalidMagic);
        }

        let frame = match cursor.u8()? {
            0 => {
                let host = cursor.string()?;
                let port = cursor.u16()?;
                let metadata = cursor.metadata()?;
                SessionFrame::OpenConnection {
                    target: Target { host, port },
                    metadata,
                }
            }
            1 => SessionFrame::ConnectionBytes {
                bytes: cursor.bytes()?,
            },
            2 => SessionFrame::CloseConnection {
                reason: cursor.close_reason()?,
            },
            3 => SessionFrame::ResetConnection {
                reason: cursor.close_reason()?,
            },
            4 => SessionFrame::KeepAlive,
            value => return Err(WireError::InvalidFrameType(value)),
        };
        cursor.finish()?;
        Ok(frame)
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn finish(&self) -> Result<(), WireError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(WireError::LengthOutOfBounds)
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(WireError::LengthOutOfBounds)?;
        if end > self.bytes.len() {
            return Err(WireError::ShortBuffer);
        }
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| WireError::ShortBuffer)?,
        ))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| WireError::ShortBuffer)?,
        ))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| WireError::ShortBuffer)?,
        ))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, WireError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn string(&mut self) -> Result<String, WireError> {
        let bytes = self.bytes()?;
        if bytes.len() > MAX_STRING_LEN {
            return Err(WireError::TooLarge);
        }
        String::from_utf8(bytes).map_err(|_| WireError::InvalidUtf8)
    }

    fn metadata(&mut self) -> Result<Metadata, WireError> {
        let count = self.u16()? as usize;
        if count > MAX_METADATA_ITEMS {
            return Err(WireError::CountOutOfBounds);
        }
        let mut metadata = Metadata::new();
        for _ in 0..count {
            metadata.insert(self.string()?, self.string()?);
        }
        Ok(metadata)
    }

    fn close_reason(&mut self) -> Result<CloseReason, WireError> {
        Ok(match self.u8()? {
            0 => CloseReason::LocalClosed,
            1 => CloseReason::RemoteClosed,
            2 => CloseReason::Reset,
            _ => CloseReason::Error(self.string()?),
        })
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), WireError> {
    if bytes.len() > u32::MAX as usize {
        return Err(WireError::TooLarge);
    }
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_string(out: &mut Vec<u8>, value: &str) -> Result<(), WireError> {
    if value.len() > MAX_STRING_LEN {
        return Err(WireError::TooLarge);
    }
    put_bytes(out, value.as_bytes())
}

fn put_metadata(out: &mut Vec<u8>, metadata: &Metadata) -> Result<(), WireError> {
    if metadata.len() > MAX_METADATA_ITEMS {
        return Err(WireError::CountOutOfBounds);
    }
    put_u16(out, metadata.len() as u16);
    for (key, value) in metadata {
        put_string(out, key)?;
        put_string(out, value)?;
    }
    Ok(())
}

fn put_close_reason(out: &mut Vec<u8>, reason: &CloseReason) -> Result<(), WireError> {
    match reason {
        CloseReason::LocalClosed => out.push(0),
        CloseReason::RemoteClosed => out.push(1),
        CloseReason::Reset => out.push(2),
        CloseReason::Error(message) => {
            out.push(3);
            put_string(out, message)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrips_route_plan_and_payload() {
        let envelope = Envelope {
            route_plan: vec![2, 3],
            payload: b"payload".to_vec(),
        };

        let encoded = envelope.encode().unwrap();
        let decoded = Envelope::decode(&encoded).unwrap();

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn session_frame_roundtrips_open_connection() {
        let mut metadata = Metadata::new();
        metadata.insert("proto".to_string(), "tcp".to_string());
        let frame = SessionFrame::OpenConnection {
            target: Target {
                host: "example.com".to_string(),
                port: 443,
            },
            metadata,
        };

        let encoded = frame.encode().unwrap();
        let decoded = SessionFrame::decode(&encoded).unwrap();

        assert_eq!(decoded, frame);
    }
}
