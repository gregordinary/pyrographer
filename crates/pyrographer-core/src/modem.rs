//! Sending a file over a serial line: the XMODEM and YMODEM senders.
//!
//! This module is the I/O half of [`codec::xmodem`](crate::codec::xmodem), which
//! frames the blocks. It sends them and reads the receiver's answers. Two drivers
//! call it. [`recovery`](crate::recovery) sends files to the JH7110 ROM and to
//! StarFive's recovery agent. [`bootstrap::starfive`] sends an SPL to the ROM, and
//! then U-Boot to that SPL.
//!
//! # Starting
//!
//! A sender here starts on the caller's word. The caller has already read the
//! receiver's request to begin, from the text the receiver printed, and consumed it
//! from the [`Console`]. A sender that started on the first `C` byte it saw would
//! start too early. That `C` can be the one in the ROM's banner, `(C)StarFive`, or
//! in the recovery agent's `CPU freq` line, while the receiver is not listening.
//!
//! # Answers
//!
//! The sender holds its position until the receiver answers:
//!
//! - `ACK` accepts the block, and the sender moves on.
//! - `NAK` asks for the block again. A burst of `NAK`s read together asks once.
//!   The JH7110 ROM is known to send `NAK`s in bursts, and a sender that resent
//!   once per `NAK` would fall out of step with it.
//! - `C` before the first block is accepted is left over from the receiver's
//!   request to begin, and means nothing. After it, `C` asks for the block again.
//!   U-Boot's receiver asks that way in CRC mode.
//! - `CAN` ends the transfer.
//! - Silence for the length of one read asks for the block again, the classic
//!   XMODEM sender timeout.
//!
//! The sender resends the identical block, and never advances on anything but an
//! `ACK`. It gives up after [`MAX_RESENDS`] resends of one block.
//!
//! # Anything else
//!
//! What a receiver does with bytes that are not a block depends on the receiver,
//! and [`Receiver`] says which one the caller is sending to. The ROM and an SPL
//! read nothing but blocks, and a byte that is not an answer is noise to them and
//! to the sender. StarFive's recovery agent reads a line of input whenever it is
//! not receiving a file, and **echoes it**. A block sent while the agent reads a
//! line is typing, and a typed line can reach its OTP fuse menu. Against the agent,
//! a byte that is not an answer means the agent is not receiving. The sender then
//! stops at once, before another block goes out.
//!
//! # Block sizes
//!
//! The sender sends 128-byte blocks to the ROM and to the recovery agent, the size
//! StarFive's own procedure uses. A larger block does not make a transfer much
//! faster, because the time goes on the line's 115200 baud rather than on the
//! round trips. It does raise how many bytes the agent would read as typing if a
//! block ever reached it while it was not receiving. YMODEM to an SPL uses 1 KB
//! blocks, as YMODEM conventionally does. An SPL types nothing.
//!
//! [`bootstrap::starfive`]: crate::bootstrap::starfive

use crate::codec::console::Console;
use crate::codec::xmodem::{self, BlockSize};
use crate::console::{self, ConsoleSink};
use crate::progress::{Cancel, Progress, ProgressSink};
use crate::transport::Serial;
use crate::{Error, Result};

/// How many resends of one block (or the `EOT`) before the transfer is declared
/// failed.
///
/// The JH7110 ROM's XMODEM is known to over-`NAK`, so a low cap would abort a
/// transfer that a patient sender would complete. A cap is still required.
/// Retrying forever into a receiver that never accepts the block makes a recovery
/// hang instead of fail.
pub const MAX_RESENDS: u32 = 20;

/// How many reads that bring only noise to spend on one answer before resending.
const NOISE_READS: u32 = 32;

/// How many reads to spend waiting for a YMODEM receiver's `C` between the parts of
/// a batch. U-Boot's receiver sends it after two seconds of silence.
const BATCH_READS: u32 = 10;

