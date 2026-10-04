//! Reading a serial console: the read loop, the transcript sink, and the passive
//! [`watch`].
//!
//! This module is the I/O half of [`codec::console`](crate::codec::console), as
//! [`recovery`](crate::recovery) is the I/O half of
//! [`xmodem`](crate::codec::xmodem). The codec accumulates and matches. This module
//! runs the loop that fills the codec from a [`Serial`] line. The U-Boot driver in
//! [`uboot`](crate::uboot) calls it.
//!
//! # Scope
//!
//! Every part of pyrographer drives a device that is **not running an operating
//! system**. That covers on-chip boot code, a loader, and a DFU stub or recovery
//! agent. A bootloader prompt is in that set. It is the last stage before an OS, and the
//! stage a maskrom bootstrap hands off to. A board's storage is still addressable
//! there as a raw medium.
//!
//! pyrographer does not drive a **login prompt**. Driving one means credentials, a
//! service manager and a distribution's contract, and this crate has machinery for
//! none of them. The passive [`watch`] covers unattended assertion over a serial
//! line without any of that. A node prints its verdict to the console, and the host
//! reads it.
//!
//! # Read budgets
//!
//! `Instant::now()` panics on `wasm32`, so nothing in core measures elapsed time. A
//! wait therefore spends a **budget of reads** rather than a [`Duration`]. Each
//! read carries the transport's own deadline, so a budget still bounds the wait in
//! wall-clock time, and core needs no clock. [`recovery`](crate::recovery) uses the
//! same idiom, with the same naming (`HANDSHAKE_READS`, `ACK_READS`).
//!
//! A read that reaches its deadline with no bytes returns [`Error::Timeout`], not
//! `Ok(0)`. Silence therefore costs one read of the budget, and the wait continues.
//! Any other error is a real failure and is returned at once. A budget spent with no
//! match returns [`Error::Protocol`] carrying the transcript tail. A console failure
//! over a serial line cannot be diagnosed without a transcript.
//!
//! [`Duration`]: std::time::Duration

use crate::codec::console::{Console, Match, unescape};
use crate::progress::Cancel;
use crate::transport::Serial;
use crate::{Error, Result};

/// Where a console session sends the bytes as they arrive.
///
/// It is a plain closure, the same idiom as
/// [`ProgressSink`](crate::progress::ProgressSink), so core needs no dependency to
/// carry one. The CLI writes straight to the terminal, a window appends to the
/// transcript its next frame draws, and a test collects into a `Vec`.
///
/// Byte counts do not describe a console session, so the sink receives the bytes
/// themselves, not a [`Progress`](crate::progress::Progress) event. The bytes stream
/// out as they arrive, because a console that prints nothing while it waits looks
/// exactly like a hang. Pass `&mut |_| {}` to ignore them.
pub type ConsoleSink<'a> = &'a mut dyn FnMut(&[u8]);

/// How many reads a wait spends before giving up, unless a caller says otherwise.
///
/// Each read carries the serial transport's own deadline, one second on the native
/// transport. Against a far end that sends nothing, this is ~30 seconds of silence.
/// Against a far end that answers, the wait ends as soon as the pattern arrives.
pub const DEFAULT_READS: u32 = 30;

/// What a serial console line ends with.
///
/// It is a carriage return, the byte a terminal sends for the Enter key. The far end
/// therefore receives the same byte it gets from a person at a terminal. U-Boot's
/// line reader ends a line on either `\r` or `\n`, so both work. **\[DOC\]**
pub const LINE_END: &[u8] = b"\r";

/// The most bytes one read takes from the line.
const READ_CHUNK: usize = 512;

/// Which list a watched pattern came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// One of the patterns being waited for.
    Expected,
    /// One of the patterns that mean the far end reported a failure.
    Failed,
}

/// What a [`watch`] saw.
///
/// **A watch reports what appeared, and asserts nothing.** A failure pattern is
/// returned as a finding, not raised as an error. The caller decides whether a board
/// printing `FAIL` ends a script.
///
/// A watch returns an error only after its budget runs out, and the error states
/// only that the pattern did not appear. It does not claim the far end failed. The
/// cause can be any of these:
///
/// - A failure
/// - A slow boot
/// - A wrong baud rate
/// - A console on another UART
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watched {
    /// Which list the pattern came from.
    pub seen: Seen,
    /// Its index within that list.
    pub index: usize,
    /// The pattern itself, as the bytes that were matched.
    pub pattern: Vec<u8>,
    /// Where it appeared, in bytes from the start of the session.
    pub at: u64,
}

