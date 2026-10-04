//! The bootstrap that RAM-boots a StarFive JH7110 board into U-Boot over its
//! UART, writing nothing.
//!
//! A JH7110 strapped into UART recovery runs its BootROM and nothing else, and the
//! ROM takes one file by XMODEM. Sent a mainline SPL rather than StarFive's
//! recovery agent, the ROM runs it from SRAM. The SPL brings up DRAM, finds the
//! UART strap still latched, and asks for U-Boot by YMODEM. The board is then
//! running a full U-Boot in DRAM, and its flash and eMMC are untouched. This is
//! the counterpart of the Rockchip maskrom bootstrap, [`download_boot`], which
//! RAM-boots a U-Boot over USB.
//!
//! [`uart_boot`] carries out the whole of it, and leaves U-Boot stopped at its
//! prompt. A U-Boot left to its autoboot would boot whatever the board's storage
//! holds, which is rarely why somebody RAM-boots one. The prompt is where
//! [`uboot`](crate::uboot) takes over, on the same port.
//!
//! A U-Boot built with `CONFIG_CMD_USB_MASS_STORAGE` starts its `ums` gadget
//! there. The board's eMMC then appears to the host as a disk, and the Block
//! backend writes it with a read-back of every window. This is the verified route
//! to a JH7110 eMMC. The recovery agent's eMMC writes are not verified, and do not
//! match a disk image's layout.
//!
//! # The sequence
//!
//! 1. Wait for the ROM's run of `C`s, and send the SPL by XMODEM. The SPL can be a
//!    raw `u-boot-spl.bin` or a `.normal.out`, as for a recovery.
//! 2. Wait for the SPL to print `Trying to boot from UART`. A vendor SPL loads
//!    U-Boot from SPI flash whatever the strap says, and names that device instead.
//! 3. Wait for the SPL's `C`, and send `u-boot.itb` as a one-file YMODEM batch.
//! 4. Wait for U-Boot's autoboot countdown, or its prompt, and stop the countdown.
//!
//! The SPL needs `CONFIG_SPL_YMODEM_SUPPORT`, which mainline's VisionFive 2
//! defconfig sets. **\[DOC\]** (mainline U-Boot's JH7110 documentation, and the
//! YMODEM receiver in its `xyzModem.c`, which the sender here follows.)
//!
//! # Hardware verification
//!
//! No JH7110 board has run this bootstrap. A scripted serial pins it, printing
//! what the ROM, the SPL and U-Boot print. The YMODEM answers it expects are the
//! ones U-Boot's receiver gives in its code. It is **\[UNVERIFIED\]** until a
//! board settles it.
//!
//! [`download_boot`]: super::download_boot

use crate::codec::console::{Console, text};
use crate::codec::splhdr::Origin;
use crate::console::{self, ConsoleSink, Patience};
use crate::modem;
use crate::progress::{Cancel, ProgressSink};
use crate::recovery::{check_uboot, prepare_spl, send_to_rom};
use crate::transport::Serial;
use crate::uboot::{AUTOBOOT_BANNER, UBoot};
use crate::{Error, Result};

/// What the SPL prints before it names the device it loads U-Boot from.
const TRYING: &[u8] = b"Trying to boot from ";
/// The device name a RAM boot needs the SPL to print after [`TRYING`].
const FROM_UART: &str = "UART";
/// What U-Boot's SPL prints when it found nothing to boot, and what U-Boot prints
/// when it stops on a fatal error.
const GAVE_UP: [&[u8]; 2] = [
    b"SPL: failed to boot from all boot devices",
    b"### ERROR ### Please RESET the board ###",
];

/// How long to wait for the SPL to bring up DRAM and say where it loads U-Boot
/// from.
const SPL_START: Patience = Patience {
    silent_reads: 30,
    max_bytes: 64 * 1024,
};
/// How long to wait for the rest of a line the SPL is printing.
const LINE: Patience = Patience {
    silent_reads: 5,
    max_bytes: 256,
};
/// How long to wait for the SPL's YMODEM receiver to ask for the file. It asks at
/// once, and again every few seconds.
const RECEIVER_START: Patience = Patience {
    silent_reads: 10,
    max_bytes: 4096,
};
/// How long to wait for U-Boot to come up after the transfer. OpenSBI and U-Boot
/// both print banners on the way.
const UBOOT_START: Patience = Patience {
    silent_reads: 30,
    max_bytes: 256 * 1024,
};