/// The file name a YMODEM header carries. U-Boot's receiver reads the length from
/// the header and skips the name.
const YMODEM_NAME: &str = "u-boot.itb";

/// Who a file is sent to, which decides what a byte that is not an answer means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receiver {
    /// The JH7110 BootROM. It reads nothing but blocks, so a byte that is not an
    /// answer is noise. It also prints its banner between bursts of `C`, which can
    /// arrive while the first block is on its way.
    BootRom,
    /// StarFive's recovery agent. Anything that is not an answer means it is no
    /// longer receiving, and is reading a line, which it echoes. Its line reader can
    /// reach its OTP fuse menu.
    RecoveryAgent,
    /// A U-Boot SPL receiving by YMODEM. It reads nothing but blocks.
    Spl,
}

impl Receiver {
    /// Whether this receiver reads stray bytes as typing.
    fn types(self) -> bool {
        self == Receiver::RecoveryAgent
    }
}

/// What the receiver said after a block.
enum Reply {
    /// It accepted the block, and the sender moves on.
    Accepted,
    /// It wants the block again, or has gone quiet long enough to warrant a resend.
    /// Either way the *same* block is sent again.
    Again,
}

/// One transfer's sender: the line, the transcript, and what the receiver has
/// said so far.
struct Sender<'a, S> {
    serial: &'a mut S,
    console: &'a mut Console,
    receiver: Receiver,
    /// Whether the receiver has accepted a block yet. Before it has, a `C` is left
    /// over from its request to begin. After, a `C` asks for the block again.
    accepted_any: bool,
}