/// Read from `serial` into `console` until one of `patterns` appears.
///
/// It searches first, because the bytes that answer this wait can arrive while the
/// previous wait is still being satisfied. It then spends one read of the budget,
/// and searches again.
/// A read that timed out is silence, not a failure: it costs one read, and the loop
/// continues. Every byte read goes to `sink` as it arrives.
///
/// Nothing is consumed. The caller decides what a match means, and advances the
/// cursor with [`Console::consume`]. A budget spent with no match returns
/// [`Error::Protocol`], naming what was waited for and carrying the transcript tail.
pub async fn wait_for<S: Serial>(
    serial: &mut S,
    console: &mut Console,
    patterns: &[&[u8]],
    budget: u32,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<Match> {
    match look_for(serial, console, patterns, budget, sink, cancel).await? {
        Some(found) => Ok(found),
        None => Err(Error::Protocol(format!(
            "nothing matching {} arrived on the console within {budget} reads. Recent console \
             output:\n{}",
            name_patterns(patterns),
            console.tail_text(),
        ))),
    }
}

/// The same loop, for a caller to whom a missing pattern is an answer.
///
/// [`wait_for`] is this loop plus one step that turns a spent budget into an error.
/// For some waits, a spent budget is not a failure. A gadget that has taken over the
/// console never returns the prompt.
/// [`uboot::UBoot::start_gadget`](crate::uboot::UBoot::start_gadget) therefore
/// treats a prompt that does not arrive as success. Such a caller gets `Ok(None)`,
/// and does not have to parse an error message to tell the two outcomes apart.
pub async fn look_for<S: Serial>(
    serial: &mut S,
    console: &mut Console,
    patterns: &[&[u8]],
    budget: u32,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<Option<Match>> {
    let mut buf = [0u8; READ_CHUNK];

    for _ in 0..budget {
        if let Some(found) = console.find(patterns) {
            return Ok(Some(found));
        }
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }
        match serial.read(&mut buf).await {
            Ok(n) => {
                sink(&buf[..n]);
                console.push(&buf[..n]);
            }
            // Silence. On a console that is exactly what waiting looks like: the
            // board is booting, or thinking, or has not been powered on yet.
            Err(Error::Timeout { .. }) => continue,
            Err(other) => return Err(other),
        }
    }

    // The last read's bytes have not been searched yet: the budget counts reads,
    // not searches.
    Ok(console.find(patterns))
}

/// How long a wait lasts against a far end that prints while it works.
///
/// A budget of reads suits a far end that answers and then falls quiet. It does not
/// suit one that prints all the way through a long job. A recovery agent writing
/// flash prints a dot per page, and a board coming up prints banner after banner.
/// Each burst of output costs a read, so a read budget runs out while the far end
/// is plainly still working.
///
/// This wait is measured in silence instead. A read that returns bytes costs
/// nothing, and the wait ends after [`silent_reads`](Self::silent_reads) reads in a
/// row return none. A far end that is printing is working, and one that has gone
/// quiet for that long has stopped. A far end that never falls quiet is bounded by
/// [`max_bytes`](Self::max_bytes), so the wait still ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patience {
    /// How many reads in a row can come back empty before the wait ends.
    pub silent_reads: u32,
    /// How many bytes the far end can send before the wait ends anyway.
    pub max_bytes: u64,
}

/// Read from `serial` into `console` until one of `patterns` appears, for as long
/// as the far end keeps talking.
///
/// It is [`look_for`] with the budget [`Patience`] describes in place of a count of
/// reads. It returns `Ok(None)` when the far end has been silent for
/// [`Patience::silent_reads`] reads in a row, or has sent [`Patience::max_bytes`]
/// with no match. Every byte read goes to `sink` as it arrives.
pub async fn look_for_patiently<S: Serial>(
    serial: &mut S,
    console: &mut Console,
    patterns: &[&[u8]],
    patience: Patience,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<Option<Match>> {
    let mut buf = [0u8; READ_CHUNK];
    let mut silent = 0;
    let mut received: u64 = 0;

    loop {
        if let Some(found) = console.find(patterns) {
            return Ok(Some(found));
        }
        if silent >= patience.silent_reads || received >= patience.max_bytes {
            return Ok(None);
        }
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }
        match serial.read(&mut buf).await {
            // A read is meant to report silence as a timeout. One that returns no
            // bytes is silence all the same, and counts as such.
            Ok(0) | Err(Error::Timeout { .. }) => silent += 1,
            Ok(n) => {
                silent = 0;
                received += n as u64;
                sink(&buf[..n]);
                console.push(&buf[..n]);
            }
            Err(other) => return Err(other),
        }
    }
}