/// The two files a RAM boot sends, borrowed for the length of the call.
pub struct UartBootRequest<'a> {
    /// The SPL: a raw `u-boot-spl.bin`, which is headered here, or a
    /// `u-boot-spl.bin.normal.out`, whose header is checked and kept. It must be
    /// built to load U-Boot by YMODEM.
    pub spl: &'a [u8],
    /// The U-Boot FIT (`u-boot.itb`), sent as-is.
    pub uboot: &'a [u8],
}

/// What a RAM boot would send, worked out before anything is.
///
/// It needs no confirmation, because a RAM boot writes nothing. A front-end uses
/// it to say what is about to be sent. Making it also refuses a file that is not
/// what it was given as, before the board is powered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UartBootPlan {
    /// How many bytes the SPL transfer carries, header included.
    pub spl_bytes: u64,
    /// Whether the SPL's header came with the file or is built here.
    pub spl_origin: Origin,
    /// How many bytes the U-Boot transfer carries.
    pub uboot_bytes: u64,
}

/// Check a RAM boot's two files, and say what would be sent.
///
/// It is pure. It refuses what [`plan_recover`](crate::recovery::plan_recover)
/// refuses of the same files:
///
/// - An SPL whose header does not check
/// - An SPL that is a FIT image, the shape of a U-Boot payload
/// - A U-Boot payload that is not a FIT image
pub fn plan_uart_boot(request: &UartBootRequest<'_>) -> Result<UartBootPlan> {
    let spl = prepare_spl(request.spl)?;
    check_uboot(request.uboot)?;
    Ok(UartBootPlan {
        spl_bytes: spl.image.len() as u64,
        spl_origin: spl.origin,
        uboot_bytes: request.uboot.len() as u64,
    })
}

/// RAM-boot U-Boot over `serial`, and leave it at its prompt.
///
/// The board must be strapped into UART recovery. `prompt` is the prompt the
/// U-Boot being sent presents, [`DEFAULT_PROMPT`](crate::uboot::DEFAULT_PROMPT)
/// for a mainline build. Progress is reported per transfer. What the ROM, the SPL,
/// OpenSBI and U-Boot print outside a transfer goes to `sink` as it arrives.
/// [`cancel`](Cancel) is checked between blocks and between waits.
///
/// Nothing is written to the board's storage. On success the line can be handed to
/// [`UBoot`] to start a gadget.
pub async fn uart_boot<S: Serial>(
    serial: &mut S,
    request: &UartBootRequest<'_>,
    prompt: &str,
    progress: ProgressSink<'_>,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    plan_uart_boot(request)?;
    let spl = prepare_spl(request.spl)?;
    let mut console = Console::new();

    send_to_rom(
        &mut *serial,
        &mut console,
        &spl.image,
        progress,
        &mut *sink,
        cancel,
    )
    .await?;

    // Where the SPL loads U-Boot from. A RAM boot needs the UART, and an SPL that
    // names anything else will never ask for the file.
    let found = console::look_for_patiently(
        &mut *serial,
        &mut console,
        &[TRYING, GAVE_UP[0]],
        SPL_START,
        &mut *sink,
        cancel,
    )
    .await?;
    let device = match found {
        Some(found) if found.pattern == 0 => {
            console.consume(&found);
            rest_of_line(serial, &mut console, sink, cancel).await?
        }
        _ => {
            return Err(Error::Protocol(format!(
                "the SPL did not say where it loads U-Boot from. Recent console output:\n{}",
                console.tail_text()
            )));
        }
    };
    if device != FROM_UART {
        return Err(Error::Protocol(format!(
            "the SPL is loading U-Boot from {device}, not from the UART, so it will not ask for \
             the file. A RAM boot needs a mainline SPL built with CONFIG_SPL_YMODEM_SUPPORT, and \
             the board still strapped into UART recovery. StarFive's own SPL loads U-Boot from \
             SPI flash."
        )));
    }

    let asked = console::look_for_patiently(
        &mut *serial,
        &mut console,
        &[b"C"],
        RECEIVER_START,
        &mut |_| {},
        cancel,
    )
    .await?;
    let Some(asked) = asked else {
        return Err(Error::Protocol(format!(
            "the SPL said it would load U-Boot from the UART, and did not ask for it. Recent \
             console output:\n{}",
            console.tail_text()
        )));
    };
    console.consume(&asked);

    modem::send_ymodem(
        &mut *serial,
        &mut console,
        request.uboot,
        progress,
        &mut *sink,
        cancel,
    )
    .await?;

    // U-Boot's countdown, or a prompt from a build with no countdown. Stopping it
    // is the U-Boot driver's job, on the same line.
    let up = console::look_for_patiently(
        &mut *serial,
        &mut console,
        &[AUTOBOOT_BANNER, prompt.as_bytes(), GAVE_UP[0], GAVE_UP[1]],
        UBOOT_START,
        &mut *sink,
        cancel,
    )
    .await?;
    match up {
        Some(found) if found.pattern < 2 => {}
        Some(_) => {
            return Err(Error::Protocol(format!(
                "U-Boot was sent and did not start. Recent console output:\n{}",
                console.tail_text()
            )));
        }
        None => {
            return Err(Error::Protocol(format!(
                "U-Boot was sent, and neither its autoboot countdown nor the prompt \"{prompt}\" \
                 appeared. If its prompt is another, name it. Recent console output:\n{}",
                console.tail_text()
            )));
        }
    }

    UBoot::new(&mut *serial)
        .with_prompt(prompt)
        .interrupt_autoboot(sink, cancel)
        .await
}

