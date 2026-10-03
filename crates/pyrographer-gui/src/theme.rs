//! The window's style, installed once and inherited by every widget.
//!
//! The GUI has no styling outside this module. [`install`] is called from
//! [`App::new`](crate::App::new), so the desktop window and the web flasher take the
//! same style. They look identical, because egui draws its own widgets to a texture
//! and nothing per-platform needs reconciling.
//!
//! # Palette
//!
//! The palette carries the safety model. pyrographer means *one who writes by
//! burning*, and the colors follow the name. A calm cyan-teal is the one interactive
//! accent. It marks links, selection, focus, and the controls a person can press
//! freely. A reserved heat scale (amber for caution, red for destruction) is used
//! only where flash is at stake. The heat scale is [`Visuals::warn_fg_color`] and
//! [`Visuals::error_fg_color`], which the drawing code routes the refused `erase`
//! and the write gate through.
//!
//! # Contrast
//!
//! Every text-on-ground pairing is tuned to the perceptual contrast of the WCAG 3.0
//! draft (APCA). The targets are:
//!
//! - Body text at `Lc 90`
//! - Secondary labels at `Lc 70`
//! - Semantic and interactive text at `Lc 60`
//! - Borders at `Lc 15`
//!
//! APCA is not yet frozen in the draft (`[DOC]`). Every pairing is therefore held to
//! both APCA and the WCAG 2.2 ratio. A pairing that passes one bar and fails the
//! other is a finding, not a rounding error.
//!
//! The `tests` module recomputes APCA and the 2.2 ratio from the palettes in this
//! module. A color edited to look better and measure worse therefore fails the
//! build.

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Margin, Stroke, Style, TextStyle, Theme, ThemePreference,
    Visuals,
};

/// One theme's colors, named by role rather than by shade.
///
/// The two instances, [`dark`] and [`light`], carry the same roles, so [`visuals`]
/// maps a palette to egui once and serves both.
struct Palette {
    /// The window and panel ground.
    canvas: Color32,
    /// A text field's ground.
    input_bg: Color32,
    /// A button or control at rest.
    btn_rest: Color32,
    /// The same, hovered.
    btn_hover: Color32,
    /// The same, pressed.
    btn_active: Color32,
    /// A faint separator or decorative hairline.
    line: Color32,
    /// The boundary that identifies a control: a field or button edge.
    ///
    /// **This stroke is the only thing that separates a control at rest from the
    /// page.** A control's fill differs from the canvas by 1.22:1 dark and 1.05:1
    /// light, so the fill identifies nothing. WCAG 2.2 SC 1.4.11 therefore rests on
    /// this one stroke, and that sets its value.
    ///
    /// The stroke is held to the 3:1 non-text bar against every ground it is drawn
    /// on: the canvas, a button's fill and a field's fill. The `Lc 15` border target
    /// alone is not enough, because it is the more permissive of the two bars. A
    /// near-invisible value can pass `Lc 15` and fail WCAG 2.2 outright. The `tests`
    /// module checks all six pairings.
    line_strong: Color32,
    /// Body and heading text.
    text_hi: Color32,
    /// Secondary labels: what egui draws with `weak()`.
    text_mid: Color32,
    /// The one interactive accent. It is cool, which keeps it apart from the heat
    /// scale.
    accent: Color32,
    /// The accent, brightened, for a pressed control's edge.
    accent_hover: Color32,
    /// Caution: the warm end of the heat scale, short of destruction.
    warn: Color32,
    /// Destruction: a write, or a mismatch.
    danger: Color32,
    /// The deepest ground: a progress trough, a slider rail.
    extreme: Color32,
}

/// The dark theme.
///
/// Reverse-polarity contrast is the harder case, so these colors are tuned brighter
/// than a naive inversion of the light theme would leave them.
fn dark() -> Palette {
    Palette {
        canvas: Color32::from_rgb(0x12, 0x13, 0x17),
        input_bg: Color32::from_rgb(0x22, 0x26, 0x2c),
        btn_rest: Color32::from_rgb(0x22, 0x26, 0x2c),
        btn_hover: Color32::from_rgb(0x2c, 0x31, 0x3a),
        btn_active: Color32::from_rgb(0x34, 0x3b, 0x45),
        line: Color32::from_rgb(0x2a, 0x2f, 0x37),
        line_strong: Color32::from_rgb(0x67, 0x71, 0x7f),
        text_hi: Color32::from_rgb(0xe7, 0xe9, 0xec),
        text_mid: Color32::from_rgb(0xc4, 0xca, 0xd2),
        accent: Color32::from_rgb(0x4c, 0xc2, 0xd4),
        accent_hover: Color32::from_rgb(0x6f, 0xd0, 0xe0),
        warn: Color32::from_rgb(0xec, 0xb3, 0x5d),
        danger: Color32::from_rgb(0xff, 0x9d, 0x94),
        extreme: Color32::from_rgb(0x0d, 0x0f, 0x12),
    }
}

