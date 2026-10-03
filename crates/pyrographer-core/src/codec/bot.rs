//! USB Mass-Storage Bulk-Only Transport (BOT) framing.
//!
//! BOT is the envelope that carries Rockchip's rockusb protocol. One command has
//! three phases:
//!
//! 1. A 31-byte Command Block Wrapper (CBW) on bulk OUT, announcing how many
//!    bytes the data phase moves and in which direction
//! 2. An optional data phase in that direction
//! 3. A 13-byte Command Status Wrapper (CSW) on bulk IN, echoing the command's
//!    tag and reporting whether it passed
//!
//! The envelope is vendor-neutral, and the command block it carries is
//! vendor-specific. [`rockusb`](super::rockusb) defines Rockchip's.
//!
//! This module is sans-I/O: it builds and parses the two wrappers and performs no
//! I/O.

use crate::{Error, Result};

/// Direction of a BOT data phase, from the host's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Device to host (bulk IN).
    In,
    /// Host to device (bulk OUT).
    Out,
}

/// Length of a Command Block Wrapper, in bytes.
pub const CBW_LEN: usize = 31;

/// Length of a Command Status Wrapper, in bytes.
pub const CSW_LEN: usize = 13;

/// The largest command block a CBW can carry, in bytes.
pub const CBW_CB_MAX: usize = 16;

/// `dCBWSignature`: the ASCII bytes `USBC`, read as a little-endian `u32`.
const CBW_SIGNATURE: u32 = 0x4342_5355;

/// `dCSWSignature`: the ASCII bytes `USBS`, read as a little-endian `u32`.
const CSW_SIGNATURE: u32 = 0x5342_5355;

/// Build a Command Block Wrapper.
///
/// `tag` is echoed back in the CSW, so it identifies which command a status
/// answers. `data_len` is the length of the data phase, and `dir` its direction.
/// If `data_len` is zero, there is no data phase, and the direction is ignored.
///
/// The CBW's own fields are little-endian, per the Bulk-Only Transport
/// specification. The command set defines the byte order inside `command`, and
/// Rockchip's is big-endian.
///
/// # Panics
///
/// Panics on a `command` that is empty or longer than [`CBW_CB_MAX`]. The
/// Bulk-Only Transport specification's `bCBWCBLength` is 1..=16. A zero-length
/// command block is a malformed wrapper, and every caller in this crate passes a
/// real opcode. An empty one is therefore a programming error, not a runtime
/// condition.
pub fn build_cbw(tag: u32, data_len: u32, dir: Direction, command: &[u8]) -> [u8; CBW_LEN] {
    assert!(
        (1..=CBW_CB_MAX).contains(&command.len()),
        "a CBW command block is 1 to {CBW_CB_MAX} bytes, got {}",
        command.len()
    );
    let mut cbw = [0u8; CBW_LEN];
    cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes()); // dCBWSignature
    cbw[4..8].copy_from_slice(&tag.to_le_bytes()); // dCBWTag
    cbw[8..12].copy_from_slice(&data_len.to_le_bytes()); // dCBWDataTransferLength
    cbw[12] = match dir {
        Direction::In => 0x80,
        Direction::Out => 0x00,
    }; // bmCBWFlags
    cbw[13] = 0; // bCBWLUN
    cbw[14] = command.len() as u8; // bCBWCBLength
    cbw[15..15 + command.len()].copy_from_slice(command); // CBWCB
    cbw
}

/// What a device reports in a CSW's `bCSWStatus` byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CswStatus {
    /// The command succeeded.
    Passed,
    /// The command failed.
    Failed,
    /// Host and device disagree about the data phase. The endpoints must be
    /// reset before the next command.
    PhaseError,
}

/// A parsed Command Status Wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csw {
    /// Echo of the `dCBWTag` of the command this answers.
    pub tag: u32,
    /// How many of the announced data-phase bytes the device did not move.
    pub residue: u32,
    /// The command's outcome.
    pub status: CswStatus,
}

