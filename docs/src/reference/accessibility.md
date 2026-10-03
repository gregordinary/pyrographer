# Accessibility

This chapter states the accessibility standard the pyrographer window is held to, and what
backs each part of that claim. Every claim that has not been checked says so.

This chapter describes **`pyrographer-gui` as this tree builds it**, on egui/eframe 0.36.2 with
AccessKit 0.24.1. Each claim applies to that build. The numbers in this chapter are the ones the
tests in `theme.rs` and `ui.rs` recompute on every commit.

## The standard

The window follows the **WCAG 3.0 draft**, and takes its *outcomes* as the requirements
framework. Where WCAG 3.0 sets no number, the window takes the number from **WCAG 2.2 Level
AA**. WCAG 3.0 is a Working Draft with an unsettled conformance model. **This is not a claim of
conformance to WCAG 3.0**, because there is nothing yet to conform to.

The palette is also held to **APCA**, which is not frozen. Every color pairing must therefore
meet *both* an APCA `Lc` target and the WCAG 2.2 contrast ratio. A pairing that passes one and
fails the other is a defect, not a rounding error. A palette can pass the more permissive
measure and fail the other badly, on a hairline that looks correct.

## What is measured on every commit

Each claim in this section is checked by a test. The tests run with no window, no hardware and
no assistive technology installed, and a regression fails the build.

### Contrast

Each text pairing is held to its role's `Lc` target and to 4.5:1, against every background it
is drawn on, in both light and dark. The border that identifies a control at rest is held to
3:1 against all three grounds. The edge of a focused control is held to 3:1. So is the focus
ring drawn outside it, against every background it can stand on.

### Names

Every operable control has a name that an assistive technology can announce. This includes the
field on the write gate where you type the destination, which is the last control before the
flash is overwritten.

The names on one screen must also be **distinct**. The disk list draws a `Use` button on each
row, and that button selects the target of a destructive operation. Each button therefore reads
`Use` and announces the disk it selects, as in `Use /dev/sdb`.

### Reasons

A disabled control carries the reason it is disabled into the accessibility tree. The reasons
that guard a destructive operation are drawn on screen as sentences, not in a tooltip that
requires a mouse.

### Target size

Target size is measured on the rectangles the window draws, against WCAG 2.2 SC 2.5.8 (24×24),
including its spacing exception.

### Keyboard

Every enabled control is reachable with Tab. The check runs on each tab, because the window
draws one flow at a time.

The write gate places keyboard focus on the field where you type the destination. Its confirm
button joins the keyboard order only after you type the destination.

### Tabs

The bar that divides the window is exposed as a tab list, and each tab reports whether it is
the current one. egui has no tab widget of its own, and its nearest control announces as a
pressed button. The current tab is marked by a rule beneath it rather than by a fill, because no
fill in this palette is visible enough.

### Jobs

A running job is drawn on a strip outside the tabs, so its `Cancel` button is reachable from
either flow. The button's name includes the job it stops, rather than only `Cancel`.

### Focus

A keyboard-focused control is drawn with a ring outside it, which a held-down control does not
have.

## Use of color

Wherever color conveys information, text conveys it too. A refused disk reads `running system`
beside its color, and a sentence under the list states the consequence. The current tab is
underlined, not only tinted. The write gate's verdict is a sentence, not a red border. The
command-line tool uses no color and states everything in words, and the window inherits those
sentences.

Device names use the same monospace font and the same `/dev/...` spelling as the disk list. One
disk is therefore never rendered two ways on one screen. Device names have no shaded background.
Every background color in this theme is within 1.22:1 of the page, so a shaded chip would be
invisible. A visible chip would need a block of color heavy enough to compete with the warning
that contains it.

A disk refused for more than one reason is listed under **each** reason, so you can resolve
every reason in one pass. The exception is an absolute reason, which is shown alone. Remedies
listed beside it would imply that it can be overcome.

The sentences under the disk list are written **one per kind of refusal, not one per disk**.
Several disks are often refused for the same reason. For a system installed on an LVM volume in
an encrypted container, every device in that chain is refused. A warning repeated once for each
disk teaches the reader to skip it. Different kinds stay separate, because their remedies
differ. A disk that you can unmount never appears under the words *There is no override*.

## Text scaling

Ctrl-plus and Ctrl-minus rescale the whole interface, including its layout. No size in the
theme is hard-coded in a way that breaks rescaling.

## Light and dark themes

The window follows the operating system's theme preference, and a control in the header cycles
through automatic, light and dark. The dark palette is tuned on its own rather than inverted,
because reverse-polarity contrast does not behave as a mirror image.

The choice is not saved, so each launch follows the system preference again. pyrographer stores
no user state anywhere.

## Unverified

- **What a screen reader says.** The accessibility tree is correct, and a manual check confirmed
  that it reaches the Linux accessibility bus, where a screen reader reads it. No screen reader
  has been run against this window.
- **Windows and macOS.** Neither platform has been tested.

## Known defect

**An assistive technology cannot activate a control by itself.** The application accepts a
request to press a button through the accessibility interface, and the request has no effect.
The defect is in the toolkit's integration with the platform accessibility bridge, outside
pyrographer's own code.

Ordinary screen-reader use is not affected. Activating a control by pressing a key travels the
normal keyboard path, and that path is tested. The defect affects a device or review mode that
activates controls through the accessibility interface instead of sending keystrokes.

## The web flasher

The web flasher is **best-effort, and not conformant for anything a screen reader mediates**.
The limitation is architectural. egui draws to a `<canvas>`, which has no DOM, so there are no
elements to expose and no accessibility tree to populate.

The visual requirements carry over in full: contrast, color never used alone, focus visibility,
target size and zoom. Each is a property of the rendered pixels, which a canvas delivers.

**If you need a screen reader, use the native application or the command-line tool.** The CLI
exposes every verb the window does, as ordinary terminal output.