impl<S: Serial> Sender<'_, S> {
    /// Send `frame`, and send it again until the receiver accepts it, or give up.
    ///
    /// `what` names what is being sent, for the error a person reads. On success,
    /// the bytes that followed the receiver's `ACK` are in the console after the
    /// cursor, and the control bytes before it are behind the cursor.
    async fn send(&mut self, frame: &[u8], what: &str) -> Result<()> {
        for _ in 0..=MAX_RESENDS {
            self.serial.write_all(frame).await?;
            match self.reply().await? {
                Reply::Accepted => {
                    self.accepted_any = true;
                    return Ok(());
                }
                // Resend the same bytes. Nothing touches the sequence number or the
                // data, which is the invariant a NAK burst must not break.
                Reply::Again => continue,
            }
        }
        Err(Error::Protocol(format!(
            "the receiver did not accept {what} after {MAX_RESENDS} resends. The JH7110 ROM's \
             XMODEM is known to send NAKs aggressively. A transfer that does not complete is \
             reported rather than retried, because further retries risk bricking the board. Recent \
             console output:\n{}",
            self.console.tail_text()
        )))
    }

    /// Read the receiver's answer to what was just sent.
    ///
    /// The first answer byte in the stream decides: an `ACK` anywhere after leftover
    /// `C`s accepts the block, and a burst of `NAK`s is one request. Every byte read
    /// goes into the console's transcript. Bytes up to the deciding one are put
    /// behind the cursor, and bytes after it are left for the caller's next wait.
    async fn reply(&mut self) -> Result<Reply> {
        let mut buf = [0u8; 64];
        for _ in 0..NOISE_READS {
            let n = match self.serial.read(&mut buf).await {
                Ok(n) => n,
                // The receiver said nothing in a read's time. In XMODEM that is a
                // resend trigger, bounded by the caller's resend cap.
                Err(Error::Timeout { .. }) => return Ok(Reply::Again),
                Err(other) => return Err(other),
            };
            let chunk = &buf[..n];
            for (i, &byte) in chunk.iter().enumerate() {
                let decided = match byte {
                    xmodem::ACK => Some(Reply::Accepted),
                    xmodem::NAK => Some(Reply::Again),
                    xmodem::CRC_REQUEST if self.accepted_any => Some(Reply::Again),
                    xmodem::CRC_REQUEST => None,
                    xmodem::CAN => {
                        self.console.push(chunk);
                        return Err(Error::Protocol(format!(
                            "the receiver canceled the transfer (CAN). Recent console output:\n{}",
                            self.console.tail_text()
                        )));
                    }
                    _ if self.receiver.types() => {
                        self.console.push(chunk);
                        return Err(out_of_step(self.console));
                    }
                    // Noise to a receiver that reads only blocks.
                    _ => None,
                };
                if let Some(reply) = decided {
                    self.console.push(&chunk[..=i]);
                    self.console.skip_to_end();
                    self.console.push(&chunk[i + 1..]);
                    return Ok(reply);
                }
            }
            self.console.push(chunk);
        }
        Ok(Reply::Again)
    }

    /// Send `data` as numbered blocks of `size`, from block 1, and then `EOT`.
    ///
    /// [`cancel`](Cancel) is checked between blocks, so a canceled transfer stops at
    /// a block boundary and never inside a block.
    async fn blocks(
        &mut self,
        data: &[u8],
        size: BlockSize,
        progress: ProgressSink<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        let total_bytes = data.len() as u64;
        progress(Progress::Started { total_bytes });

        let mut seq: u8 = 1;
        let mut done: u64 = 0;
        for chunk in data.chunks(size.data_len()) {
            if cancel.is_canceled() {
                return Err(Error::Canceled);
            }
            self.send(&xmodem::block(size, seq, chunk), "a block")
                .await?;
            seq = seq.wrapping_add(1);
            done += chunk.len() as u64;
            progress(Progress::Advanced {
                done_bytes: done,
                total_bytes,
            });
        }

        self.send(&[xmodem::EOT], "the end of the transfer").await?;
        progress(Progress::Finished { done_bytes: done });
        Ok(())
    }

    /// Wait for a YMODEM receiver's `C`, which asks for the next part of a batch.
    ///
    /// It can already be in the console, read with the `ACK` before it.
    async fn batch_request(&mut self, cancel: &Cancel) -> Result<()> {
        let found = console::look_for(
            &mut *self.serial,
            self.console,
            &[&[xmodem::CRC_REQUEST]],
            BATCH_READS,
            &mut |_| {},
            cancel,
        )
        .await?;
        match found {
            Some(found) => {
                self.console.consume(&found);
                Ok(())
            }
            None => Err(Error::Protocol(format!(
                "the YMODEM receiver stopped asking for the next part of the transfer. Recent \
                 console output:\n{}",
                self.console.tail_text()
            ))),
        }
    }
}

/// Send `data` to an XMODEM-CRC receiver that has asked to begin.
///
/// The caller has read the receiver's request and consumed it from `console`. The
/// transfer sends 128-byte blocks from block 1 and ends with `EOT`. Whatever the
/// receiver printed after accepting the `EOT` goes to `sink` and stays in `console`
/// for the caller's next wait.
pub async fn send_xmodem<S: Serial>(
    serial: &mut S,
    console: &mut Console,
    data: &[u8],
    receiver: Receiver,
    progress: ProgressSink<'_>,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let mut sender = Sender {
        serial,
        console,
        receiver,
        accepted_any: false,
    };
    sender
        .blocks(data, BlockSize::Small, progress, cancel)
        .await?;
    sink(unread(sender.console));
    Ok(())
}

/// Send `data` as a one-file YMODEM batch to a receiver that has asked to begin.
///
/// The caller has read the receiver's request and consumed it from `console`. The
/// batch opens with block 0, which carries the file's name and length. The file
/// follows in 1 KB blocks and an `EOT`, and the empty block 0 ends the batch. Before the file and
/// before the empty block, the receiver asks again with a `C`.
pub async fn send_ymodem<S: Serial>(
    serial: &mut S,
    console: &mut Console,
    data: &[u8],
    progress: ProgressSink<'_>,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let mut sender = Sender {
        serial,
        console,
        receiver: Receiver::Spl,
        accepted_any: false,
    };

    let header = xmodem::ymodem_header(YMODEM_NAME, data.len() as u64);
    sender.send(&header, "the file's header block").await?;
    sender.batch_request(cancel).await?;

    sender
        .blocks(data, BlockSize::Large, progress, cancel)
        .await?;

    sender.batch_request(cancel).await?;
    sender
        .send(&xmodem::ymodem_end(), "the block that ends the batch")
        .await?;
    sink(unread(sender.console));
    Ok(())
}

