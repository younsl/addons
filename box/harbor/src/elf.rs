//! Minimal ELF header parsing, enough to tell which CPU a binary targets.

use crate::error::{Error, Result};

#[cfg(test)]
pub const EM_X86_64: u16 = 62;
pub const EM_AARCH64: u16 = 183;

const MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const EI_DATA: usize = 5;
const E_MACHINE: usize = 18;

/// Return `e_machine` from an ELF header. `name` only labels the error.
pub fn machine(header: &[u8], name: &str) -> Result<u16> {
    if header.len() < E_MACHINE + 2 || header[..4] != MAGIC {
        return Err(Error::InvalidElf(name.to_string()));
    }
    let bytes = [header[E_MACHINE], header[E_MACHINE + 1]];
    match header[EI_DATA] {
        1 => Ok(u16::from_le_bytes(bytes)),
        2 => Ok(u16::from_be_bytes(bytes)),
        _ => Err(Error::InvalidElf(name.to_string())),
    }
}

#[cfg(test)]
pub fn header(machine: u16) -> Vec<u8> {
    let mut h = vec![0u8; 64];
    h[..4].copy_from_slice(&MAGIC);
    h[4] = 2;
    h[EI_DATA] = 1;
    h[E_MACHINE..E_MACHINE + 2].copy_from_slice(&machine.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_machine() {
        assert_eq!(machine(&header(EM_AARCH64), "bin").unwrap(), EM_AARCH64);
        assert_eq!(machine(&header(EM_X86_64), "bin").unwrap(), EM_X86_64);
    }

    #[test]
    fn reads_big_endian_machine() {
        let mut h = header(0);
        h[EI_DATA] = 2;
        h[E_MACHINE..E_MACHINE + 2].copy_from_slice(&EM_AARCH64.to_be_bytes());
        assert_eq!(machine(&h, "bin").unwrap(), EM_AARCH64);
    }

    #[test]
    fn rejects_non_elf() {
        assert!(matches!(
            machine(b"#!/bin/sh\necho hello world", "script"),
            Err(Error::InvalidElf(_))
        ));
        assert!(matches!(
            machine(&[0x7f, b'E'], "short"),
            Err(Error::InvalidElf(_))
        ));
    }

    #[test]
    fn rejects_unknown_endianness() {
        let mut h = header(EM_AARCH64);
        h[EI_DATA] = 0;
        assert!(matches!(machine(&h, "bin"), Err(Error::InvalidElf(_))));
    }
}