/// The light theme.
fn light() -> Palette {
    Palette {
        canvas: Color32::from_rgb(0xf6, 0xf7, 0xf8),
        input_bg: Color32::from_rgb(0xff, 0xff, 0xff),
        btn_rest: Color32::from_rgb(0xf0, 0xf2, 0xf4),
        btn_hover: Color32::from_rgb(0xe6, 0xe9, 0xec),
        btn_active: Color32::from_rgb(0xdd, 0xe1, 0xe5),
        line: Color32::from_rgb(0xe2, 0xe6, 0xea),
        line_strong: Color32::from_rgb(0x7f, 0x8d, 0x9d),
        text_hi: Color32::from_rgb(0x19, 0x1c, 0x21),
        text_mid: Color32::from_rgb(0x56, 0x5e, 0x6a),
        accent: Color32::from_rgb(0x0e, 0x7c, 0x8b),
        accent_hover: Color32::from_rgb(0x0b, 0x6b, 0x78),
        warn: Color32::from_rgb(0x8a, 0x5a, 0x12),
        danger: Color32::from_rgb(0xbf, 0x2f, 0x2a),
        extreme: Color32::from_rgb(0xff, 0xff, 0xff),
    }
}

/// Corner radius for a widget: modern, not glossy.
const R_WIDGET: CornerRadius = CornerRadius::same(4);
/// Corner radius for a window or menu frame.
const R_FRAME: CornerRadius = CornerRadius::same(6);

/// Turn a palette into egui [`Visuals`], starting from egui's own theme so every
/// field this does not name keeps a sane default.
fn visuals(p: &Palette, base: Visuals) -> Visuals {
    let mut v = base;

    v.panel_fill = p.canvas;
    v.window_fill = p.canvas;
    v.extreme_bg_color = p.extreme;
    v.text_edit_bg_color = Some(p.input_bg);
    v.code_bg_color = p.input_bg;
    v.faint_bg_color = p.btn_rest;

    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    // What `ui.weak()` draws: the "required"/"optional" labels lifted off the floor.
    v.weak_text_color = Some(p.text_mid);

    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = R_FRAME;
    v.menu_corner_radius = R_FRAME;

    // Text selection, and a focused field's ring, both in the accent.
    v.selection.bg_fill =
        Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 90);
    v.selection.stroke = Stroke::new(1.0, p.accent);

    // Plain text and separators.
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text_hi);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.noninteractive.corner_radius = R_FRAME;

    // A control at rest.
    v.widgets.inactive.weak_bg_fill = p.btn_rest;
    v.widgets.inactive.bg_fill = p.btn_rest;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line_strong);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text_hi);
    v.widgets.inactive.corner_radius = R_WIDGET;

    // Hovered: the accent edges in, and the widget lifts by a pixel.
    v.widgets.hovered.weak_bg_fill = p.btn_hover;
    v.widgets.hovered.bg_fill = p.btn_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text_hi);
    v.widgets.hovered.corner_radius = R_WIDGET;
    v.widgets.hovered.expansion = 1.0;

    // Pressed.
    v.widgets.active.weak_bg_fill = p.btn_active;
    v.widgets.active.bg_fill = p.btn_active;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent_hover);
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text_hi);
    v.widgets.active.corner_radius = R_WIDGET;
    v.widgets.active.expansion = 1.0;

    // An open combo or menu wears the pressed look.
    v.widgets.open = v.widgets.active;

    v
}

/// The type scale and spacing, shared by both themes.
///
/// This is kept out of [`visuals`] because it is the same in light and dark, and
/// only color differs between the themes. Sizes map to egui's [`TextStyle`]s. The
/// stock font ships one weight, so hierarchy here is size and color, not bold.
/// Bundling a multi-weight face is a later step.
fn tune(style: &mut Style) {
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(16.0, 10.0);
    s.button_padding = egui::vec2(12.0, 7.0);
    s.interact_size.y = 30.0;
    s.window_margin = Margin::same(16);
    s.menu_margin = Margin::same(8);
    s.indent = 18.0;

    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(22.0)),
        (TextStyle::Body, FontId::proportional(14.5)),
        (TextStyle::Button, FontId::proportional(14.5)),
        (TextStyle::Monospace, FontId::monospace(13.5)),
        (TextStyle::Small, FontId::proportional(12.5)),
    ]
    .into();
}

