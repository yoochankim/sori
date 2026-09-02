//! Crash-safe 16-bit PCM WAV writer.
//!
//! A WAV header carries the data length, so a file whose process died mid-write
//! is normally unreadable. This writer patches the header every `flush()` so
//! that at worst the last few seconds are lost, not the whole meeting.

use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub struct WavWriter {
    out: BufWriter<File>,
    data_bytes: u32,
    sample_rate: u32,
}

const HEADER_LEN: u64 = 44;

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(Permissions::from_mode(0o600))?;
        let mut w = Self {
            out: BufWriter::with_capacity(1 << 16, file),
            data_bytes: 0,
            sample_rate,
        };
        w.write_header()?;
        Ok(w)
    }

    fn write_header(&mut self) -> std::io::Result<()> {
        let channels: u16 = 1;
        let bits: u16 = 16;
        let block_align = channels * bits / 8;
        let byte_rate = self.sample_rate * block_align as u32;

        self.out.seek(SeekFrom::Start(0))?;
        self.out.write_all(b"RIFF")?;
        self.out.write_all(&(36 + self.data_bytes).to_le_bytes())?;
        self.out.write_all(b"WAVE")?;
        self.out.write_all(b"fmt ")?;
        self.out.write_all(&16u32.to_le_bytes())?;
        self.out.write_all(&1u16.to_le_bytes())?; // PCM
        self.out.write_all(&channels.to_le_bytes())?;
        self.out.write_all(&self.sample_rate.to_le_bytes())?;
        self.out.write_all(&byte_rate.to_le_bytes())?;
        self.out.write_all(&block_align.to_le_bytes())?;
        self.out.write_all(&bits.to_le_bytes())?;
        self.out.write_all(b"data")?;
        self.out.write_all(&self.data_bytes.to_le_bytes())?;
        Ok(())
    }

    #[inline]
    pub fn write_sample(&mut self, s: f32) -> std::io::Result<()> {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        self.out.write_all(&v.to_le_bytes())?;
        self.data_bytes = self.data_bytes.saturating_add(2);
        Ok(())
    }

    /// Number of samples written so far.
    pub fn samples(&self) -> u64 {
        self.data_bytes as u64 / 2
    }

    /// Patch the header with the current length and push everything to disk.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let end = HEADER_LEN + self.data_bytes as u64;
        self.write_header()?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()
    }

    pub fn finalize(mut self) -> std::io::Result<()> {
        self.flush()?;
        self.out.get_ref().sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn wav_is_owner_read_write_only() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("sori-wav-{}-{nonce}.wav", std::process::id()));
        WavWriter::create(&path, 48_000)
            .unwrap()
            .finalize()
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(path).unwrap();
    }
}
