//! The SoCs the wrong-loader gate knows, and the `K_FW_GET_CHIP_VER` replies
//! pinned for them.
//!
//! The wrong-loader gate compares the loader's own answer to which SoC it runs
//! on with the SoC the caller named. This module holds the table the gate
//! consults. The gate matches **exact pinned bytes**: an entry is the whole reply
//! a real board gave. A SoC without an entry cannot be named at all.
//!
//! Nothing here decodes a reply into meaning. Both pinned replies open with the
//! SoC's ASCII digits byte-reversed, and they differ after that. The RK3576's SPL
//! loader sends twelve zeros, and the RK3588's usbplug loader sends twelve `0xff`.
//! Whether that tail belongs to the SoC or to the loader build is **\[UNVERIFIED\]**.
//! Either way, a reply with another tail comes from a loader nobody has measured,
//! and the gate refuses it. That is why an entry is the whole reply.
//!
//! Adding a SoC is one entry here. Run `pyrographer chipver` against a board
//! whose part is known, and pin what it answered.

use crate::{Error, Result};

/// A SoC the wrong-loader gate can be armed for.
///
/// A value of this type means the pinned table holds the exact `K_FW_GET_CHIP_VER`
/// reply a real board of this SoC gave. The gate can therefore make the comparison.
/// [`Soc::parse`] is the only way to get one, and it refuses names with no pinned
/// entry. A plan's `Option<Soc>` that is `Some` therefore carries everything the
/// gate needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Soc {
    /// The canonical name: `"rk3576"`.
    name: &'static str,
    /// The reply a real board of this SoC gave, whole.
    reply: &'static [u8],
    /// The four bytes an RKBOOT container for this SoC carries in its chip
    /// field, or `None` where no container has been measured.
    ///
    /// Pinned separately from `reply`, and never derived from it. On both pinned
    /// SoCs, they agree. An RK3576 container says `"6753"`, and an RK3576 loader
    /// answers `"6753"` and twelve zeros. That agreement is a measurement, not a
    /// rule for computing one from the other.
    ///
    /// A SoC pinned by a board but by no container is `None` here. Its container
    /// cannot be judged before an upload, which is a different answer from judging
    /// it and finding a mismatch.
    container_chip: Option<&'static [u8]>,
}

/// Every pinned reply: one entry per SoC a real board has answered for.
const PINNED: &[Soc] = &[
    // H96 Max M9, rkbin SPL loader v1.12.108, 2026-07-16: "6753" -- the SoC's
    // ASCII digits byte-reversed -- and twelve zeros.
    Soc {
        name: "rk3576",
        reply: &[
            0x36, 0x37, 0x35, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ],
        // Measured at offset 21 of four RK3576 containers: rkbin's own
        // `rk3576_spl_loader_v1.12.108.bin`, and three built by `boot_merger`
        // from an `.ini` whose `[CHIP_NAME] NAME=RK3576` is what puts the bytes
        // there. All four carry `"6753"`.
        container_chip: Some(b"6753"),
    },
    // NanoPi R6S, an RK3588S, 2026-10-05, through the usbplug loader in the
    // `MiniLoaderAll.bin` on FriendlyELEC's RK3588 eflasher card (2025-12-22):
    // "8853" -- the SoC's ASCII digits byte-reversed -- and twelve 0xff. The
    // RK3588S is the RK3588 in a smaller package, and answers as one.
    Soc {
        name: "rk3588",
        reply: &[
            0x38, 0x38, 0x35, 0x33, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff,
        ],
        // Measured at offset 21 of one container: the same `MiniLoaderAll.bin`,
        // whose loader gave the reply above.
        container_chip: Some(b"8853"),
    },
];