/// The page's inset: how far the content stands off the window's edge.
///
/// egui's `Frame::central_panel` hard-codes 8 px. That puts a heading almost against
/// the frame, and the left edge of every row looks accidental. This constant is the
/// one place the page's margin is decided. The window, the scrolling content and the
/// job strip along the bottom therefore all start on the same line.
///
/// The margin is wider than
/// [`Spacing::item_spacing`](egui::style::Spacing::item_spacing). The space around
/// the page therefore reads as a deliberate margin rather than as one more gap
/// between two items.
pub const PAGE_MARGIN: Margin = Margin::same(20);

/// The widest a paragraph can get, in points.
///
/// A line of prose that runs the width of a wide monitor is hard to read. The eye
/// travels back across the whole window to find the start of the next line, and
/// loses its place. The long-standing remedy is a *measure* of roughly 45 to 75
/// characters.
///
/// The value is measured. In this crate's body font at 14.5 pt, the window's own
/// prose averages 6.29 pt per character. The average is taken across three of the
/// window's real sentences, not a synthetic alphabet, which runs wider. At that rate
/// 66 characters is 415 pt and 75 is 472. This constant is 76 characters, the top of
/// the band, which suits text this dense and technical.
///
/// Without the cap, a paragraph is 121 characters wide in an 800 pt window. It is
/// 299 in a 1920 pt window, and 541 maximized on an ultrawide.
///
/// The cap applies to **prose only**, meaning text that is language. A row of disks
/// or partitions is scanned, not read, and needs every column on one line.
pub const MEASURE: f32 = 480.0;

/// The widest the page can get, in points.
///
/// The window can be any size. The content stops growing at the width the layout
/// was drawn for. The constant mainly governs the elements that span the page: the
/// rule under the header, the rail under the tabs, and the separators between
/// sections. Left unbounded, those stretch to the width of the monitor. A hairline
/// running three thousand points beside a column of controls six hundred wide reads
/// as unfinished.
///
/// The page stays **anchored left** rather than centered. A resize then moves
/// nothing sideways, and the reading order starts at the title.
pub const PAGE_WIDTH: f32 = 1150.0;

/// The frame the whole page is drawn in.
///
/// Takes the panel fill from the installed palette and the inset from
/// [`PAGE_MARGIN`], so no call site chooses either.
pub fn page_frame(style: &Style) -> egui::Frame {
    egui::Frame::new()
        .inner_margin(PAGE_MARGIN)
        .fill(style.visuals.panel_fill)
}

/// Install the theme on a context.
///
/// Sets both themes' colors and the shared type scale, then follows the OS light or
/// dark preference. A person overrides that at runtime with [`theme_toggle`]. The
/// choice is not persisted, because the GUI keeps no stored user state. A fresh
/// launch therefore follows the system again.
pub fn install(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Dark, visuals(&dark(), Visuals::dark()));
    ctx.set_visuals_of(Theme::Light, visuals(&light(), Visuals::light()));
    ctx.all_styles_mut(tune);
    ctx.set_theme(ThemePreference::System);
}