/// Parse a Command Status Wrapper.
///
/// Returns [`Error::Protocol`] for a wrapper of the wrong length or a signature
/// other than `USBS`. It returns the same error for a status byte outside the three
/// the specification defines. A device that sends any of these is not speaking BOT,
/// so none of the wrapper's fields are returned.
pub fn parse_csw(bytes: &[u8]) -> Result<Csw> {
    if bytes.len() != CSW_LEN {
        return Err(Error::Protocol(format!(
            "CSW is {} bytes, expected {CSW_LEN}",
            bytes.len()
        )));
    }

    let signature = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if signature != CSW_SIGNATURE {
        return Err(Error::Protocol(format!(
            "CSW signature is {signature:#010x}, expected {CSW_SIGNATURE:#010x} (\"USBS\")"
        )));
    }

    let status = match bytes[12] {
        0x00 => CswStatus::Passed,
        0x01 => CswStatus::Failed,
        0x02 => CswStatus::PhaseError,
        other => {
            return Err(Error::Protocol(format!(
                "CSW status byte is {other:#04x}, which is not a defined status"
            )));
        }
    };

    Ok(Csw {
        tag: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        residue: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a CSW as a device sends it, for feeding to [`parse_csw`].
    fn csw_bytes(tag: u32, residue: u32, status: u8) -> [u8; CSW_LEN] {
        let mut csw = [0u8; CSW_LEN];
        csw[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        csw[4..8].copy_from_slice(&tag.to_le_bytes());
        csw[8..12].copy_from_slice(&residue.to_le_bytes());
        csw[12] = status;
        csw
    }

    #[test]
    fn cbw_layout_is_correct() {
        let cbw = build_cbw(0x0102_0304, 512, Direction::In, &[0x80]);
        assert_eq!(&cbw[0..4], b"USBC"); // signature, little-endian
        assert_eq!(&cbw[4..8], &[0x04, 0x03, 0x02, 0x01]); // tag, little-endian
        assert_eq!(&cbw[8..12], &[0x00, 0x02, 0x00, 0x00]); // 512, little-endian
        assert_eq!(cbw[12], 0x80); // direction IN
        assert_eq!(cbw[13], 0x00); // LUN 0
        assert_eq!(cbw[14], 1); // command length
        assert_eq!(cbw[15], 0x80); // command byte
        assert_eq!(&cbw[16..], &[0u8; 15]); // the rest of the block is zeroed
    }

    #[test]
    fn cbw_marks_an_out_data_phase() {
        let cbw = build_cbw(1, 16, Direction::Out, &[0x15]);
        assert_eq!(cbw[12], 0x00);
    }

    #[test]
    fn csw_round_trips() {
        let csw = parse_csw(&csw_bytes(0x0102_0304, 7, 0x00)).unwrap();
        assert_eq!(
            csw,
            Csw {
                tag: 0x0102_0304,
                residue: 7,
                status: CswStatus::Passed,
            }
        );
    }

    #[test]
    fn csw_reports_failure_and_phase_error() {
        assert_eq!(
            parse_csw(&csw_bytes(1, 0, 0x01)).unwrap().status,
            CswStatus::Failed
        );
        assert_eq!(
            parse_csw(&csw_bytes(1, 0, 0x02)).unwrap().status,
            CswStatus::PhaseError
        );
    }

    #[test]
    fn csw_rejects_a_bad_signature() {
        let mut bytes = csw_bytes(1, 0, 0x00);
        bytes[0] = b'X';
        assert!(matches!(parse_csw(&bytes), Err(Error::Protocol(_))));
    }

    #[test]
    fn csw_rejects_a_wrong_length() {
        assert!(matches!(parse_csw(&[]), Err(Error::Protocol(_))));
        // Short.
        assert!(matches!(
            parse_csw(&csw_bytes(1, 0, 0)[..12]),
            Err(Error::Protocol(_))
        ));
        // Over-length: a 14th byte is not a longer CSW, it is a framing error.
        let mut over = csw_bytes(1, 0, 0).to_vec();
        over.push(0);
        assert!(matches!(parse_csw(&over), Err(Error::Protocol(_))));
    }

    /// The CSW signature, checked against the literal ASCII `USBS` rather than the
    /// `CSW_SIGNATURE` constant the parser uses. A mistyped constant fails here,
    /// where a comparison of the constant with itself would pass.
    #[test]
    fn csw_signature_is_the_ascii_bytes_usbs() {
        assert_eq!(CSW_SIGNATURE.to_le_bytes(), *b"USBS");
    }

    /// `build_cbw` refuses an empty command block: the specification's
    /// `bCBWCBLength` is 1..=16, and every real command carries an opcode.
    #[test]
    #[should_panic(expected = "1 to")]
    fn build_cbw_refuses_an_empty_command_block() {
        let _ = build_cbw(1, 0, Direction::In, &[]);
    }

    #[test]
    fn csw_rejects_an_undefined_status() {
        assert!(matches!(
            parse_csw(&csw_bytes(1, 0, 0x7f)),
            Err(Error::Protocol(_))
        ));
    }
}