impl Soc {
    /// Look a name up: `"rk3576"`, `"RK3576"`, and `"3576"` all name the same
    /// entry.
    ///
    /// A name with no pinned entry returns [`Error::InvalidRequest`], whether it
    /// names an unmeasured Rockchip part or no part at all. The gate cannot compare
    /// against bytes it does not have. The refusal comes before a device is opened
    /// or a plan is made, so a write never carries a name the gate would refuse
    /// later.
    pub fn parse(name: &str) -> Result<Soc> {
        let trimmed = name.trim().to_ascii_lowercase();
        let bare = trimmed.strip_prefix("rk").unwrap_or(&trimmed);

        // Both sides drop a leading `rk` if they carry one, then compare. Stripping
        // only the entry side would make an entry whose name does not start with
        // `rk` -- a future non-Rockchip part, `jh7110` say -- unmatchable by any
        // name at all.
        PINNED
            .iter()
            .copied()
            .find(|soc| soc.name.strip_prefix("rk").unwrap_or(soc.name) == bare)
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "the chip-version reply for '{name}' is not pinned against hardware, so the \
                     wrong-loader gate cannot be armed for it. Pinned so far: {}. To pin a new \
                     SoC, read the chip version of a board whose part is known, and report the \
                     reply",
                    names()
                ))
            })
    }

    /// The canonical name: `"rk3576"`.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The pinned reply, whole.
    pub fn pinned_reply(&self) -> &'static [u8] {
        self.reply
    }

    /// The four bytes an RKBOOT container for this SoC carries, where one has
    /// been measured.
    pub fn container_chip(&self) -> Option<&'static [u8]> {
        self.container_chip
    }

    /// Whether a container's chip field claims this SoC.
    ///
    /// There are three answers:
    ///
    /// - `Some(true)`: the container claims this SoC.
    /// - `Some(false)`: it claims something else.
    /// - `None`: **nothing has pinned what a container for this SoC looks
    ///   like**. The question cannot be answered and must not be guessed at.
    ///
    /// A caller must not collapse `None` into either answer. A caller that
    /// collapsed it into `false` would refuse every upload for a SoC nobody has a
    /// container sample of. One that collapsed it into `true` would report a check
    /// it never made.
    pub fn claimed_by_container(&self, chip: &[u8]) -> Option<bool> {
        self.container_chip.map(|pinned| pinned == chip)
    }

    /// Whether the loader's answer matches this SoC's pinned reply exactly.
    ///
    /// Exactly means the same bytes and the same length. A reply with the right
    /// digits at a length nobody has seen comes from a loader build nobody has
    /// measured. It returns `false`, as any unknown reply does, until a board pins
    /// it.
    pub fn matches(&self, answered: &[u8]) -> bool {
        self.reply == answered
    }
}

/// The pinned SoC a container's chip field claims, or `None`.
///
/// It is the reverse lookup of [`Soc::claimed_by_container`]. It lets a front-end
/// say *this loader says it is for rk3576* from a file alone, with no SoC named
/// and no device open. `None` means no pinned SoC claims those bytes: an
/// unmeasured part, or an older container holding something else in the field.
/// It is not a finding against the file. It means only that the table has no
/// entry for those bytes.
pub fn by_container_chip(chip: &[u8]) -> Option<Soc> {
    PINNED
        .iter()
        .copied()
        .find(|soc| soc.container_chip == Some(chip))
}