/// The keyboard focus ring, drawn separately from the pressed look.
///
/// egui gives a keyboard-focused widget the *pressed* visuals. `Widgets::style`
/// answers `has_focus()` with `active`, and that mapping is not a setting. A person
/// tabbing through therefore sees the held-down look travel, and cannot tell a
/// focused control from one under their finger. egui exposes nothing that changes
/// this, because focus lives in `Memory` and the mapping is hard-coded. The
/// distinction has to be *added*, because it cannot be reassigned.
///
/// The pressed look is left unchanged, and focus gains something the pressed look
/// lacks: a ring outside the widget, separated from it by a gap. The difference is a
/// shape rather than a shade. A shape reads at a glance, and it still works for a
/// person who cannot separate the two accents.
///
/// The ring is drawn once per frame, in the foreground layer, from the focused
/// widget's own rectangle. No call site opts in, so none can forget to. The ring is
/// drawn in the accent, read through one private function that the `tests` module
/// also reads. The tests measure that color against both bars on every ground the
/// ring can stand on.
pub fn focus_ring(ctx: &egui::Context) {
    let Some(id) = ctx.memory(|memory| memory.focused()) else {
        return;
    };
    let Some(response) = ctx.read_response(id) else {
        return;
    };

    // **A focused widget that is no longer drawn keeps its focus.** egui does not
    // clear it when a screen is replaced -- the write gate takes the whole window
    // and the id focused on the main screen survives -- and `read_response` falls
    // back to the previous pass, so ringing it would put a ring at coordinates
    // nothing occupies any more. So the rectangle is required to still be one of
    // the live ones. The gates place focus themselves, which is the fix for the
    // case that matters; this is what keeps every other screen change honest.
    if !ctx
        .interactive_rects_last_pass()
        .contains(&response.interact_rect)
    {
        return;
    }

    // Outside the widget and clear of it: a ring drawn *on* the border would be
    // the pressed border again, in a slightly different color.
    let ring = response.rect.expand(GAP + WIDTH / 2.0);
    // The installed palette for whichever theme is active, which is where the
    // accent the ring is drawn in lives.
    let visuals = ctx.style_of(ctx.theme()).visuals.clone();
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("pyrographer_focus_ring"),
    ))
    .rect_stroke(
        ring,
        visuals.widgets.active.corner_radius + GAP as u8,
        egui::Stroke::new(WIDTH, ring_color(&visuals)),
        egui::StrokeKind::Middle,
    );
}

/// The color the focus ring is drawn in: the accent.
///
/// It is one function so that [`focus_ring`] and the test that measures the ring
/// read the same value. A test that measured a color the ring is not drawn in
/// would pass while the ring itself faded.
fn ring_color(visuals: &Visuals) -> Color32 {
    visuals.hyperlink_color
}

/// How far the focus ring stands off the widget it rings.
const GAP: f32 = 2.0;

/// How thick the focus ring is.
///
/// WCAG 2.2's focus-appearance criterion asks two device-independent pixels of a
/// perimeter indicator. That width also keeps the ring visible against the pressed
/// border it has to be told apart from.
const WIDTH: f32 = 2.0;

/// A button that cycles the theme: automatic, then light, then dark.
///
/// Its label names the current choice in words. An icon would leave a person
/// guessing, and the project has no emoji to draw one from. [`ui`](crate::ui) draws
/// the button in the header.
pub fn theme_toggle(ui: &mut egui::Ui) {
    let current = ui.ctx().options(|o| o.theme_preference);
    let (label, next) = match current {
        ThemePreference::System => ("Theme: Auto", ThemePreference::Light),
        ThemePreference::Light => ("Theme: Light", ThemePreference::Dark),
        ThemePreference::Dark => ("Theme: Dark", ThemePreference::System),
    };
    if ui
        .button(label)
        .on_hover_text("Cycle automatic, light, and dark")
        .clicked()
    {
        ui.ctx().set_theme(next);
    }
}

/// The contrast the palette claims, recomputed from the palette.
///
/// The accessibility claim is held as tests rather than as a comment. A color is the
/// one kind of constant that gets edited by eye. An eye cannot tell `1.51:1` from
/// `3.02:1` on a light gray hairline. Both bars the project holds itself to are
/// therefore computed here from [`dark`] and [`light`] themselves. A swatch changed
/// to look better and measure worse fails the build, before it can fail a person who
/// needs contrast.
///
/// The tests are pure arithmetic over constants, with no window, GPU or hardware.
/// Every codec test in core has the same shape, for the same reason: it needs no
/// hardware to run.
#[cfg(test)]
mod tests {
    use super::{Palette, dark, light, ring_color, visuals};
    use eframe::egui::{Color32, Visuals};

    /// Linearize one 8-bit channel for APCA.
    ///
    /// APCA uses a plain 2.4 power curve, not the sRGB piecewise transfer that the
    /// relative luminance of WCAG 2.x uses.
    fn apca_lin(c: u8) -> f64 {
        (c as f64 / 255.0).powf(2.4)
    }

    /// APCA screen luminance, with the black soft-clamp the algorithm applies
    /// before any comparison.
    fn apca_y(c: Color32) -> f64 {
        let y =
            0.2126729 * apca_lin(c.r()) + 0.7151522 * apca_lin(c.g()) + 0.0721750 * apca_lin(c.b());
        if y < 0.022 {
            y + (0.022 - y).powf(1.414)
        } else {
            y
        }
    }