/// The bytes in `console` after its cursor: what the receiver printed after its
/// last answer.
fn unread(console: &Console) -> &[u8] {
    let transcript = console.transcript();
    let unread = (console.stream_len() - console.cursor()) as usize;
    &transcript[transcript.len() - unread.min(transcript.len())..]
}

/// The error for a receiver that answered a block with text.
fn out_of_step(console: &Console) -> Error {
    Error::Protocol(format!(
        "the recovery agent answered a block with text, so it is reading input rather than \
         receiving a file. pyrographer stopped sending at once, because the agent reads what it \
         is sent as typing. Recent console output:\n{}",
        console.tail_text()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testing::{ScriptedSerial, SerialStep};

    const SMALL: BlockSize = BlockSize::Small;

    /// The receiver's side of a clean XMODEM send of `data`: an `ACK` for each block
    /// and for the `EOT`. Each block's sequence number comes from its index by
    /// modular arithmetic, not from the sender's own `wrapping_add`, so the wrap at
    /// 256 is pinned by an independent formula.
    fn acked_blocks(data: &[u8], size: BlockSize) -> Vec<SerialStep> {
        let mut steps = Vec::new();
        for (i, chunk) in data.chunks(size.data_len()).enumerate() {
            let seq = ((i + 1) % 256) as u8;
            steps.push(SerialStep::ExpectTx(xmodem::block(size, seq, chunk)));
            steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        }
        steps.push(SerialStep::ExpectTx(vec![xmodem::EOT]));
        steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        steps
    }

    /// Drive `send_xmodem` alone against `steps`.
    fn run_xmodem(data: &[u8], receiver: Receiver, steps: Vec<SerialStep>) -> Result<Console> {
        let mut serial = ScriptedSerial::new(steps);
        let mut console = Console::new();
        pollster::block_on(send_xmodem(
            &mut serial,
            &mut console,
            data,
            receiver,
            &mut |_| {},
            &mut |_| {},
            &Cancel::new(),
        ))?;
        serial.assert_drained();
        Ok(console)
    }

    /// A multi-block send, on the wire: a block per 128 bytes numbered from 1, then
    /// `EOT`. The scripted serial asserts every byte, so a block built wrong fails
    /// here.
    #[test]
    fn a_send_frames_the_blocks_and_the_eot() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 5 + 1) as u8).collect();
        run_xmodem(&data, Receiver::RecoveryAgent, acked_blocks(&data, SMALL))
            .expect("a clean send follows the script");
    }

    /// An end-to-end send that crosses the block-255 wrap. It sends 257 blocks, so
    /// the sender's own counter produces 1..255, 0, 1, checked against an
    /// expectation that numbers blocks by index.
    #[test]
    fn a_send_crosses_the_block_255_wrap() {
        let data = vec![0xa9u8; 257 * 128];
        run_xmodem(&data, Receiver::BootRom, acked_blocks(&data, SMALL))
            .expect("the wrap was crossed cleanly");
    }

    /// The NAK tolerance. The receiver answers block 1 with a burst of NAKs in one
    /// read. The sender resends the identical block 1 exactly once, as the single
    /// second `ExpectTx(block 1)` asserts, and moves on only on the ACK.
    #[test]
    fn a_nak_burst_resends_the_same_block_once_and_does_not_advance() {
        let data: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let steps = vec![
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data[..128])),
            SerialStep::Rx(vec![xmodem::NAK, xmodem::NAK, xmodem::NAK]),
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data[..128])),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(xmodem::block(SMALL, 2, &data[128..])),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        run_xmodem(&data, Receiver::BootRom, steps)
            .expect("a NAK burst is tolerated, not doubled or advanced past");
    }

    /// Leftover `C`s from the request to begin arrive while the sender waits for
    /// the first block's ACK. They mean nothing, and the ACK in the same read
    /// accepts the block. This holds for the strict agent too: a `C` is an answer.
    #[test]
    fn leftover_cs_before_the_first_ack_are_ignored() {
        let data = vec![0x7eu8; 64];
        let steps = vec![
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data)),
            SerialStep::Rx(vec![xmodem::CRC_REQUEST, xmodem::CRC_REQUEST, xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        run_xmodem(&data, Receiver::RecoveryAgent, steps)
            .expect("leftover Cs do not derail the send");
    }

    /// After a block has been accepted, a `C` asks for the next block again. U-Boot's
    /// receiver answers a damaged block that way in CRC mode, and a sender that took
    /// the `C` for noise would wait out a read's time before resending.
    #[test]
    fn a_c_after_the_first_accepted_block_asks_for_the_block_again() {
        let data = vec![0x42u8; 200];
        let steps = vec![
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data[..128])),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(xmodem::block(SMALL, 2, &data[128..])),
            SerialStep::Rx(vec![xmodem::CRC_REQUEST]),
            SerialStep::ExpectTx(xmodem::block(SMALL, 2, &data[128..])),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        run_xmodem(&data, Receiver::BootRom, steps).expect("the block was sent again");
    }

    /// The ROM prints its banner between bursts of `C`, and the banner can arrive
    /// while the first block is on its way. To a tolerant receiver it is noise.
    #[test]
    fn a_tolerant_receiver_can_print_before_it_answers() {
        let data = vec![0x11u8; 20];
        let steps = vec![
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data)),
            SerialStep::Rx(b"(C)StarFive\r\n".to_vec()),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        run_xmodem(&data, Receiver::BootRom, steps).expect("the banner is noise here");
    }

    /// **The echo guard.** The recovery agent answers a block with text: its own
    /// menu, or the echo of the block it read as typing. The sender stops at once
    /// and sends nothing more. The script holds no step after the text, so a sender
    /// that sent another block would run off its end and panic.
    #[test]
    fn a_strict_receiver_that_answers_with_text_stops_the_send_at_once() {
        let data = vec![0x34u8; 300];
        let echo = xmodem::block(SMALL, 1, &data[..128]);
        let steps = vec![
            SerialStep::ExpectTx(echo.clone()),
            // The agent echoes the block back, byte for byte. It begins with SOH.
            SerialStep::Rx(echo[..64].to_vec()),
        ];
        let error =
            run_xmodem(&data, Receiver::RecoveryAgent, steps).expect_err("the agent is typing");
        assert!(
            matches!(&error, Error::Protocol(m) if m.contains("reading input")),
            "{error:?}"
        );
    }

    /// The text the agent prints after accepting the `EOT` is the start of its
    /// write. It follows the ACK in the same read, it is not an answer, and it stays
    /// in the console for the next wait rather than stopping the send.
    #[test]
    fn text_after_the_eot_ack_is_left_for_the_next_wait() {
        let data = vec![0x55u8; 10];
        let mut tail = vec![xmodem::ACK];
        tail.extend_from_slice(b"updata first section\r\n");
        let steps = vec![
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data)),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(tail),
        ];
        let console = run_xmodem(&data, Receiver::RecoveryAgent, steps).expect("a clean send");
        assert!(
            console.find(&[b"updata first section"]).is_some(),
            "the agent's text is after the cursor"
        );
        assert!(
            console.find(&[&[xmodem::ACK]]).is_none(),
            "and the answers are behind it"
        );
    }

    /// A receiver that never accepts a block is given up on after the resend cap,
    /// rather than retried forever.
    #[test]
    fn a_receiver_that_never_accepts_is_given_up_on() {
        let data = vec![0u8; 16];
        let frame = xmodem::block(SMALL, 1, &data);
        let mut steps = Vec::new();
        for _ in 0..=MAX_RESENDS {
            steps.push(SerialStep::ExpectTx(frame.clone()));
            steps.push(SerialStep::Rx(vec![xmodem::NAK]));
        }
        let error = run_xmodem(&data, Receiver::BootRom, steps).expect_err("never accepted");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }

    /// A `CAN` ends the transfer at once.
    #[test]
    fn a_can_ends_the_transfer() {
        let data = vec![0u8; 16];
        let steps = vec![
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data)),
            SerialStep::Rx(vec![xmodem::CAN, xmodem::CAN]),
        ];
        let error = run_xmodem(&data, Receiver::BootRom, steps).expect_err("canceled");
        assert!(
            matches!(&error, Error::Protocol(m) if m.contains("CAN")),
            "{error:?}"
        );
    }

    /// Cancellation stops the send at a block boundary. The token is set as the
    /// first block's progress is reported. A send that ignored it would try a second
    /// block and run off the end of the script.
    #[test]
    fn a_canceled_send_stops_at_the_next_block_boundary() {
        let data = vec![0x11u8; 300];
        let mut serial = ScriptedSerial::new(vec![
            SerialStep::ExpectTx(xmodem::block(SMALL, 1, &data[..128])),
            SerialStep::Rx(vec![xmodem::ACK]),
        ]);
        let cancel = Cancel::new();
        let mut console = Console::new();
        let error = pollster::block_on(send_xmodem(
            &mut serial,
            &mut console,
            &data,
            Receiver::BootRom,
            &mut |event| {
                if let Progress::Advanced { .. } = event {
                    cancel.cancel();
                }
            },
            &mut |_| {},
            &cancel,
        ))
        .expect_err("the send was canceled after the first block");
        assert!(matches!(error, Error::Canceled), "{error:?}");
    }

    /// A whole YMODEM batch against U-Boot's receiver, as its code runs it:
    ///
    /// - The header block 0 is answered with an `ACK` and, two seconds later, a `C`.
    /// - The file goes in 1 KB blocks, the last one padded.
    /// - The `EOT` is answered `ACK`, `ACK`, `C`: U-Boot acknowledges it twice and
    ///   asks for the next header.
    /// - The empty block 0 ends the batch, and is answered with an `ACK`.
    ///
    /// The SPL's own text after the last `ACK` goes to the sink.
    #[test]
    fn a_ymodem_batch_follows_u_boots_receiver() {
        let data: Vec<u8> = (0..1500u32).map(|i| (i * 11) as u8).collect();
        let large = BlockSize::Large;
        let mut last = vec![xmodem::ACK];
        last.extend_from_slice(b"Loaded 1500 bytes\r\n");
        let steps = vec![
            SerialStep::ExpectTx(xmodem::ymodem_header("u-boot.itb", 1500)),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Rx(vec![xmodem::CRC_REQUEST]),
            SerialStep::ExpectTx(xmodem::block(large, 1, &data[..1024])),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(xmodem::block(large, 2, &data[1024..])),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK, xmodem::ACK, xmodem::CRC_REQUEST]),
            SerialStep::ExpectTx(xmodem::ymodem_end()),
            SerialStep::Rx(last),
        ];
        let mut serial = ScriptedSerial::new(steps);
        let mut console = Console::new();
        let mut said = Vec::new();
        pollster::block_on(send_ymodem(
            &mut serial,
            &mut console,
            &data,
            &mut |_| {},
            &mut |bytes| said.extend_from_slice(bytes),
            &Cancel::new(),
        ))
        .expect("the batch follows the script");
        serial.assert_drained();
        assert_eq!(said, b"Loaded 1500 bytes\r\n");
    }
}
