//! Small checked primitives shared by cooked-cache binary envelopes.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BinaryError {
    Allocation,
    CapacityExceeded,
    Truncated,
    LengthOverflow,
    TrailingBytes,
}

pub(super) struct BinaryWriter {
    bytes: Vec<u8>,
    max_len: usize,
}

impl BinaryWriter {
    /// Creates a writer whose checked capacity is also a hard encoded-length ceiling.
    pub(super) fn with_capacity(capacity: usize) -> Result<Self, BinaryError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| BinaryError::Allocation)?;
        Ok(Self {
            bytes,
            max_len: capacity,
        })
    }

    pub(super) fn write(&mut self, value: &[u8]) -> Result<(), BinaryError> {
        let next_len = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(BinaryError::LengthOverflow)?;
        if next_len > self.max_len {
            return Err(BinaryError::CapacityExceeded);
        }
        self.bytes
            .try_reserve(value.len())
            .map_err(|_| BinaryError::Allocation)?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    pub(super) fn u8(&mut self, value: u8) -> Result<(), BinaryError> {
        self.write(&[value])
    }

    pub(super) fn u16(&mut self, value: u16) -> Result<(), BinaryError> {
        self.write(&value.to_le_bytes())
    }

    pub(super) fn u32(&mut self, value: u32) -> Result<(), BinaryError> {
        self.write(&value.to_le_bytes())
    }

    pub(super) fn u64(&mut self, value: u64) -> Result<(), BinaryError> {
        self.write(&value.to_le_bytes())
    }

    pub(super) fn f32_bits(&mut self, value: f32) -> Result<(), BinaryError> {
        self.u32(value.to_bits())
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub(super) struct BinaryReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BinaryReader<'a> {
    pub(super) fn new(bytes: &'a [u8], magic: &[u8]) -> Result<Self, BinaryError> {
        let mut reader = Self { bytes, offset: 0 };
        if reader.read_exact(magic.len())? != magic {
            return Err(BinaryError::Truncated);
        }
        Ok(reader)
    }

    pub(super) fn read_exact(&mut self, length: usize) -> Result<&'a [u8], BinaryError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(BinaryError::LengthOverflow)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(BinaryError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    pub(super) fn u16(&mut self) -> Result<u16, BinaryError> {
        Ok(u16::from_le_bytes(
            self.read_exact(2)?
                .try_into()
                .map_err(|_| BinaryError::Truncated)?,
        ))
    }

    pub(super) fn u8(&mut self) -> Result<u8, BinaryError> {
        Ok(self.read_exact(1)?[0])
    }

    pub(super) fn u32(&mut self) -> Result<u32, BinaryError> {
        Ok(u32::from_le_bytes(
            self.read_exact(4)?
                .try_into()
                .map_err(|_| BinaryError::Truncated)?,
        ))
    }

    pub(super) fn u64(&mut self) -> Result<u64, BinaryError> {
        Ok(u64::from_le_bytes(
            self.read_exact(8)?
                .try_into()
                .map_err(|_| BinaryError::Truncated)?,
        ))
    }

    pub(super) fn f32_bits(&mut self) -> Result<f32, BinaryError> {
        Ok(f32::from_bits(self.u32()?))
    }

    pub(super) fn finish(self) -> Result<(), BinaryError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(BinaryError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BinaryError, BinaryReader, BinaryWriter};

    #[test]
    fn checked_fields_roundtrip_and_reject_truncation_or_trailing_bytes() {
        let mut writer = BinaryWriter::with_capacity(19).unwrap();
        writer.write(b"TST1").unwrap();
        writer.u8(7).unwrap();
        writer.u16(0x1234).unwrap();
        writer.u32(0x89ab_cdef).unwrap();
        writer.u64(0x0123_4567_89ab_cdef).unwrap();
        let encoded = writer.into_bytes();

        assert_eq!(
            BinaryReader::new(&encoded[..3], b"TST1").err(),
            Some(BinaryError::Truncated)
        );
        let mut reader = BinaryReader::new(&encoded, b"TST1").unwrap();
        assert_eq!(reader.u8().unwrap(), 7);
        assert_eq!(reader.u16().unwrap(), 0x1234);
        assert_eq!(reader.u32().unwrap(), 0x89ab_cdef);
        assert_eq!(reader.u64().unwrap(), 0x0123_4567_89ab_cdef);
        assert_eq!(reader.finish(), Ok(()));

        let mut truncated = BinaryReader::new(&encoded[..encoded.len() - 1], b"TST1").unwrap();
        truncated.read_exact(1).unwrap();
        truncated.u16().unwrap();
        truncated.u32().unwrap();
        assert_eq!(truncated.u64(), Err(BinaryError::Truncated));

        let mut with_trailer = encoded.clone();
        with_trailer.push(0);
        assert_eq!(
            BinaryReader::new(&with_trailer, b"TST1").unwrap().finish(),
            Err(BinaryError::TrailingBytes)
        );

        let mut bounded_writer = BinaryWriter::with_capacity(1).unwrap();
        bounded_writer.write(&[1]).unwrap();
        assert_eq!(
            bounded_writer.write(&[2]),
            Err(BinaryError::CapacityExceeded)
        );
    }
}
