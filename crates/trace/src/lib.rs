//! The `.oxytrace` execution-trace format: writer, reader, and diff.
//!
//! A trace records the interpreter's per-instruction execution: the PC,
//! the ARM/Thumb and halted state bits, the register writes the
//! instruction made, and the memory writes the step performed (IO, RAM,
//! and immediate DMA alike). Two headless runs of the same ROM with the
//! same inputs produce byte-identical traces, which makes the trace the
//! oracle baseline for the recompiled backends.
//!
//! # Format
//!
//! Little-endian throughout. The 12-byte header is the magic
//! `OXYTRACE`, a `u16` format version, and a reserved `u16`. Each
//! record is:
//!
//! - `u32` PC (post-execution, the address of the next instruction)
//! - `u8` flags: bit 0 Thumb state, bit 1 the CPU halted
//! - `u8` register-write count, then that many `(u8 index, u32 value)`
//! - `u32` memory-write count, then that many `(u32 addr, u8 width,
//!   u32 value)`; width is 1, 2, or 4 bytes and the value is stored in
//!   the low bytes exactly as the bus wrote it
//!
//! The register indices address the current-mode register view after
//! the instruction (a mode switch such as an SWI entry makes the
//! banked registers appear as deltas); SPSR contents are not recorded.
//! A single record may carry many writes when the instruction armed an
//! immediate DMA.

use std::io::{self, Read, Write};

/// The format magic: the first eight bytes of every trace file.
pub const MAGIC: &[u8; 8] = b"OXYTRACE";

/// The current format version.
pub const VERSION: u16 = 1;

/// Record flag: the instruction executed in Thumb state.
pub const FLAG_THUMB: u8 = 0x01;

/// Record flag: the CPU was halted when this step ran.
pub const FLAG_HALTED: u8 = 0x02;

/// One register write of an executed instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegWrite {
    /// Register index in the current-mode view (0-15).
    pub index: u8,
    /// The value written.
    pub value: u32,
}

/// One bus write performed during an executed instruction's step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemWrite {
    /// The bus address written.
    pub addr: u32,
    /// The write width in bytes (1, 2, or 4).
    pub width: u8,
    /// The value, right-aligned in the low bytes.
    pub value: u32,
}

/// One executed instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The PC after the instruction (the next instruction's address).
    pub pc: u32,
    /// [`FLAG_THUMB`] and [`FLAG_HALTED`] bits.
    pub flags: u8,
    /// The register writes, in write order.
    pub reg_delta: Vec<RegWrite>,
    /// The bus writes, in write order.
    pub writes: Vec<MemWrite>,
}

/// An error while reading a trace: a bad header, truncated record, or
/// impossible field.
#[derive(Debug)]
pub struct ReadError(pub io::Error);

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for ReadError {}

/// The first difference between two traces.
#[derive(Debug, PartialEq, Eq)]
pub enum Diff {
    /// Every record matched.
    Identical,
    /// The records at `index` differ in PC, state, register delta, or
    /// memory writes.
    Diverged {
        /// Zero-based instruction index of the first difference.
        index: u64,
        /// The record in the first trace.
        first: Box<Record>,
        /// The record in the second trace.
        second: Box<Record>,
    },
    /// Both traces agree up to `index`, where the first one ended but
    /// the second continued.
    LengthMismatch {
        /// Zero-based index of the first record that only one trace
        /// has.
        index: u64,
        /// Whether the longer trace is the second one (`false` means
        /// the first trace is the longer one).
        second_longer: bool,
    },
}

/// Streams records into a writer.
pub struct Writer<W: Write> {
    inner: W,
}