/// Read the rest of the line the SPL is printing, and return it trimmed.
async fn rest_of_line<S: Serial>(
    serial: &mut S,
    console: &mut Console,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<String> {
    let found = console::look_for_patiently(serial, console, &[b"\n"], LINE, sink, cancel).await?;
    let Some(found) = found else {
        return Err(Error::Protocol(format!(
            "the SPL stopped partway through a line. Recent console output:\n{}",
            console.tail_text()
        )));
    };
    let line = text(console.before(&found)).trim().to_string();
    console.consume(&found);
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::splhdr;
    use crate::codec::xmodem::{self, BlockSize};
    use crate::transport::testing::{ScriptedSerial, SerialStep};

    fn spl() -> Vec<u8> {
        (0..300u32).map(|i| (i * 7 + 3) as u8).collect()
    }

    fn uboot() -> Vec<u8> {
        let mut fit = vec![0xd0, 0x0d, 0xfe, 0xed];
        fit.extend((0..1500u32).map(|i| (i * 13) as u8));
        fit
    }

    /// The ROM asking, and the SPL sent to it in 128-byte blocks.
    fn rom_takes(image: &[u8]) -> Vec<SerialStep> {
        let mut steps = vec![SerialStep::Rx(b"(C)StarFive\r\nCCCCCCCCCCCC".to_vec())];
        for (i, chunk) in image.chunks(128).enumerate() {
            steps.push(SerialStep::ExpectTx(xmodem::block(
                BlockSize::Small,
                (i + 1) as u8,
                chunk,
            )));
            steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        }
        steps.push(SerialStep::ExpectTx(vec![xmodem::EOT]));
        steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        steps
    }

    /// U-Boot's SPL taking `uboot` by YMODEM, as its receiver's code answers.
    fn spl_takes(uboot: &[u8]) -> Vec<SerialStep> {
        let mut steps = vec![
            SerialStep::ExpectTx(xmodem::ymodem_header("u-boot.itb", uboot.len() as u64)),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::Timeout,
            SerialStep::Rx(vec![xmodem::CRC_REQUEST]),
        ];
        for (i, chunk) in uboot.chunks(1024).enumerate() {
            steps.push(SerialStep::ExpectTx(xmodem::block(
                BlockSize::Large,
                (i + 1) as u8,
                chunk,
            )));
            steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        }
        steps.push(SerialStep::ExpectTx(vec![xmodem::EOT]));
        steps.push(SerialStep::Rx(vec![
            xmodem::ACK,
            xmodem::ACK,
            xmodem::CRC_REQUEST,
        ]));
        steps.push(SerialStep::ExpectTx(xmodem::ymodem_end()));
        steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        steps
    }

    fn run(spl: &[u8], uboot: &[u8], steps: Vec<SerialStep>) -> (Result<()>, String) {
        let mut serial = ScriptedSerial::new(steps);
        let mut said = Vec::new();
        let result = pollster::block_on(uart_boot(
            &mut serial,
            &UartBootRequest { spl, uboot },
            "=> ",
            &mut |_| {},
            &mut |bytes| said.extend_from_slice(bytes),
            &Cancel::new(),
        ));
        if result.is_ok() {
            serial.assert_drained();
        }
        (result, text(&said))
    }

    /// The whole RAM boot, on the wire: the SPL to the ROM, `u-boot.itb` to the
    /// SPL once it has said it boots from the UART and asked, and U-Boot's countdown
    /// stopped with a carriage return.
    #[test]
    fn a_ram_boot_sends_the_spl_then_u_boot_and_stops_at_the_prompt() {
        let spl = spl();
        let uboot = uboot();
        let mut steps = rom_takes(&splhdr::build(&spl));
        steps.push(SerialStep::Rx(
            b"\r\nU-Boot SPL 2025.10 (Oct 01 2026 - 12:00:00 +0000)\r\nDRAM:  4 GiB\r\n\
              Trying to boot from "
                .to_vec(),
        ));
        steps.push(SerialStep::Rx(b"UART\r\nC".to_vec()));
        steps.extend(spl_takes(&uboot));
        steps.push(SerialStep::Rx(
            b"Loaded 1504 bytes\r\n\r\nOpenSBI v1.5\r\n".to_vec(),
        ));
        steps.push(SerialStep::Rx(
            b"\r\nU-Boot 2025.10\r\nHit any key to stop autoboot:  2 ".to_vec(),
        ));
        steps.push(SerialStep::ExpectTx(b"\r".to_vec()));
        steps.push(SerialStep::Rx(b"\x08\x08\x08 0 \r\n=> ".to_vec()));

        let (result, said) = run(&spl, &uboot, steps);
        result.expect("the RAM boot follows the script");
        assert!(said.contains("Trying to boot from UART"), "{said}");
        assert!(said.contains("OpenSBI"), "{said}");
    }

    /// An SPL that loads U-Boot from somewhere else is reported by the device it
    /// named, and U-Boot is not sent.
    #[test]
    fn an_spl_that_does_not_boot_from_the_uart_is_reported_and_nothing_more_sent() {
        let spl = spl();
        let uboot = uboot();
        let mut steps = rom_takes(&splhdr::build(&spl));
        steps.push(SerialStep::Rx(
            b"U-Boot SPL 2021.10\r\nTrying to boot from SPI\r\n".to_vec(),
        ));
        let (result, _) = run(&spl, &uboot, steps);
        let Err(Error::Protocol(message)) = result else {
            panic!("{result:?}");
        };
        assert!(message.contains("from SPI"), "{message}");
        assert!(message.contains("CONFIG_SPL_YMODEM_SUPPORT"), "{message}");
    }

    /// A U-Boot that stops on a fatal error is reported, and the transcript says
    /// why.
    #[test]
    fn a_u_boot_that_stops_on_an_error_is_reported() {
        let spl = spl();
        let uboot = uboot();
        let mut steps = rom_takes(&splhdr::build(&spl));
        steps.push(SerialStep::Rx(b"Trying to boot from UART\r\nC".to_vec()));
        steps.extend(spl_takes(&uboot));
        steps.push(SerialStep::Rx(
            b"initcall failed\r\n### ERROR ### Please RESET the board ###\r\n".to_vec(),
        ));
        let (result, _) = run(&spl, &uboot, steps);
        let Err(Error::Protocol(message)) = result else {
            panic!("{result:?}");
        };
        assert!(message.contains("did not start"), "{message}");
        assert!(message.contains("initcall failed"), "{message}");
    }

    /// The files are checked before the board is asked anything: the SPL and the
    /// U-Boot given in each other's place are both refused.
    #[test]
    fn files_given_in_each_others_place_are_refused_before_anything_is_sent() {
        let spl = spl();
        let uboot = uboot();
        let mut serial = ScriptedSerial::new(vec![]);
        let swapped = UartBootRequest {
            spl: &uboot,
            uboot: &spl,
        };
        assert!(plan_uart_boot(&swapped).is_err());
        let result = pollster::block_on(uart_boot(
            &mut serial,
            &swapped,
            "=> ",
            &mut |_| {},
            &mut |_| {},
            &Cancel::new(),
        ));
        assert!(
            matches!(result, Err(Error::InvalidRequest(_))),
            "{result:?}"
        );
    }

    /// The plan says what would go, and whether the SPL's header came with it.
    #[test]
    fn the_plan_counts_the_headered_spl() {
        let spl = spl();
        let uboot = uboot();
        let plan = plan_uart_boot(&UartBootRequest {
            spl: &spl,
            uboot: &uboot,
        })
        .unwrap();
        assert_eq!(plan.spl_bytes, (splhdr::HEADER_LEN + spl.len()) as u64);
        assert_eq!(plan.spl_origin, Origin::HeaderedHere);
        assert_eq!(plan.uboot_bytes, uboot.len() as u64);
    }
}
