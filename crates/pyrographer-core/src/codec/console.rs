//! The console accumulator: the bytes a bootloader prompt has sent back, and
//! where in them a pattern appeared.
//!
//! It is a sans-I/O codec, like [`xmodem`](super::xmodem) and
//! [`splhdr`](super::splhdr). This crate defines a codec by the absence of I/O, not
//! by the shape of what it describes. It touches no transport, so scripted bytes pin
//! it, and it compiles to `wasm32` unchanged. [`console`](crate::console) does the
//! reading and writing that fills it, and the U-Boot driver in
//! [`uboot`](crate::uboot) calls [`console`](crate::console). The division matches
//! the one between [`xmodem`](super::xmodem) and [`recovery`](crate::recovery).
//!
//! # The cursor
//!
//! A prompt string remains in the transcript after it is matched. A matcher with no
//! cursor finds the **first** `=> ` on every call. From the second command onward,
//! it returns at once, and every result is off by one and looks plausible. A
//! [`Match`] therefore reports where it ended, and the caller advances the cursor
//! past it with [`Console::consume`]. A search always starts at the cursor.
//!
//! Offsets count from the first byte ever pushed to the stream, not from the start
//! of the retained buffer. A [`Match`] therefore stays valid across compaction.
//!
//! # Matching
//!
//! Patterns are byte strings matched against raw bytes, with no line splitting and
//! no normalization, for two reasons:
//!
//! - A prompt is `=> `, including its trailing space. Normalizing trailing
//!   whitespace would remove part of the prompt.
//! - Some U-Boot builds rewrite the autoboot countdown digit in place with
//!   backspaces. A line-oriented matcher has to interpret them, and a
//!   byte-oriented one matches past them. **\[DOC\]**
//!
//! The **earliest occurrence in the stream wins**, not the first pattern in the
//! list. This rule governs a wait on two patterns at once. A board already at a
//! prompt never prints the autoboot countdown, so a wait on both must resolve by what
//! the far end produced. A loop of `contains` over the patterns cannot do that. Ties
//! at the same start offset go to the longer pattern, which is the more specific
//! match.
//!
//! An empty pattern names nothing, and is skipped rather than matching everywhere.
//!
//! # The transcript
//!
//! A console failure over a serial line cannot be diagnosed without a transcript.
//! The console therefore retains what arrived, bounded to a tail. Compaction drops
//! only bytes before the cursor, so it cannot split a match.
//! [`Console::tail_text`] marks its output as a tail whenever it is one.

/// How much of the transcript a [`Console`] keeps by default.
///
/// It bounds memory and does not affect matching. Bytes at or after the cursor are
/// never dropped. A console whose caller has not consumed a match therefore grows
/// past this size, and the match stays whole.
pub const DEFAULT_TAIL: usize = 64 * 1024;

/// How much of the transcript [`Console::tail_text`] puts in an error message.
///
/// It is smaller than [`DEFAULT_TAIL`], because a person reading a failure needs
/// the board's last output, not the whole session.
pub const ERROR_TAIL: usize = 2048;

/// Where a pattern appeared in the stream.
///
/// The offsets are counted from the first byte ever pushed to the [`Console`],
/// so they stay meaningful across compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Which of the patterns matched, as an index into the slice that was
    /// searched. A caller waiting on several patterns at once uses it to learn
    /// which one the far end produced.
    pub pattern: usize,
    /// Where the match begins, in bytes from the start of the stream.
    pub start: u64,
    /// Where it ends, and where the cursor goes once the match is consumed.
    pub end: u64,
}

/// The bytes a console has sent back, and a cursor into them.
///
/// The caller pushes what arrives, finds a pattern, consumes the match, and
/// searches again.
#[derive(Debug, Clone)]
pub struct Console {
    /// The retained transcript.
    buf: Vec<u8>,
    /// The stream offset of `buf[0]`: how many bytes compaction has dropped.
    base: u64,
    /// Where the next search starts, as a stream offset.
    cursor: u64,
    /// How much of the transcript to keep.
    tail: usize,
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Console {
    /// A console that has seen nothing, keeping [`DEFAULT_TAIL`] of transcript.
    pub fn new() -> Self {
        Self::with_tail(DEFAULT_TAIL)
    }

    /// A console that keeps `tail` bytes of transcript.
    ///
    /// Bytes at or after the cursor are kept regardless. The value is therefore a
    /// floor on what is retained, and the buffer can grow past it.
    pub fn with_tail(tail: usize) -> Self {
        Self {
            buf: Vec::new(),
            base: 0,
            cursor: 0,
            tail,
        }
    }

    /// Take bytes that arrived from the far end.
    ///
    /// It compacts afterward, and drops only bytes the cursor has already passed.
    /// Compaction therefore cannot split a match.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        self.compact();
    }