/// Watch a console for the patterns that mean it worked, and the ones that mean
/// it did not.
///
/// It is the simplest consumer of the accumulator, and the most widely applicable.
/// It needs no prompt, no echo handling, and no login or credentials. A node that
/// runs its own self-test at boot needs exactly this from a host. The node prints
/// its verdict to the console, and the host reads it.
///
/// Both lists are matched at once, and the **earliest occurrence in the stream
/// wins**. A board that prints `FAIL` before `PASS` therefore reports the `FAIL`,
/// whichever list was passed first. The transcript streams out to `sink` as it
/// arrives.
///
/// It returns [`Error::InvalidRequest`] for an empty pattern, and for a call with
/// nothing to look for. Either wait could only end with the budget spent.
pub async fn watch<S: Serial>(
    serial: &mut S,
    expect: &[Vec<u8>],
    fail: &[Vec<u8>],
    budget: u32,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<Watched> {
    if expect.is_empty() && fail.is_empty() {
        return Err(Error::InvalidRequest(
            "a watch needs at least one pattern to look for, and none was given".to_string(),
        ));
    }
    if expect.iter().chain(fail).any(|pattern| pattern.is_empty()) {
        return Err(Error::InvalidRequest(
            "an empty pattern cannot match anything. Give the watch the bytes the device \
             prints"
                .to_string(),
        ));
    }

    // One search over both lists, so the earliest in the stream wins rather than
    // the first list passed. The index says which list it came from.
    let patterns: Vec<&[u8]> = expect
        .iter()
        .chain(fail)
        .map(|pattern| pattern.as_slice())
        .collect();

    let mut console = Console::new();
    let found = wait_for(serial, &mut console, &patterns, budget, sink, cancel).await?;

    let (seen, index) = if found.pattern < expect.len() {
        (Seen::Expected, found.pattern)
    } else {
        (Seen::Failed, found.pattern - expect.len())
    };
    Ok(Watched {
        seen,
        index,
        pattern: patterns[found.pattern].to_vec(),
        at: found.start,
    })
}

/// Send one line to the console: the text, then [`LINE_END`].
///
/// It refuses a line that carries a control character of its own, before any byte
/// goes out. A newline inside the text would submit two commands where the caller
/// wrote one. The U-Boot driver consumes one line of echo, so a smuggled newline
/// would leave it waiting for an echo that has already passed.
pub async fn send_line<S: Serial>(serial: &mut S, line: &str) -> Result<()> {
    if let Some(bad) = line.chars().find(|c| c.is_control()) {
        return Err(Error::InvalidRequest(format!(
            "a console command is a single line, and {bad:?} cannot be part of it. Send the \
             commands one at a time."
        )));
    }
    let mut bytes = line.as_bytes().to_vec();
    bytes.extend_from_slice(LINE_END);
    serial.write_all(&bytes).await
}

/// Read a caller's pattern, from a command line or a field in a window, into the
/// bytes it names.
///
/// Matching is byte-oriented, so a pattern uses the escapes
/// [`codec::console::unescape`](crate::codec::console::unescape) defines. Every
/// front-end reads its patterns through this function. It returns the crate's own
/// error type, so a caller has one kind of failure to handle.
pub fn pattern(text: &str) -> Result<Vec<u8>> {
    unescape(text).map_err(|why| Error::InvalidRequest(format!("'{text}' is not a pattern: {why}")))
}

/// Name the patterns a wait was watching for, in an error a person reads.
fn name_patterns(patterns: &[&[u8]]) -> String {
    if patterns.is_empty() {
        return "anything".to_string();
    }
    patterns
        .iter()
        .map(|pattern| format!("\"{}\"", String::from_utf8_lossy(pattern).escape_debug()))
        .collect::<Vec<_>>()
        .join(" or ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testing::{ScriptedSerial, SerialStep};

    /// Drive a watch against a scripted console.
    fn run_watch(
        steps: Vec<SerialStep>,
        expect: &[&str],
        fail: &[&str],
        budget: u32,
    ) -> (Result<Watched>, Vec<u8>) {
        let mut serial = ScriptedSerial::new(steps);
        let mut seen = Vec::new();
        let expect: Vec<Vec<u8>> = expect.iter().map(|p| p.as_bytes().to_vec()).collect();
        let fail: Vec<Vec<u8>> = fail.iter().map(|p| p.as_bytes().to_vec()).collect();
        let result = pollster::block_on(watch(
            &mut serial,
            &expect,
            &fail,
            budget,
            &mut |bytes| seen.extend_from_slice(bytes),
            &Cancel::new(),
        ));
        (result, seen)
    }

    /// A complete watch: a board boots and prints its verdict, and the host reads
    /// the verdict from the console. The transcript streams out as it arrives,
    /// because a console that prints nothing while it waits is indistinguishable
    /// from a hang.
    #[test]
    fn a_watch_reports_the_pattern_that_appeared() {
        let (watched, transcript) = run_watch(
            vec![
                SerialStep::Rx(b"[    0.00] booting\r\n".to_vec()),
                SerialStep::Timeout,
                SerialStep::Rx(b"selftest: PASS\r\n".to_vec()),
            ],
            &["PASS"],
            &["FAIL"],
            8,
        );

        let watched = watched.expect("PASS arrived");
        assert_eq!(watched.seen, Seen::Expected);
        assert_eq!(watched.index, 0);
        assert_eq!(watched.pattern, b"PASS");
        assert!(
            transcript.starts_with(b"[    0.00] booting"),
            "the transcript streams out as it arrives"
        );
    }

    /// The earliest occurrence in the stream wins, across both lists. The board
    /// printed `FAIL` first, so `FAIL` is reported. The expected pattern was passed
    /// first, and appears later in the same read.
    #[test]
    fn a_failure_that_came_first_wins_over_a_success_that_came_later() {
        let (watched, _) = run_watch(
            vec![SerialStep::Rx(b"stage1: FAIL\r\nstage2: PASS\r\n".to_vec())],
            &["PASS"],
            &["FAIL"],
            8,
        );

        let watched = watched.expect("something matched");
        assert_eq!(watched.seen, Seen::Failed);
        assert_eq!(watched.pattern, b"FAIL");
    }

    /// Silence costs a read, and the wait continues, because a board that has not
    /// been powered on yet has not failed. When the budget is spent, the error says
    /// the pattern did not appear, and carries what the far end did send.
    #[test]
    fn silence_is_waited_through_and_a_spent_budget_carries_the_transcript() {
        let (watched, _) = run_watch(
            vec![
                SerialStep::Timeout,
                SerialStep::Rx(b"U-Boot SPL 2026.04\r\n".to_vec()),
                SerialStep::Timeout,
                SerialStep::Timeout,
            ],
            &["PASS"],
            &[],
            3,
        );

        let Err(Error::Protocol(message)) = watched else {
            panic!("a spent budget is a protocol failure, not a match");
        };
        assert!(message.contains("\"PASS\""), "{message}");
        assert!(
            message.contains("U-Boot SPL 2026.04"),
            "the transcript is the error message: {message}"
        );
    }

    /// A port that disconnects mid-watch is a real failure, and the watch returns
    /// it at once. Only a quiet port is waited through.
    #[test]
    fn a_line_that_drops_is_not_mistaken_for_silence() {
        let (watched, _) = run_watch(
            vec![
                SerialStep::Rx(b"booting\r\n".to_vec()),
                SerialStep::Disconnect,
            ],
            &["PASS"],
            &[],
            8,
        );
        assert!(matches!(watched, Err(Error::Disconnected)), "{watched:?}");
    }

    /// A watch with nothing to look for, or with a pattern that names no bytes,
    /// could only end with the budget spent. Both are refused before the line is
    /// read.
    #[test]
    fn a_watch_with_nothing_to_look_for_is_refused() {
        let (watched, _) = run_watch(vec![], &[], &[], 4);
        assert!(
            matches!(watched, Err(Error::InvalidRequest(_))),
            "{watched:?}"
        );

        let (watched, _) = run_watch(vec![], &[""], &[], 4);
        assert!(
            matches!(watched, Err(Error::InvalidRequest(_))),
            "{watched:?}"
        );
    }

    /// Cancellation is checked between reads. A watch left running in a window
    /// therefore stops before its next read once the button is pressed. It does not
    /// run out its budget first.
    #[test]
    fn a_canceled_watch_stops_between_reads() {
        let mut serial = ScriptedSerial::new(vec![SerialStep::Rx(b"booting\r\n".to_vec())]);
        let cancel = Cancel::new();
        cancel.cancel();
        let result = pollster::block_on(watch(
            &mut serial,
            &[b"PASS".to_vec()],
            &[],
            8,
            &mut |_| {},
            &cancel,
        ));
        assert!(matches!(result, Err(Error::Canceled)), "{result:?}");
    }

    /// A line is one line. A newline smuggled into a command would submit two
    /// commands, and the U-Boot driver's echo consumption reads back only one. The
    /// line is therefore refused before a byte goes out.
    #[test]
    fn a_line_carrying_a_newline_is_refused_before_anything_is_sent() {
        let mut serial = ScriptedSerial::new(vec![]);
        let result = pollster::block_on(send_line(&mut serial, "setenv x 1\nsaveenv"));
        assert!(
            matches!(result, Err(Error::InvalidRequest(_))),
            "{result:?}"
        );
    }

    /// A line goes out with its terminator, and the scripted serial asserts the
    /// exact bytes. What a console driver types is therefore pinned, as an XMODEM
    /// block is.
    #[test]
    fn a_line_goes_out_with_its_terminator() {
        let mut serial = ScriptedSerial::new(vec![SerialStep::ExpectTx(b"printenv\r".to_vec())]);
        pollster::block_on(send_line(&mut serial, "printenv")).expect("it went out");
        serial.assert_drained();
    }

    /// Run a patient wait for `pattern` against a scripted line.
    fn run_patiently(steps: Vec<SerialStep>, patience: Patience) -> Result<Option<Match>> {
        let mut serial = ScriptedSerial::new(steps);
        let mut console = Console::new();
        let result = pollster::block_on(look_for_patiently(
            &mut serial,
            &mut console,
            &[b"updata success"],
            patience,
            &mut |_| {},
            &Cancel::new(),
        ));
        serial.assert_drained();
        result
    }

    /// A far end that keeps printing keeps the wait open. Here it prints more
    /// bursts than the silence allowance, with a quiet read between some of them,
    /// and the pattern still arrives. A budget of reads would have run out on the
    /// dots.
    #[test]
    fn a_far_end_that_keeps_printing_is_waited_for() {
        let mut steps = vec![SerialStep::Rx(b"updata first section\r\n".to_vec())];
        for _ in 0..10 {
            steps.push(SerialStep::Rx(b"....".to_vec()));
            steps.push(SerialStep::Timeout);
        }
        steps.push(SerialStep::Rx(b"\r\nupdata success\r\n".to_vec()));
        let patience = Patience {
            silent_reads: 2,
            max_bytes: 4096,
        };
        let found = run_patiently(steps, patience).expect("no failure");
        assert!(
            found.is_some(),
            "the verdict arrived while the far end was busy"
        );
    }

    /// Silence ends the wait. After the allowance of quiet reads in a row, the wait
    /// returns no match, and reads no further.
    #[test]
    fn silence_in_a_row_ends_a_patient_wait() {
        let steps = vec![
            SerialStep::Rx(b"updata first section".to_vec()),
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
        ];
        let patience = Patience {
            silent_reads: 3,
            max_bytes: 4096,
        };
        assert_eq!(run_patiently(steps, patience).expect("no failure"), None);
    }

    /// A far end that never falls quiet still ends the wait, once it has sent the
    /// most bytes the wait allows.
    #[test]
    fn a_far_end_that_never_falls_quiet_is_bounded_by_bytes() {
        let steps = vec![
            SerialStep::Rx(vec![b'.'; 64]),
            SerialStep::Rx(vec![b'.'; 64]),
        ];
        let patience = Patience {
            silent_reads: 2,
            max_bytes: 128,
        };
        assert_eq!(run_patiently(steps, patience).expect("no failure"), None);
    }
}