impl<W: Write> Writer<W> {
    /// Writes the header and returns a writer positioned at the first
    /// record.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn new(mut inner: W) -> io::Result<Self> {
        inner.write_all(MAGIC)?;
        inner.write_all(&VERSION.to_le_bytes())?;
        inner.write_all(&0u16.to_le_bytes())?;
        Ok(Writer { inner })
    }

    /// Appends one record.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures and rejects a register delta of more
    /// than 255 entries or a write list of more than `u32::MAX`.
    pub fn write(&mut self, record: &Record) -> io::Result<()> {
        self.inner.write_all(&record.pc.to_le_bytes())?;
        self.inner.write_all(&[record.flags])?;
        let regs = u8::try_from(record.reg_delta.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "register delta exceeds 255 entries",
            )
        })?;
        self.inner.write_all(&[regs])?;
        for reg in &record.reg_delta {
            self.inner.write_all(&[reg.index])?;
            self.inner.write_all(&reg.value.to_le_bytes())?;
        }
        let writes = u32::try_from(record.writes.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "write list exceeds u32::MAX")
        })?;
        self.inner.write_all(&writes.to_le_bytes())?;
        for write in &record.writes {
            self.inner.write_all(&write.addr.to_le_bytes())?;
            self.inner.write_all(&[write.width])?;
            self.inner.write_all(&write.value.to_le_bytes())?;
        }
        Ok(())
    }

    /// Flushes the underlying writer.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn finish(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Reads records back from a trace byte stream. Wrap file handles in
/// `BufReader`: reads are byte-wise.
pub struct Reader<R: Read> {
    inner: R,
    one: [u8; 1],
}