    /// Drop transcript from the front, never past the cursor.
    fn compact(&mut self) {
        if self.buf.len() <= self.tail {
            return;
        }
        let over = self.buf.len() - self.tail;
        let passed = self.buf_index(self.cursor);
        let drop = over.min(passed);
        if drop == 0 {
            return;
        }
        self.buf.drain(..drop);
        self.base += drop as u64;
    }

    /// Where a stream offset sits in the retained buffer.
    ///
    /// It saturates, so an offset from before the retained tail reads as the tail's
    /// start rather than panicking. Every offset this crate returns is at or after
    /// the cursor, and the cursor is never dropped. The saturation only guards a
    /// caller that holds an offset from somewhere else.
    fn buf_index(&self, offset: u64) -> usize {
        offset.saturating_sub(self.base).min(self.buf.len() as u64) as usize
    }

    /// The earliest occurrence of any of `patterns` at or after the cursor.
    ///
    /// Earliest in the stream wins, and a tie at the same offset goes to the longer
    /// pattern. Empty patterns are skipped. Nothing is consumed:
    /// [`consume`](Self::consume) does that.
    pub fn find(&self, patterns: &[&[u8]]) -> Option<Match> {
        let from = self.buf_index(self.cursor);
        let hay = &self.buf[from..];

        let mut best: Option<Match> = None;
        for (pattern, needle) in patterns.iter().enumerate() {
            if needle.is_empty() {
                continue;
            }
            let Some(at) = find_sub(hay, needle) else {
                continue;
            };
            let start = self.cursor + at as u64;
            let found = Match {
                pattern,
                start,
                end: start + needle.len() as u64,
            };
            // Earliest start wins; the longer match wins a tie, being the more
            // specific of the two.
            let better = match best {
                None => true,
                Some(best) => {
                    found.start < best.start || (found.start == best.start && found.end > best.end)
                }
            };
            if better {
                best = Some(found);
            }
        }
        best
    }

    /// Advance the cursor past a match, so the next search starts after it.
    pub fn consume(&mut self, found: &Match) {
        self.cursor = self.cursor.max(found.end);
        self.compact();
    }

    /// Advance the cursor past everything pushed so far.
    ///
    /// The bytes stay in the transcript, and no later search finds them. A caller
    /// uses it after handling a stretch of the stream by other means. The XMODEM
    /// sender does, with the control bytes of a transfer.
    pub fn skip_to_end(&mut self) {
        self.cursor = self.stream_len();
        self.compact();
    }

    /// What lay between the cursor and a match: a command's output, between the
    /// echo of the command and the prompt that follows it.
    pub fn before(&self, found: &Match) -> &[u8] {
        let from = self.buf_index(self.cursor);
        let to = self.buf_index(found.start).max(from);
        &self.buf[from..to]
    }

    /// The bytes the match itself covers.
    pub fn matched(&self, found: &Match) -> &[u8] {
        let from = self.buf_index(found.start);
        let to = self.buf_index(found.end).max(from);
        &self.buf[from..to]
    }

    /// Where the next search starts, in bytes from the start of the stream.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// How many bytes have ever been pushed.
    pub fn stream_len(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// The transcript that is still retained.
    pub fn transcript(&self) -> &[u8] {
        &self.buf
    }

    /// Whether anything has been dropped from the front of the transcript.
    pub fn truncated(&self) -> bool {
        self.base > 0
    }

    /// The end of the transcript, as prose for an error a person reads.
    ///
    /// Carriage returns are dropped and unprintable bytes become `.`, so a
    /// transcript full of countdown backspaces does not scramble the terminal it
    /// is printed to. The output opens with a marker in two cases: compaction
    /// dropped the beginning, or only the last [`ERROR_TAIL`] bytes are shown.
    pub fn tail_text(&self) -> String {
        let from = self.buf.len().saturating_sub(ERROR_TAIL);
        let mut out = String::new();
        if self.truncated() || from > 0 {
            out.push_str("...(earlier console output not shown)...\n");
        }
        out.push_str(&text(&self.buf[from..]));
        out
    }
}

/// Where `needle` first occurs in `hay`.
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Console bytes as text a person can read.
///
/// Carriage returns are dropped and unprintable bytes become `.`. A transcript
/// carrying an autoboot countdown's backspaces therefore reads as the text the board
/// sent, and does not rewrite the terminal it is shown in. Line feeds and tabs are
/// kept, because they carry the layout of what the console printed.
///
/// It is the only rendering core does. Every front-end uses it, and so does every
/// error message built in core, so a transcript reads the same everywhere.
pub fn text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .filter(|&&b| b != b'\r')
        .map(|&b| match b {
            b'\n' | b'\t' => b as char,
            0x20..=0x7e => b as char,
            _ => '.',
        })
        .collect()
}