    /// Perceptual lightness contrast (APCA `Lc`), as an absolute value.
    ///
    /// The two polarities take different exponents on purpose, because
    /// light-on-dark is not the mirror of dark-on-light. That asymmetry is why the
    /// draft prefers APCA to a ratio. It is also why [`dark`] is its own palette
    /// rather than an inversion of [`light`].
    ///
    /// APCA is a Working Draft and not frozen (`[DOC]`). This implementation is
    /// version 0.1.9, so every pairing is also held to [`ratio`].
    fn lc(text: Color32, bg: Color32) -> f64 {
        let (yt, yb) = (apca_y(text), apca_y(bg));
        if (yb - yt).abs() < 0.0005 {
            return 0.0;
        }
        let sapc = if yb > yt {
            (yb.powf(0.56) - yt.powf(0.57)) * 1.14
        } else {
            (yb.powf(0.65) - yt.powf(0.62)) * 1.14
        };
        if sapc.abs() < 0.1 {
            0.0
        } else if sapc > 0.0 {
            (sapc - 0.027) * 100.0
        } else {
            ((sapc + 0.027) * 100.0).abs()
        }
    }

    /// Relative luminance per WCAG 2.x, whose piecewise curve is *not* APCA's.
    fn rel_lum(c: Color32) -> f64 {
        let ch = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    }