/// The pinned names, comma-separated, for a message that lists them.
fn names() -> String {
    PINNED
        .iter()
        .map(|soc| soc.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_normalized_before_it_is_looked_up() {
        for spelling in ["rk3576", "RK3576", "3576", " rk3576 ", "Rk3576"] {
            let soc = Soc::parse(spelling).expect("every spelling names the pinned entry");
            assert_eq!(soc.name(), "rk3576", "{spelling}");
        }
    }

    /// There are three answers, and `None` is distinct from `Some(false)`.
    #[test]
    fn a_container_claim_is_matched_named_or_left_unjudged() {
        let rk3576 = Soc::parse("rk3576").expect("pinned");
        assert_eq!(rk3576.claimed_by_container(b"6753"), Some(true));
        assert_eq!(rk3576.claimed_by_container(b"8853"), Some(false));
        // An older container may hold an enumerated device type rather than
        // ASCII. That is not this SoC's pinned value, so it is a mismatch --
        // there is nothing tentative about bytes that are pinned.
        assert_eq!(
            rk3576.claimed_by_container(&[0x50, 0x00, 0x00, 0x00]),
            Some(false)
        );
    }

    /// The reverse lookup names what a file claims, with no device and no SoC
    /// argument. For bytes no entry pins, it returns `None` rather than guessing.
    #[test]
    fn a_container_claim_names_a_pinned_soc_or_nothing() {
        assert_eq!(
            by_container_chip(b"6753").map(|soc| soc.name()),
            Some("rk3576")
        );
        assert_eq!(
            by_container_chip(b"8853").map(|soc| soc.name()),
            Some("rk3588")
        );
        assert_eq!(
            by_container_chip(b"9933").map(|soc| soc.name()),
            None,
            "no entry pins these bytes, so nothing claims them"
        );
        assert_eq!(by_container_chip(&[]).map(|soc| soc.name()), None);
    }

    /// The container value is pinned in its own right, not computed from the
    /// chipver reply. The two agree on both SoCs where both are measured. This
    /// test asserts that agreement as an observation, not a derivation.
    #[test]
    fn the_container_value_is_pinned_separately_from_the_chipver_reply() {
        for (name, chip) in [("rk3576", b"6753"), ("rk3588", b"8853")] {
            let soc = Soc::parse(name).expect("pinned");
            let container = soc.container_chip().expect("measured from a file");
            assert_eq!(container, chip);
            assert_eq!(
                container,
                &soc.pinned_reply()[..4],
                "on {name} the two measurements agree; that is an observation, and \
                 nothing computes one from the other"
            );
        }
    }

    /// A SoC nobody has measured cannot be named. The refusal lists the pinned
    /// SoCs and says how to pin a new one.
    #[test]
    fn an_unpinned_soc_is_refused_with_the_pinned_list_named() {
        for stranger in ["rk3399", "rk3566", "banana"] {
            let error = Soc::parse(stranger).expect_err("no board has pinned this");
            let Error::InvalidRequest(message) = error else {
                panic!("an unpinned SoC is a caller problem, not a device one");
            };
            assert!(message.contains("rk3576"), "{message}");
            assert!(message.contains("read the chip version"), "{message}");
        }
    }

    /// The comparison is exact. The right digits at the wrong length come from a
    /// loader build nobody has measured, and they do not match.
    #[test]
    fn the_match_is_exact_bytes_not_a_shape() {
        let soc = Soc::parse("rk3576").expect("pinned");

        let measured = [0x36, 0x37, 0x35, 0x33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(soc.matches(&measured));

        assert!(!soc.matches(&[0x36, 0x37, 0x35, 0x33]), "short reply");
        assert!(
            !soc.matches(&[0x38, 0x38, 0x35, 0x33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            "another SoC's digits"
        );
        assert!(!soc.matches(&[]), "an empty reply");
    }

    /// The RK3588 reply is pinned with the tail its loader sent. The same digits
    /// with the RK3576's zero tail come from a loader nobody has measured.
    #[test]
    fn the_rk3588_reply_is_pinned_whole_with_its_tail() {
        let soc = Soc::parse("RK3588").expect("pinned");
        assert_eq!(soc.name(), "rk3588");

        let mut measured = vec![0x38, 0x38, 0x35, 0x33];
        measured.extend([0xff; 12]);
        assert!(soc.matches(&measured));

        let mut zero_tail = vec![0x38, 0x38, 0x35, 0x33];
        zero_tail.extend([0x00; 12]);
        assert!(!soc.matches(&zero_tail), "another tail");
        assert!(!soc.matches(&[0x38, 0x38, 0x35, 0x33]), "no tail");
    }
}