/// Read a pattern written with backslash escapes into the bytes it names.
///
/// Matching is byte-oriented, so a pattern must be able to name any byte. The
/// escapes are:
///
/// - `\n`, `\r` and `\t`
/// - `\0`
/// - `\\`
/// - `\xNN`, for any byte at all
///
/// Every other character stands for itself. An escape that names no byte is
/// refused, not passed through. A typo is therefore caught where it was typed, and
/// does not become a pattern that can never match.
///
/// The CLI's `--expect` and the window's text field both rely on this function, so a
/// pattern means the same thing in both.
pub fn unescape(pattern: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut chars = pattern.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hi = chars.next();
                let lo = chars.next();
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err("\\x needs two hex digits, as in \\x1b".to_string());
                };
                let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16)
                    .map_err(|_| format!("\\x{hi}{lo} is not two hex digits"))?;
                out.push(byte);
            }
            Some(other) => {
                return Err(format!(
                    "\\{other} names no byte. The escapes are \\n \\r \\t \\0 \\\\ and \\xNN, and \
                     a lone backslash is written \\\\"
                ));
            }
            None => {
                return Err("the pattern ends in a lone backslash. Write it as \\\\".to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prompt appears once and is matched once. Without a cursor, the second
    /// search would find the first prompt again and return at once. Every later
    /// command would then read the previous command's output.
    #[test]
    fn a_consumed_match_is_not_found_again() {
        let mut console = Console::new();
        console.push(b"U-Boot 2026.04\n=> ");

        let first = console.find(&[b"=> "]).expect("the prompt arrived");
        console.consume(&first);
        assert!(
            console.find(&[b"=> "]).is_none(),
            "the prompt in the transcript is behind the cursor and must not match again"
        );

        console.push(b"version\r\nU-Boot 2026.04\n=> ");
        let second = console.find(&[b"=> "]).expect("the next prompt arrived");
        assert!(
            second.start > first.end,
            "the second prompt is a later one, not the first found again"
        );
    }

    /// Skipping to the end hides everything pushed so far from the next search, and
    /// keeps it in the transcript. Bytes pushed afterward are found as usual.
    #[test]
    fn skipping_to_the_end_hides_what_was_pushed_and_keeps_the_transcript() {
        let mut console = Console::new();
        console.push(b"CCCC\x06\x06");
        console.skip_to_end();
        assert!(console.find(&[b"CCCC"]).is_none(), "behind the cursor now");
        assert_eq!(
            console.transcript(),
            b"CCCC\x06\x06",
            "still in the transcript"
        );

        console.push(b"CCCC");
        let found = console.find(&[b"CCCC"]).expect("a later run");
        assert_eq!(found.start, 6);
    }

    /// The earliest occurrence wins, whatever order the patterns are listed in. A
    /// board that is counting down prints the banner and later the prompt. A board
    /// already at a prompt never prints the banner. A wait on both must resolve by
    /// which one the far end produced, which a loop of `contains` cannot do.
    #[test]
    fn the_earliest_occurrence_wins_whatever_order_it_was_listed_in() {
        let banner: &[u8] = b"Hit any key to stop autoboot";
        let prompt: &[u8] = b"=> ";

        let mut counting = Console::new();
        counting.push(b"Hit any key to stop autoboot:  2 \n=> ");
        // The prompt is listed first, and the banner still wins: it came first.
        let found = counting.find(&[prompt, banner]).expect("both are there");
        assert_eq!(
            found.pattern, 1,
            "the banner is what the board produced first"
        );

        let mut at_prompt = Console::new();
        at_prompt.push(b"=> ");
        let found = at_prompt
            .find(&[prompt, banner])
            .expect("the prompt is there");
        assert_eq!(found.pattern, 0, "this board never printed a countdown");
    }

    /// Two patterns starting at the same byte: the longer one is the more
    /// specific match, and it is the one reported.
    #[test]
    fn a_tie_goes_to_the_longer_pattern() {
        let mut console = Console::new();
        console.push(b"boot_targets=mmc0\n");

        let found = console
            .find(&[b"boot_targets", b"boot_targets=mmc0"])
            .expect("both start at the same byte");
        assert_eq!(found.pattern, 1);
        assert_eq!(console.matched(&found), b"boot_targets=mmc0");
    }

    /// A pattern split across two arrivals still matches. A serial read has no
    /// framing to align to, and the accumulator makes the byte stream searchable
    /// across reads.
    #[test]
    fn a_pattern_split_across_reads_still_matches() {
        let mut console = Console::new();
        console.push(b"Hit any key to sto");
        assert!(console.find(&[b"stop autoboot"]).is_none());
        console.push(b"p autoboot:  3 ");
        assert!(console.find(&[b"stop autoboot"]).is_some());
    }

    /// Compaction cannot split a match. The tail here is tiny, and far more than
    /// that arrives. Nothing at or after the cursor is ever dropped, so a pattern
    /// that straddles the point where a cut would fall still matches.
    #[test]
    fn compaction_drops_only_what_the_cursor_has_passed() {
        let mut console = Console::with_tail(16);

        console.push(b"........................................");
        assert!(
            !console.truncated(),
            "nothing has been consumed, so nothing may be dropped"
        );
        assert_eq!(console.transcript().len(), 40);

        // Consume the first ten bytes, and only those may go.
        console.consume(&Match {
            pattern: 0,
            start: 0,
            end: 10,
        });
        console.push(b"=> ");
        assert!(console.truncated(), "the passed-over bytes were dropped");
        assert!(
            console.stream_len() > console.transcript().len() as u64,
            "the stream is longer than what is kept"
        );

        // And the offsets still line up after the drop.
        let found = console.find(&[b"=> "]).expect("the prompt is retained");
        assert_eq!(found.end, console.stream_len());
        assert_eq!(console.matched(&found), b"=> ");
    }

    /// What lay between the cursor and the prompt is a command's output. A match
    /// reports where it starts, as well as where it ends, so that a caller can read
    /// that output.
    #[test]
    fn the_bytes_before_a_match_are_the_output() {
        let mut console = Console::new();
        console.push(b"printenv boot_targets\r\nboot_targets=mmc0 usb0\r\n=> ");

        let echo = console.find(&[b"printenv boot_targets"]).expect("the echo");
        console.consume(&echo);

        let prompt = console.find(&[b"=> "]).expect("the prompt");
        assert_eq!(console.before(&prompt), b"\r\nboot_targets=mmc0 usb0\r\n");
    }

    /// An empty pattern names nothing. If it matched everywhere, a wait on it would
    /// return at once, every time. A caller that passed one by accident never
    /// intends that.
    #[test]
    fn an_empty_pattern_matches_nothing() {
        let mut console = Console::new();
        console.push(b"=> ");
        assert!(console.find(&[b""]).is_none());
        // And it does not spoil a real pattern beside it.
        let found = console.find(&[b"", b"=> "]).expect("the real one matches");
        assert_eq!(found.pattern, 1);
    }

    /// An error message is prose. Carriage returns and the countdown's backspaces
    /// would scramble the terminal it is printed to, so they are removed and the
    /// text is kept.
    #[test]
    fn the_tail_is_rendered_for_a_person_to_read() {
        let mut console = Console::new();
        console.push(b"autoboot:  3 \x08\x08\x082 \r\nU-Boot\r\n");
        let text = console.tail_text();
        assert!(text.contains("autoboot:  3 ...2 \nU-Boot\n"), "{text}");
        assert!(!text.contains('\r'), "{text}");
        assert!(
            !text.starts_with("...(earlier"),
            "nothing was dropped: {text}"
        );
    }

    /// A transcript that lost its beginning says so, rather than reading as the
    /// whole of what the board said.
    #[test]
    fn a_compacted_tail_says_it_is_a_tail() {
        let mut console = Console::with_tail(8);
        console.push(b"the beginning of a long session, now gone");
        console.consume(&Match {
            pattern: 0,
            start: 0,
            end: 40,
        });
        console.push(b"=> ");
        assert!(console.tail_text().starts_with("...(earlier"));
    }

    /// A pattern is bytes, and the escapes can write any byte.
    #[test]
    fn escapes_name_the_bytes_they_say_they_do() {
        assert_eq!(unescape("=> ").unwrap(), b"=> ");
        assert_eq!(unescape(r"a\nb").unwrap(), b"a\nb");
        assert_eq!(unescape(r"\r\n").unwrap(), b"\r\n");
        assert_eq!(unescape(r"\x1b[0m").unwrap(), b"\x1b[0m");
        assert_eq!(unescape(r"back\\slash").unwrap(), br"back\slash");
        assert_eq!(unescape(r"\0").unwrap(), b"\0");
    }

    /// An escape that names nothing is a typo, and a typo becomes a pattern that
    /// can never match. Refusing it where it was typed is cheaper than spending a
    /// whole budget against a board that was answering all along.
    #[test]
    fn an_escape_that_names_nothing_is_refused() {
        assert!(unescape(r"\q").is_err());
        assert!(unescape(r"\x").is_err());
        assert!(unescape(r"\xzz").is_err());
        assert!(unescape("ends in a backslash\\").is_err());
    }
}