    /// The WCAG 2.x contrast ratio: the second bar, and the one with settled
    /// numbers behind it.
    fn ratio(a: Color32, b: Color32) -> f64 {
        let (x, y) = (rel_lum(a), rel_lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Every theme, so a failure names which one.
    fn themes() -> [(&'static str, Palette); 2] {
        [("dark", dark()), ("light", light())]
    }

    /// Text against the grounds it is drawn on, at the `Lc` its role claims and at
    /// 2.2 AA's 4.5:1.
    ///
    /// The grounds are listed one by one rather than assumed. Body text is drawn on
    /// the canvas, on a control and in a field. A palette can pass on one of those
    /// grounds and fail on another.
    #[test]
    fn every_text_pairing_clears_both_bars() {
        for (name, p) in themes() {
            let pairings: &[(&str, Color32, Color32, f64)] = &[
                ("body on canvas", p.text_hi, p.canvas, 90.0),
                ("body on control", p.text_hi, p.btn_rest, 90.0),
                ("body on field", p.text_hi, p.input_bg, 90.0),
                ("secondary on canvas", p.text_mid, p.canvas, 70.0),
                ("accent on canvas", p.accent, p.canvas, 60.0),
                ("caution on canvas", p.warn, p.canvas, 60.0),
                ("destruction on canvas", p.danger, p.canvas, 60.0),
                ("caution on control", p.warn, p.btn_rest, 60.0),
                ("destruction on control", p.danger, p.btn_rest, 60.0),
            ];
            for (what, fg, bg, want_lc) in pairings {
                let (got_lc, got_ratio) = (lc(*fg, *bg), ratio(*fg, *bg));
                assert!(
                    got_lc >= *want_lc,
                    "{name}: {what} is Lc {got_lc:.1}, under the Lc {want_lc:.0} its role claims"
                );
                assert!(
                    got_ratio >= 4.5,
                    "{name}: {what} is {got_ratio:.2}:1, under 2.2 AA's 4.5:1"
                );
            }
        }
    }

    /// The boundary that identifies a control at rest, against every ground it
    /// is drawn on, at WCAG 2.2 SC 1.4.11's 3:1.
    ///
    /// In this palette, SC 1.4.11 rests on this pairing alone.
    /// [`control_fills_do_not_identify_a_control`] pins the premise behind that.
    #[test]
    fn control_boundaries_clear_the_non_text_bar() {
        for (name, p) in themes() {
            for (ground, bg) in [
                ("canvas", p.canvas),
                ("a control's fill", p.btn_rest),
                ("a field's fill", p.input_bg),
            ] {
                let got = ratio(p.line_strong, bg);
                assert!(
                    got >= 3.0,
                    "{name}: a control's boundary is {got:.2}:1 against {ground}, under \
                     SC 1.4.11's 3:1 -- and the fill does not identify the control either"
                );
            }
        }
    }

    /// A focused control's own edge, against its fill and the ground around it.
    ///
    /// egui draws a keyboard-focused widget in the *pressed* visuals, so a focused
    /// control's edge is `accent_hover` over `btn_active`. If either color moves,
    /// this test checks that both sides of that edge stay visible. The ring drawn
    /// outside the control is measured on its own, in
    /// [`the_focus_ring_clears_both_bars_on_every_ground_it_stands_on`].
    #[test]
    fn a_focused_controls_edge_clears_the_non_text_bar() {
        for (name, p) in themes() {
            for (against, bg) in [("its own fill", p.btn_active), ("the canvas", p.canvas)] {
                let got = ratio(p.accent_hover, bg);
                assert!(
                    got >= 3.0,
                    "{name}: a focused control's edge is {got:.2}:1 against {against}, under 3:1"
                );
            }
        }
    }

    /// The focus ring, in the color it is drawn in, against every ground it stands
    /// on.
    ///
    /// The ring stands clear of the control, so the ground on both sides of it is
    /// whatever the control is drawn on. That is the page, or a striped grid row,
    /// where the device and disk lists put their `Use` buttons. The color is read
    /// through [`ring_color`] from the installed [`visuals`], so a change to either
    /// the palette or the mapping is measured. The ring is held to WCAG 2.2's 3:1
    /// for non-text contrast, and to the `Lc 15` border target the module sets.
    #[test]
    fn the_focus_ring_clears_both_bars_on_every_ground_it_stands_on() {
        for (name, p, base) in [
            ("dark", dark(), Visuals::dark()),
            ("light", light(), Visuals::light()),
        ] {
            let v = visuals(&p, base);
            let ring = ring_color(&v);
            for (ground, bg) in [
                ("the page", v.panel_fill),
                ("a striped row", v.faint_bg_color),
            ] {
                let (got_ratio, got_lc) = (ratio(ring, bg), lc(ring, bg));
                assert!(
                    got_ratio >= 3.0,
                    "{name}: the focus ring is {got_ratio:.2}:1 against {ground}, under 3:1"
                );
                assert!(
                    got_lc >= 15.0,
                    "{name}: the focus ring is Lc {got_lc:.1} against {ground}, under Lc 15"
                );
            }
        }
    }

    /// The premise that makes [`control_boundaries_clear_the_non_text_bar`]
    /// necessary.
    ///
    /// The fills are near-identical to the canvas by design, because the visual
    /// direction is a restrained, flat surface. Only the boundary distinguishes a
    /// control. This test asserts that premise, not a target. If a future palette
    /// gives controls a fill that identifies them on its own, this test fails. The
    /// claim that the boundary carries SC 1.4.11 alone is then revisited, rather
    /// than resting on a premise that is no longer true.
    #[test]
    fn control_fills_do_not_identify_a_control() {
        for (name, p) in themes() {
            let got = ratio(p.btn_rest, p.canvas);
            assert!(
                got < 3.0,
                "{name}: a control's fill is now {got:.2}:1 against the canvas, which is enough \
                 to identify it on its own -- revisit why the boundary carries SC 1.4.11 alone"
            );
        }
    }

    /// Interactive controls against WCAG 2.2 SC 2.5.8's 24x24 px minimum.
    ///
    /// The 44 px of SC 2.5.5 (AAA) is the standing aim and is **not** met. This
    /// test does not assert it. It pins the floor that is cleared. If spacing is
    /// tuned below that floor, the test fails.
    ///
    /// **`interact_size` is a floor for some widgets and inert for others.** It is
    /// therefore not the target-size claim, and this test pins only what it binds.
    ///
    /// A `Button` is taller than 30 px from its text and padding alone, so this
    /// number never reaches it. `Button::small` skips the floor by construction
    /// (`button.rs`: "Min size height always equal or greater than interact size if
    /// not small"). A `TextEdit`'s height is its rows plus its own margin, and does
    /// not consult the floor at all. What the floor binds is a radio and a checkbox.
    ///
    /// The target-size claim is measured on drawn geometry, in the tests of `ui`,
    /// where there is a frame to measure:
    /// `ui::tests::every_target_clears_the_minimum_size_or_its_spacing_exception`. A
    /// constant cannot answer SC 2.5.8, because the criterion concerns the rectangle
    /// a person aims at and what is next to it.
    #[test]
    fn interact_size_holds_the_floor_it_binds() {
        let mut style = super::Style::default();
        super::tune(&mut style);
        let h = style.spacing.interact_size.y;
        assert!(
            h >= 24.0,
            "the widgets interact_size binds are {h} px tall, under SC 2.5.8's 24 px"
        );
    }
}