impl<R: Read> Reader<R> {
    /// Checks the header and returns a reader positioned at the first
    /// record.
    ///
    /// # Errors
    ///
    /// Fails on I/O errors and on a bad magic or unsupported version.
    pub fn new(mut inner: R) -> Result<Self, ReadError> {
        let mut header = [0u8; 12];
        inner.read_exact(&mut header).map_err(ReadError)?;
        if &header[0..8] != MAGIC {
            return Err(ReadError(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an OXYTRACE file (bad magic)",
            )));
        }
        let version = u16::from_le_bytes([header[8], header[9]]);
        if version != VERSION {
            return Err(ReadError(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported trace version {version}"),
            )));
        }
        Ok(Reader { inner, one: [0] })
    }

    fn byte(&mut self) -> Result<u8, ReadError> {
        self.inner.read_exact(&mut self.one).map_err(ReadError)?;
        Ok(self.one[0])
    }

    fn word(&mut self) -> Result<[u8; 4], ReadError> {
        let mut buf = [0u8; 4];
        for slot in &mut buf {
            *slot = self.byte()?;
        }
        Ok(buf)
    }

    /// Returns the next record, or `None` at a clean end of stream.
    ///
    /// # Errors
    ///
    /// Fails on a truncated record.
    pub fn next_record(&mut self) -> Result<Option<Record>, ReadError> {
        let Some(first) = self.byte_opt()? else {
            return Ok(None);
        };
        let pc = u32::from_le_bytes([first, self.byte()?, self.byte()?, self.byte()?]);
        let flags = self.byte()?;
        let mut record = Record {
            pc,
            flags,
            reg_delta: Vec::new(),
            writes: Vec::new(),
        };
        let regs = self.byte()?;
        for _ in 0..regs {
            let index = self.byte()?;
            let value = u32::from_le_bytes(self.word()?);
            record.reg_delta.push(RegWrite { index, value });
        }
        let writes = u32::from_le_bytes(self.word()?);
        for _ in 0..writes {
            let addr = u32::from_le_bytes(self.word()?);
            let width = self.byte()?;
            let value = u32::from_le_bytes(self.word()?);
            record.writes.push(MemWrite { addr, width, value });
        }
        Ok(Some(record))
    }

    fn byte_opt(&mut self) -> Result<Option<u8>, ReadError> {
        match self.byte() {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.0.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Compares two traces record by record and returns the first
/// difference, or [`Diff::Identical`].
///
/// # Errors
///
/// Propagates read failures from either trace.
pub fn diff<R: Read, S: Read>(mut a: Reader<R>, mut b: Reader<S>) -> Result<Diff, ReadError> {
    let mut index = 0u64;
    loop {
        let ra = a.next_record()?;
        let rb = b.next_record()?;
        match (ra, rb) {
            (Some(ra), Some(rb)) => {
                if ra != rb {
                    return Ok(Diff::Diverged {
                        index,
                        first: Box::new(ra),
                        second: Box::new(rb),
                    });
                }
            }
            (Some(_), None) => {
                return Ok(Diff::LengthMismatch {
                    index,
                    second_longer: false,
                });
            }
            (None, Some(_)) => {
                return Ok(Diff::LengthMismatch {
                    index,
                    second_longer: true,
                });
            }
            (None, None) => return Ok(Diff::Identical),
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(records: &[Record]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut writer = Writer::new(&mut bytes).expect("header");
        for record in records {
            writer.write(record).expect("record");
        }
        writer.finish().expect("flush");
        bytes
    }

    fn sample(index: u8, value: u32) -> Record {
        Record {
            pc: 0x0800_0100 + u32::from(index) * 4,
            flags: FLAG_THUMB,
            reg_delta: vec![RegWrite { index, value }],
            writes: vec![MemWrite {
                addr: 0x0400_0000,
                width: 2,
                value,
            }],
        }
    }

    #[test]
    fn round_trips_records() {
        let records = vec![
            sample(0, 0x1234_5678),
            Record {
                pc: 0x0000_00EA,
                flags: FLAG_HALTED,
                reg_delta: (0..16)
                    .map(|i| RegWrite {
                        index: i,
                        value: u32::from(i),
                    })
                    .collect(),
                writes: (0..300)
                    .map(|i| MemWrite {
                        addr: 0x0200_0000 + i * 4,
                        width: 4,
                        value: i,
                    })
                    .collect(),
            },
            Record {
                pc: 0x0800_0000,
                flags: 0,
                reg_delta: Vec::new(),
                writes: Vec::new(),
            },
        ];
        let bytes = encode(&records);
        let mut reader = Reader::new(&bytes[..]).expect("header");
        for want in &records {
            assert_eq!(&reader.next_record().expect("read").expect("record"), want);
        }
        assert!(reader.next_record().expect("read").is_none());
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let mut bytes = encode(&[]);
        bytes[0] = b'X';
        assert!(Reader::new(&bytes[..]).is_err());
        let mut bytes = encode(&[]);
        bytes[8] = 9;
        assert!(Reader::new(&bytes[..]).is_err());
    }

    #[test]
    fn detects_truncated_records() {
        let bytes = encode(&[sample(3, 7)]);
        let cut = &bytes[..bytes.len() - 2];
        let mut reader = Reader::new(cut).expect("header");
        assert!(reader.next_record().is_err());
    }

    #[test]
    fn diff_reports_identical_and_diverged() {
        let bytes = encode(&[sample(0, 1), sample(1, 2)]);
        let diverged = Record {
            pc: sample(1, 2).pc,
            flags: FLAG_THUMB,
            reg_delta: vec![RegWrite { index: 1, value: 9 }],
            writes: vec![MemWrite {
                addr: 0x0400_0000,
                width: 2,
                value: 2,
            }],
        };
        let mutate = encode(&[sample(0, 1), diverged.clone()]);
        assert_eq!(
            diff(
                Reader::new(&bytes[..]).unwrap(),
                Reader::new(&bytes[..]).unwrap()
            )
            .expect("read"),
            Diff::Identical
        );
        assert_eq!(
            diff(
                Reader::new(&bytes[..]).unwrap(),
                Reader::new(&mutate[..]).unwrap()
            )
            .expect("read"),
            Diff::Diverged {
                index: 1,
                first: Box::new(sample(1, 2)),
                second: Box::new(diverged),
            }
        );
    }

    #[test]
    fn diff_reports_length_mismatch() {
        let short = encode(&[sample(0, 1)]);
        let long = encode(&[sample(0, 1), sample(1, 2)]);
        assert_eq!(
            diff(
                Reader::new(&short[..]).unwrap(),
                Reader::new(&long[..]).unwrap()
            )
            .expect("read"),
            Diff::LengthMismatch {
                index: 1,
                second_longer: true,
            }
        );
        assert_eq!(
            diff(
                Reader::new(&long[..]).unwrap(),
                Reader::new(&short[..]).unwrap()
            )
            .expect("read"),
            Diff::LengthMismatch {
                index: 1,
                second_longer: false,
            }
        );
    }
}
