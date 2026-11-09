//! ELF test fixture generator.
//!
//! Generates minimal ELF binary headers in code to avoid committing binary artifacts
//! into the repository while maintaining test coverage for static, dynamic, PIE,
//! ELF32, and big-endian ELF validation.

use std::io::Write;
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfClass {
    Elf32,
    Elf64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    Little,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    Load,   // PT_LOAD = 1
    Interp, // PT_INTERP = 3
}

#[derive(Debug, Clone)]
pub struct ElfBuilder {
    pub class: ElfClass,
    pub endianness: Endianness,
    pub elf_type: u16,
    pub machine: u16,
    pub segment_type: SegmentType,
    pub interpreter: Option<Vec<u8>>,
}

impl ElfBuilder {
    pub fn static_binary() -> Self {
        Self {
            class: ElfClass::Elf64,
            endianness: Endianness::Little,
            elf_type: 2,   // ET_EXEC
            machine: 0x3e, // EM_X86_64
            segment_type: SegmentType::Load,
            interpreter: None,
        }
    }

    pub fn dynamic_binary() -> Self {
        Self {
            class: ElfClass::Elf64,
            endianness: Endianness::Little,
            elf_type: 2,   // ET_EXEC
            machine: 0x3e, // EM_X86_64
            segment_type: SegmentType::Interp,
            interpreter: Some(b"/lib/ld.so\0".to_vec()),
        }
    }

    pub fn static_pie() -> Self {
        Self {
            class: ElfClass::Elf64,
            endianness: Endianness::Little,
            elf_type: 3,   // ET_DYN
            machine: 0x3e, // EM_X86_64
            segment_type: SegmentType::Load,
            interpreter: None,
        }
    }

    pub fn static_big_endian() -> Self {
        Self {
            class: ElfClass::Elf64,
            endianness: Endianness::Big,
            elf_type: 2,   // ET_EXEC
            machine: 0x3e, // EM_X86_64
            segment_type: SegmentType::Load,
            interpreter: None,
        }
    }

    pub fn static_elf32() -> Self {
        Self {
            class: ElfClass::Elf32,
            endianness: Endianness::Little,
            elf_type: 2, // ET_EXEC
            machine: 3,  // EM_386
            segment_type: SegmentType::Load,
            interpreter: None,
        }
    }

    pub fn build(&self) -> Vec<u8> {
        let is_le = self.endianness == Endianness::Little;
        let mut buf = Vec::new();

        buf.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
        buf.push(match self.class {
            ElfClass::Elf32 => 1,
            ElfClass::Elf64 => 2,
        });
        buf.push(match self.endianness {
            Endianness::Little => 1,
            Endianness::Big => 2,
        });
        buf.push(1); // EV_CURRENT
        buf.extend_from_slice(&[0u8; 9]);

        let p_type: u32 = match self.segment_type {
            SegmentType::Load => 1,
            SegmentType::Interp => 3,
        };

        let interp_bytes = self.interpreter.as_deref().unwrap_or_default();

        match self.class {
            ElfClass::Elf32 => {
                let ehsize: u16 = 52;
                let phentsize: u16 = 32;
                let phnum: u16 = 1;
                let phoff: u32 = 52;

                write_u16(&mut buf, self.elf_type, is_le);
                write_u16(&mut buf, self.machine, is_le);
                write_u32(&mut buf, 1, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u32(&mut buf, phoff, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u16(&mut buf, ehsize, is_le);
                write_u16(&mut buf, phentsize, is_le);
                write_u16(&mut buf, phnum, is_le);
                write_u16(&mut buf, 0, is_le);
                write_u16(&mut buf, 0, is_le);
                write_u16(&mut buf, 0, is_le);

                let p_offset: u32 = if self.segment_type == SegmentType::Interp {
                    52 + 32
                } else {
                    0
                };
                let p_filesz: u32 = u32::try_from(interp_bytes.len()).unwrap_or(0);

                write_u32(&mut buf, p_type, is_le);
                write_u32(&mut buf, p_offset, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u32(&mut buf, p_filesz, is_le);
                write_u32(&mut buf, p_filesz, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u32(&mut buf, 0, is_le);
            }
            ElfClass::Elf64 => {
                let ehsize: u16 = 64;
                let phentsize: u16 = 56;
                let phnum: u16 = 1;
                let phoff: u64 = 64;

                write_u16(&mut buf, self.elf_type, is_le);
                write_u16(&mut buf, self.machine, is_le);
                write_u32(&mut buf, 1, is_le);
                write_u64(&mut buf, 0, is_le);
                write_u64(&mut buf, phoff, is_le);
                write_u64(&mut buf, 0, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u16(&mut buf, ehsize, is_le);
                write_u16(&mut buf, phentsize, is_le);
                write_u16(&mut buf, phnum, is_le);
                write_u16(&mut buf, 0, is_le);
                write_u16(&mut buf, 0, is_le);
                write_u16(&mut buf, 0, is_le);

                let p_offset: u64 = if self.segment_type == SegmentType::Interp {
                    64 + 56
                } else {
                    0
                };
                let p_filesz: u64 = u64::try_from(interp_bytes.len()).unwrap_or(0);

                write_u32(&mut buf, p_type, is_le);
                write_u32(&mut buf, 0, is_le);
                write_u64(&mut buf, p_offset, is_le);
                write_u64(&mut buf, 0, is_le);
                write_u64(&mut buf, 0, is_le);
                write_u64(&mut buf, p_filesz, is_le);
                write_u64(&mut buf, p_filesz, is_le);
                write_u64(&mut buf, 0, is_le);
            }
        }

        buf.extend_from_slice(interp_bytes);
        buf
    }

    pub fn write_temp(&self) -> std::io::Result<NamedTempFile> {
        let mut file = NamedTempFile::new()?;
        file.write_all(&self.build())?;
        file.flush()?;
        Ok(file)
    }
}

fn write_u16(buf: &mut Vec<u8>, val: u16, is_le: bool) {
    if is_le {
        buf.extend_from_slice(&val.to_le_bytes());
    } else {
        buf.extend_from_slice(&val.to_be_bytes());
    }
}

fn write_u32(buf: &mut Vec<u8>, val: u32, is_le: bool) {
    if is_le {
        buf.extend_from_slice(&val.to_le_bytes());
    } else {
        buf.extend_from_slice(&val.to_be_bytes());
    }
}

fn write_u64(buf: &mut Vec<u8>, val: u64, is_le: bool) {
    if is_le {
        buf.extend_from_slice(&val.to_le_bytes());
    } else {
        buf.extend_from_slice(&val.to_be_bytes());
    }
}
