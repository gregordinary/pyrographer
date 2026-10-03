//! Drawing, and only drawing.
//!
//! The module is thin on purpose. The decisions are made in [`state`], which has no
//! egui in it and is tested without one. Whatever this module leaves untested
//! therefore holds none of the decisions. What is here is layout, wording, and
//! which button is grayed out.
//!
//! Two of its behaviors are not decoration:
//!
//! - **The plan is a screen.** While a plan waits to be confirmed, it takes the
//!   whole window. It is the last thing a person reads before flash is
//!   overwritten, so it cannot be skimmed past on the way to a button.
//! - **A verb a backend cannot perform is drawn disabled, with its reason
//!   showing.** It is not hidden. A grayed `erase` that says why erase is refused
//!   tells a person something a missing button does not.

use eframe::egui;
use pyrographer_core::Error;
use pyrographer_core::agent::FlashInfo;
use pyrographer_core::block::BlockDevice;
use pyrographer_core::codec::console as console_codec;
use pyrographer_core::codec::rockusb::ResetMode;
use pyrographer_core::discovery::{DeviceInfo, Mode};
use pyrographer_core::fill::FillReport;
use pyrographer_core::partition::{Partition, PartitionTable, TableFormat};
use pyrographer_core::progress::Progress;
use pyrographer_core::soc::Soc;
use pyrographer_core::uboot::Gadget;
use pyrographer_core::verbs::{
    ClonePlan, ParamMedium, SegmentedPlan, TableAction, Touches, WritePlan,
};

use crate::app::{App, AuthorSourceKind, Tab, Which};
use crate::platform;
use crate::state::{
    self, Aim, Chosen, Confirmation, Measured, Outcome, Plan, Report, Stoppable, Table, Task,
};

use crate::app::{PortField, RecoveryFileKind};
use pyrographer_core::recovery::RecoveryTarget;

use crate::app::{IngenicStageKind, MaskromStageKind};

/// Draw a frame.
pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    crate::theme::page_frame(ui.style()).show(ui, |ui| {
        // The page stops growing at a width the layout was drawn for, and stays
        // anchored left. What this governs is mostly the things that span -- the
        // header's rule, the rail under the tabs, the separators between sections
        // -- which otherwise stretch to whatever the monitor happens to be.
        ui.set_max_width(ui.available_width().min(crate::theme::PAGE_WIDTH));

        // **Drawn before the gates, and that is deliberate.** The gate's "no
        // escape hatch" property is about *navigation*: there is no tab to leave
        // through and no way to scroll the plan out of sight. A one-line strip
        // that reports a running job and stops it is not an escape hatch -- and
        // taking it off the screen the moment a plan lands is the exact failure
        // the strip exists to prevent, because a dump started before the plan is
        // still running behind it and Cancel is the only control anybody has over
        // a job already under way.
        job_strip(app, ui);

        // The gate. A plan waiting for an answer is the only thing on the
        // screen, because a plan somebody scrolled past is a plan somebody did
        // not read.
        if app.session.pending.is_some() {
            plan_screen(app, ui);
            return;
        }
        // The serial flow's gate, held to the same standard.
        if app.session.recovery.pending.is_some() {
            recovery_plan_screen(app, ui);
            return;
        }
        main_screen(app, ui);
    });
}

/// The tone of a screen's header.
///
/// A gate uses the heat scale: its title is in the destructive color, and its
/// subtitle in the caution color. A screen that overwrites flash therefore looks
/// hotter than one that only lists or reads.
enum Tone {
    /// An ordinary screen.
    Neutral,
    /// A gate confirming an irreversible write.
    Danger,
}

/// Draw a screen's header the same way on every screen: a title, a subtitle, and
/// the theme toggle pinned to the right.
///
/// The main screen and the two gates all draw their title bar through this
/// function, so a header is decided in one place.
fn screen_header(ui: &mut egui::Ui, title: &str, subtitle: &str, tone: Tone) {
    ui.horizontal(|ui| {
        let title = match tone {
            Tone::Neutral => egui::RichText::new(title),
            Tone::Danger => egui::RichText::new(title).color(ui.visuals().error_fg_color),
        };
        ui.heading(title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            crate::theme::theme_toggle(ui);
        });
    });
    match tone {
        Tone::Neutral => {
            ui.label(subtitle);
        }
        Tone::Danger => {
            ui.colored_label(ui.visuals().warn_fg_color, subtitle);
        }
    }
    ui.separator();
}

/// A section heading, in the cool accent.
///
/// The accent is the instrument chrome that is always on screen. The heat scale is
/// spent only where flash is at stake. The accent gives an idle window (no board,
/// no plan, nothing warm to show) the palette's identity, where it would otherwise
/// be a wall of gray.
fn section_label(ui: &mut egui::Ui, text: impl Into<String>) {
    let accent = ui.visuals().hyperlink_color;
    ui.label(egui::RichText::new(text.into()).color(accent).strong());
}

/// A paragraph of explanatory prose, at full text contrast.
///
/// [`egui::Ui::weak`] is for short inline hints, such as "required", "none chosen"
/// and "busy". Body text of a sentence or more takes a paragraph's contrast
/// instead of a hint's gray. A long weak paragraph is legible but tiring to read,
/// more so on the light ground. The rule is length: a word or a phrase is weak,
/// and prose is not.
fn prose(ui: &mut egui::Ui, text: impl Into<String>) {
    measured(ui, |ui| {
        ui.label(text.into());
    });
}

/// Hold a block of text to a readable measure.
///
/// This opens a scope instead of calling `ui.set_max_width` on the caller's own
/// [`egui::Ui`]. That call would also narrow everything drawn after it, including
/// the rows and grids that span the full page.
/// [`theme::MEASURE`](crate::theme::MEASURE) gives the width and how it was
/// chosen.
fn measured(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.scope(|ui| {
        ui.set_max_width(ui.available_width().min(crate::theme::MEASURE));
        add(ui);
    });
}

/// A control's reason, given to the accessibility tree as well as the tooltip.
///
/// egui maps a widget's role, name, value and disabled flag into the AccessKit
/// tree, and nothing from a tooltip. `on_hover_text` renders a transient
/// [`egui::Label`] into a hover `Area`, and no part of that reaches the node. A
/// screen reader therefore never reads a reason carried only by a tooltip. A
/// tooltip also needs a pointer, so a keyboard never reaches it either.
///
/// This seam carries the reasons that explain a control. The tooltip stays, for a
/// pointer user. The same words are attached to the node as its description, which
/// `accesskit_atspi_common` exposes as AT-SPI `Accessible.Description`.
///
/// A reason that guards a destructive act does **not** go through this seam.
/// [`guard`] draws it as a sentence instead. A description is invisible to a
/// sighted keyboard user, and the web build has no accessibility tree at all.
trait Explain {
    /// Attach a reason to a control that is available.
    fn explain(self, text: impl Into<String>) -> Self;

    /// Attach the reason a control is grayed out.
    ///
    /// The description is set only while the control is disabled, so an enabled
    /// button carries no explanation of a state it is not in.
    fn explain_disabled(self, text: impl Into<String>) -> Self;

    /// Give a control an accessible name that says which thing it acts on.
    ///
    /// A row of devices draws a `Use` button per row. Six buttons called `Use` and
    /// `Clone from` leave a keyboard user tabbing through identical labels, with
    /// nothing to say which disk each one selects. That button picks the target
    /// for destructive work. A grid does not help, because a row is a layout
    /// concept and not a node. `end_row` builds no container, so nothing between
    /// the button and the whole grid carries the context.
    ///
    /// The visible text stays short and the accessible name grows, as WCAG 2.2 SC
    /// 2.5.3 requires. The visible label must be contained in the accessible name.
    /// `Use` and `Use /dev/sdb` therefore agree, and `Select /dev/sdb` would not.
    fn named(self, name: impl Into<String>) -> Self;
}

impl Explain for egui::Response {
    fn explain(self, text: impl Into<String>) -> Self {
        let text = text.into();
        if self.enabled() {
            describe(&self, &text);
        }
        self.on_hover_text(text)
    }

    fn explain_disabled(self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !self.enabled() {
            describe(&self, &text);
        }
        self.on_disabled_hover_text(text)
    }

    fn named(self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.ctx.accesskit_node_builder(self.id, |node| {
            node.set_label(name);
        });
        self
    }
}

/// Attach a description to a response's accessibility node.
///
/// The widget has already built the node from its `WidgetInfo`, so this only adds
/// a field to it. When AccessKit is off, it does nothing. AccessKit is off in
/// every build that no assistive technology is reading, and in every headless
/// frame that has not asked for a tree.
fn describe(response: &egui::Response, text: &str) {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_description(text);
    });
}

/// Give a control that has no text of its own the name drawn beside it.
///
/// A [`egui::TextEdit`] and a [`egui::DragValue`] reach the accessibility tree
/// with `label: None`, because egui has no text to give them. A screen reader
/// announces the role and stops. The `ui.label` drawn beside one is a separate
/// node with no relation to it.
///
/// `labelled_by` relates the two, and it works here because these controls have
/// no direct label. When a node has a direct label, `accesskit_consumer` composes
/// its name from that label. When it has none, it falls back to the `labelled_by`
/// targets. On a button the same call would be inert, because the button's own
/// text wins. This helper is therefore for fields only.
fn named_field(
    ui: &mut egui::Ui,
    name: &str,
    add: impl FnOnce(&mut egui::Ui) -> egui::Response,
) -> egui::Response {
    let label = ui.label(name);
    add(ui).labelled_by(label.id)
}

/// A reason that guards a destructive act, drawn as a sentence.
///
/// In the house style, a grayed button explains itself, and a tooltip is not
/// enough. A tooltip needs a pointer, so a keyboard user never sees it, and it
/// reaches no accessibility tree. The reasons that stand between a person and
/// overwritten flash are therefore drawn as text, in the caution color, next to
/// what they are about.
///
/// It is kept scarce on purpose, as the palette is. If every reason were a
/// sentence, a person would stop reading the ones that guard a write. This
/// function is for a **capability refusal**, a permanent fact about this device or
/// this backend, and for anything on a plan screen. A prompt about the state of
/// the form ("choose an image first") is recoverable by looking, and takes
/// [`Explain::explain_disabled`] instead.
fn guard(ui: &mut egui::Ui, text: impl Into<String>) {
    let color = ui.visuals().warn_fg_color;
    measured(ui, |ui| {
        ui.colored_label(color, text.into());
    });
}

/// Whether a gate has already placed keyboard focus on its confirmation field.
///
/// A gate that appears must put focus somewhere real. A gate already on screen
/// must not take focus back every frame, or focus could never leave the field.
/// [`main_screen`] clears the flag. It draws whenever no gate is up, so the next
/// gate places focus again.
///
/// The flag is keyed per gate, because two gates can be waiting at once. A write
/// plan is drawn in preference to a recovery plan. Dismissing the first takes a
/// person straight to the second, with no main screen in between. A single flag
/// would be set by the first gate, and the second would then place no focus at
/// all. That is the case [`place_gate_focus`] exists to fix.
const GATE_FOCUS_PLACED: &str = "pyrographer_gate_focus_placed";

/// Which gate a focus flag belongs to. See [`GATE_FOCUS_PLACED`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Gate {
    /// The USB write gate: a write, a clone, or a table write.
    Write,
    /// The serial flow's gate: a StarFive recovery.
    Recovery,
}

impl Gate {
    /// Both gates, so the main screen clears every flag instead of only the one it
    /// remembers.
    const ALL: [Gate; 2] = [Gate::Write, Gate::Recovery];

    /// This gate's focus flag.
    fn focus_id(self) -> egui::Id {
        egui::Id::new((GATE_FOCUS_PLACED, self as u8))
    }
}

/// Put keyboard focus on a gate's confirmation control, once, as the gate appears.
///
/// **When a screen is replaced, egui does not clear focus.** A person who tabbed
/// onto a button on the main screen arrives at the write gate with focus still on
/// that button. The button is no longer drawn, so Space does nothing and no focus
/// ring appears. The screen looks unfocused while the keyboard points at a control
/// that is gone. This matters most on the gate, which cannot be skipped and is
/// where a write is confirmed.
///
/// Focus is not consent. This saves the tabbing and nothing else: the coordinate
/// must still be typed and the button still pressed.
fn place_gate_focus(ui: &mut egui::Ui, gate: Gate, field: &egui::Response) {
    let id = gate.focus_id();
    if ui
        .memory(|memory| memory.data.get_temp::<bool>(id))
        .is_some()
    {
        return;
    }
    field.request_focus();
    ui.memory_mut(|memory| memory.data.insert_temp(id, true));
}

/// The flow bar, exposed to assistive technology as a tab bar.
///
/// egui has no tab widget. `Button::selectable` reaches the accessibility tree as
/// `Role::Button` carrying a `Toggled`. A screen reader therefore announces
/// "Boards and disks button, pressed". That describes a control that is on, and
/// says neither that it is one of a set nor which one is current.
///
/// AccessKit has roles that say both. [`egui::Context::accesskit_node_builder`]
/// sets them after the widget has built its node, through the seam
/// [`Explain::named`] and [`describe`] also use. The bar publishes
/// `Role::TabList` over two `Role::Tab` children, with `selected` on the current
/// one, and adds no dependency.
///
/// **The current tab is marked by a shape, not by a shade**, a choice that follows
/// from measurement. egui paints a selected button in `selection.bg_fill` behind
/// `selection.stroke` text. Against this palette's canvas, that fill measures
/// 2.11:1 in dark and 1.61:1 in light, under the 3:1 a sole indicator needs. The
/// light theme's selected text measures 2.84:1, which fails 4.5:1.
///
/// The resting control boundary measured the same way. The fills in this palette
/// are too faint to carry an indicator, so anything that must be seen is drawn as
/// a line. The rule under the current tab is in the accent. The palette already
/// measures the accent against both contrast bars (APCA and the WCAG 2.2 ratio)
/// for the focus ring. The label color reinforces the rule and does not carry the
/// indication on its own.
fn tab_bar(app: &mut App, ui: &mut egui::Ui) {
    let accent = ui.visuals().hyperlink_color;
    let rail = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let chosen = app.tab;

    // Where the mark goes, collected while the row is laid out and drawn after
    // it, so the mark and the rail it sits on share one baseline.
    let mut mark = None;

    let bar = ui.horizontal(|ui| {
        // **A tab at rest carries no frame.** What makes a row of words read as
        // tabs rather than as buttons is the rail they sit on and the mark under
        // the current one -- a framed tab is just a button that happens to be at
        // the top of the page. Only the resting look is cleared: hovering keeps
        // egui's own fill, so pointing at one still answers, and the padding stays
        // whatever the button padding is, which is what keeps the target big
        // enough to hit.
        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;

        for tab in Tab::ALL {
            let current = tab == chosen;
            let label = if current {
                egui::RichText::new(tab.name()).color(accent)
            } else {
                egui::RichText::new(tab.name())
            };

            // A plain button, not `selectable_label`: its selected styling is the
            // fill this palette cannot show.
            let response = ui.button(label).explain(tab.describe());
            if response.clicked() {
                app.tab = tab;
            }

            // The roles egui does not have. Set after the widget built its node,
            // the same way a name is.
            response.ctx.accesskit_node_builder(response.id, |node| {
                node.set_role(egui::accesskit::Role::Tab);
                node.set_selected(current);
            });

            if current {
                mark = Some(response.rect.x_range());
            }
        }
    });

    bar.response
        .ctx
        .accesskit_node_builder(bar.response.id, |node| {
            node.set_role(egui::accesskit::Role::TabList);
        });

    // **The rail, and the mark that sits on it.** The rail runs the width of the
    // page and is what the tabs stand on; the mark is the same line in the accent,
    // thicker, over the stretch the current tab occupies. Drawn second so it
    // covers the rail rather than sitting beside it -- one line, two colors, and
    // the join is where the tab is.
    let y = bar.response.rect.bottom() + RULE + 2.0;
    ui.painter()
        .hline(ui.max_rect().x_range(), y, egui::Stroke::new(1.0, rail));
    if let Some(x) = mark {
        ui.painter().hline(x, y, egui::Stroke::new(RULE, accent));
    }
    ui.add_space(RULE + 8.0);
}

/// The thickness of the rule under the current tab. It matches the focus ring's
/// two pixels, for the same reason: it must read as a mark and not as the edge of
/// something.
const RULE: f32 = 2.0;

/// The window while no plan waits to be confirmed.
fn main_screen(app: &mut App, ui: &mut egui::Ui) {
    // No gate is up, so the next one to come up places focus afresh -- either of
    // them, because either could be the next one.
    ui.memory_mut(|memory| {
        for gate in Gate::ALL {
            memory.data.remove::<bool>(gate.focus_id());
        }
    });

    screen_header(
        ui,
        "pyrographer",
        "Flashing and recovery for embedded devices.",
        Tone::Neutral,
    );

    tab_bar(app, ui);

    egui::ScrollArea::vertical().show(ui, |ui| {
        match app.tab {
            Tab::Flash => flash_tab(app, ui),
            Tab::Serial => serial_tab(app, ui),
        }

        // **Under both tabs, because there is one of it.** Every flow reports
        // into the same slot -- a dump, a bootstrap, a recovery and a console
        // session all end here -- so drawing it inside a tab would be choosing
        // one flow whose answers a person can read. It is the answer to the last
        // thing they did, and which tab they are looking at now does not change
        // what that was.
        if let Some(outcome) = &app.session.last {
            report(outcome, ui);
        }
    });
}

/// Every job in flight, on a strip outside the tabs.
///
/// A window split by flow can put a running job behind a tab. Progress out of
/// sight is a nuisance, and **Cancel** out of sight leaves a person no way to
/// stop the job. A write stops only between windows, and stopping it there is the
/// only control a person has over a write already under way. A job in flight is
/// therefore window furniture and not a tab's content. It is
/// drawn in a bottom panel, which keeps its place whichever tab is up and however
/// far the content has scrolled.
///
/// Each job takes one line, and the line names the tab the job belongs to. The
/// strip is deliberately not the whole panel. The byte counts, the read-back note
/// and the transcript stay in the job's own tab, and the strip says where to
/// look. When nothing is running, nothing is drawn at all. A permanent empty strip
/// would teach a person to stop reading the place where a warning appears.
///
/// This cancel is a second route to the one the job's own panel offers, and does
/// not replace it. A person watching a write uses the button under the progress
/// bar. A person who has looked away can cancel without finding the tab first.
fn job_strip(app: &mut App, ui: &mut egui::Ui) {
    if !app.session.anything_running() {
        return;
    }

    egui::Panel::bottom("pyrographer_jobs")
        .frame(egui::Frame::new().inner_margin(egui::Margin {
            top: 6,
            bottom: 4,
            ..Default::default()
        }))
        .show(ui, |ui| {
            ui.add_space(2.0);

            if let Some(job) = &app.session.job {
                let text = job.fraction().map(|_| {
                    format!(
                        "{} of {}",
                        human_bytes(job.done_bytes()),
                        human_bytes(job.total_bytes().unwrap_or(0))
                    )
                });
                running_row(ui, Tab::Flash, job.label, text, job.is_canceling(), || {
                    job.cancel();
                });
            }
            if let Some(job) = &app.session.bootstrap {
                running_row(
                    ui,
                    Tab::Flash,
                    "Uploading loader",
                    None,
                    job.is_canceling(),
                    || job.cancel(),
                );
            }
            if let Some(job) = &app.session.ingenic_bootstrap {
                running_row(
                    ui,
                    Tab::Flash,
                    "Bootstrapping to DFU",
                    None,
                    job.is_canceling(),
                    || job.cancel(),
                );
            }
            if let Some(job) = &app.session.recovery.job {
                running_row(
                    ui,
                    Tab::Serial,
                    "Recovering over serial",
                    None,
                    job.is_canceling(),
                    || job.cancel(),
                );
            }
            if let Some(job) = &app.session.console.job {
                running_row(ui, Tab::Serial, job.label, None, job.is_canceling(), || {
                    job.cancel();
                });
            }

            ui.add_space(2.0);
        });
}

/// One running job's line on the strip.
///
/// The line names the tab instead of leaving it implied by which tab is up. The
/// strip exists because the job can be on the side nobody is looking at. The
/// Cancel carries the job's name into the accessibility tree, as a disk's `Use`
/// button carries the disk's. Two jobs can run at once, and a control called only
/// "Cancel" would not say which one it stops.
fn running_row(
    ui: &mut egui::Ui,
    home: Tab,
    label: &str,
    progress: Option<String>,
    canceling: bool,
    cancel: impl FnOnce(),
) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.strong(label);
        ui.weak(format!("({})", home.name()));
        if let Some(progress) = progress {
            ui.weak(progress);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if canceling {
                ui.weak("stopping at the next window...");
                return;
            }
            // Canceling is not an undo. What a write has already put on the
            // flash stays there; stopping between windows only means the device
            // is never left partway through a command.
            if ui
                .button("Cancel")
                .named(format!("Cancel {label}"))
                .clicked()
            {
                cancel();
            }
        });
    });
}

/// The tab for everything that has a flash agent, and the uniform verbs they share.
///
/// It holds boards on the bus, this machine's disks, and the bootstraps that bring
/// a board to a flash agent. The two acquisitions are drawn together because they
/// fill the same slot. A [`Chosen`] is a board or a disk, and every verb drawn here
/// is written once for both. The bootstrap panels belong in this tab because a
/// board with no flash agent yet is still a row in this list. The one action
/// offered for it brings it into this flow.
fn flash_tab(app: &mut App, ui: &mut egui::Ui) {
    devices(app, ui);
    ui.add_space(8.0);

    // The machine's own disks: the same slot, the same verbs, and a section
    // that stays shut until somebody opens it. A board is on the bus because
    // they put it there; a disk is not.
    disks(app, ui);
    ui.add_space(8.0);

    // **The board a clone copies, when one is chosen, and above the board it
    // overwrites.** The clone plan lists them in that order for the reason it
    // colors only one of them: the source is read and the target is destroyed,
    // and the mistake this whole flow exists to catch is the two the wrong way
    // round. A reading order that matches the plan's is one more place a person
    // can notice it before the gate.
    //
    // Drawn on the same terms as the target's -- only once something is in the
    // slot -- so a window nobody is cloning with looks exactly as it did.
    if app.board(Which::Source).device.is_some() {
        board(app, ui, Which::Source);
        ui.add_space(8.0);
    }

    // The board panel and its verbs appear only once a board is chosen. Until
    // then the scan area leads and there is no empty "none chosen" stub on the
    // idle screen: a method is revealed when a device supports it, not before.
    if app.board(Which::Target).device.is_some() {
        board(app, ui, Which::Target);
        ui.add_space(8.0);

        // Idle, running a job, or finished: in all three the verbs panel is
        // drawn. A desynchronized board draws it **disabled, with the reason**,
        // rather than vanishing -- a panel that disappears teaches nothing, and
        // the buttons gray themselves out on the connection state anyway.
        let connection = &app.board(Which::Target).connection;
        if connection.is_idle() || connection.is_desynchronized() || app.session.job.is_some() {
            verbs_panel(app, ui);
            ui.add_space(8.0);
        }
    }

    if let Some(job) = &app.session.job {
        job_panel(app, job, ui);
        ui.add_space(8.0);
    }

    if let Some(bootstrap) = &app.session.bootstrap {
        bootstrap_panel(app, bootstrap, ui);
        ui.add_space(8.0);
    }

    if let Some(bootstrap) = &app.session.ingenic_bootstrap {
        ingenic_bootstrap_panel(app, bootstrap, ui);
        ui.add_space(8.0);
    }
}

/// The tab for what answers on a serial line, which has no flash agent and no
/// block surface.
///
/// Two things reach a board this way, and they form one flow. One is a StarFive
/// ROM waiting for an image, and the other is a bootloader prompt waiting for
/// input. Both are declared rather than discovered. Nothing on a serial wire
/// announces what is connected to it, so a person names the port. This tab
/// therefore opens with a form, and the flash tab opens with a list.
fn serial_tab(app: &mut App, ui: &mut egui::Ui) {
    // Serial, named-port, write-only; its own section rather than grayed USB
    // verbs, because a recovery board has no LBAs, no partitions, and no
    // read-back to gray out.
    recovery_entry(app, ui);
    ui.add_space(8.0);

    if let Some(job) = &app.session.recovery.job {
        recovery_job_panel(app, job, ui);
        ui.add_space(8.0);
    }

    // The other thing that answers on a serial line: a bootloader prompt.
    // Inside the same flow, because a console is reached exactly as a recovery
    // is -- a person names a port.
    console_entry(app, ui);
    ui.add_space(8.0);

    // Drawn inline rather than as a screen of its own, unlike the two write
    // gates. An override sets `boot_targets` in RAM and boots; it overwrites
    // nothing and there is no second board to confuse with the first, and
    // ceremony attached to a harmless act is how the ceremony attached to a
    // dangerous one stops being read.
    if app.session.console.pending.is_some() {
        boot_plan(app, ui);
        ui.add_space(8.0);
    }

    if let Some(job) = &app.session.console.job {
        console_job_panel(job, ui);
        ui.add_space(8.0);
    } else if !app.session.console.transcript.is_empty() {
        // The session is over, and what it printed is its answer -- so the
        // transcript outlives the job that read it.
        let mut clear = false;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong("Console transcript");
                // The transcript is what holds the section open after a
                // session ends, so there has to be a way to say it has been
                // read.
                clear = ui.small_button("Clear").clicked();
            });
            transcript_box(ui, &app.session.console.transcript);
        });
        if clear {
            app.session.console.clear_transcript();
        }
        ui.add_space(8.0);
    }
}

/// The device-acquisition seam, drawn.
///
/// Natively, the window scans the bus and owns the list, so every Rockchip and
/// Ingenic board on it appears. A maskrom board is listed and classified. It is
/// offered an "Upload loader" job of its own, the bootstrap that brings it to loader
/// mode. An Ingenic boot-ROM board is offered its bootstrap to DFU in the same way.
/// Neither is offered the block verbs, because its flash is not yet reachable.
#[cfg(not(target_arch = "wasm32"))]
fn devices(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        section_label(ui, "Devices");
        // The bus is polled at rest anyway. The button is for the moment somebody
        // plugs a board in and does not want to wait for the next second to come
        // round.
        if ui
            .button("Rescan")
            .named("Rescan the bus for boards")
            .clicked()
        {
            app.session.force_rescan();
        }
        if app.session.job.is_some() {
            ui.weak("(paused while a job runs)");
        }
    });

    if app.session.devices.is_empty() {
        // Every vendor the scan covers, not one of them: `verbs::list` sweeps
        // Rockchip *and* Ingenic, and the rows below classify and draw boot-ROM
        // and DFU boards.
        ui.weak("No board in a boot or recovery mode is connected.");
        // **Said here and not always.** Somebody with a card reader plugged in and
        // no board has found nothing, and the thing they are looking for is one
        // section away -- shut by default, because a disk is not on the bus
        // because they put it there. The CLI says the same at the same moment.
        if !app.session.disks.show {
            ui.weak("This machine's own disks are under Disks, below.");
        }
        return;
    }

    device_rows(app, ui);
}

/// The device list, drawn.
///
/// This is one piece of code in both builds. A row shows what a board is and
/// offers the two actions available for it, and neither depends on how the board
/// was found. Only the source of the list differs: a bus scan natively, and a list
/// of already-granted devices in a tab. [`platform::handle_of`] turns the listing
/// a row draws into the handle that reopens the board.
///
/// The leading `bus:address` column appears only natively, where there is a
/// coordinate to show. A browser has no place to name, so in a tab the column is
/// left out instead of drawn empty. [`state::coordinate`] explains why, and why
/// the web write gate asks for a re-pick instead of a typed coordinate.
fn device_rows(app: &mut App, ui: &mut egui::Ui) {
    // **Taken out of the session for the frame and put back at the end**, rather
    // than cloned. The rows below need `app` mutably -- `Use` re-points a slot --
    // and the listing is what they are drawn from, so one of the two has to give.
    // A clone was a heap allocation every frame for a borrow-checker reason rather
    // than a semantic one; a swap is the same maneuver with no copy in it, and
    // nothing between here and the restore can observe the empty list.
    let devices = std::mem::take(&mut app.session.devices);
    let coordinates = devices
        .iter()
        .any(|device| !state::usb_coordinate(device).is_empty());

    egui::Grid::new("devices")
        .num_columns(if coordinates { 4 } else { 3 })
        .striped(true)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            if coordinates {
                column_headers(ui, &["Address", "Ids", "Mode", ""]);
            } else {
                column_headers(ui, &["Ids", "Mode", ""]);
            }
            for device in &devices {
                if coordinates {
                    ui.monospace(state::usb_coordinate(device));
                }
                ui.monospace(format!(
                    "{:04x}:{:04x}",
                    device.vendor_id, device.product_id
                ));
                mode_label(device, ui);

                ui.horizontal(|ui| {
                    // Disabled while the board slot is opening or busy: re-pointing
                    // a board whose agent is out on a job or on its way in would
                    // land that agent under this row's identity, and the write gate
                    // would then name a board the write never reaches. See
                    // [`App::can_select`].
                    // Named for the board, for the reason the disk rows are: the
                    // row is not a node, so the button has to carry which device
                    // it acts on. The coordinate when there is one, the ids when
                    // there is not -- a browser has no place to name.
                    let which = board_name(device);
                    if ui
                        .add_enabled(app.can_select(Which::Target), egui::Button::new("Use"))
                        .named(format!("Use {which}"))
                        .explain_disabled(
                            "The current target is opening or busy with a job. Wait for it to \
                             finish before choosing another.",
                        )
                        .clicked()
                    {
                        app.session
                            .select_target(platform::handle_of(device), device.clone());
                    }
                    if ui
                        .add_enabled(
                            app.can_select(Which::Source),
                            egui::Button::new("Clone from"),
                        )
                        .named(format!("Clone from {which}"))
                        .explain_disabled(
                            "The board to copy is opening or busy with a job. Wait for it to \
                             finish before choosing another.",
                        )
                        .clicked()
                    {
                        app.session
                            .select_source(platform::handle_of(device), device.clone());
                    }
                });
                ui.end_row();
            }
        });

    app.session.devices = devices;
}

/// The machine's own disks, listed only on request.
///
/// This is the window's form of the CLI's `list` against `list --blocks`. A board
/// is on the bus because somebody put it there, so listing it tells them nothing
/// new. The machine's disks are different. Drawing them beside a board, with
/// nothing to tell the two apart, invites the mistake the Block backend's
/// refusals exist to prevent. The section therefore stays closed until it is
/// opened.
///
/// The section is drawn on every build, including builds with no Block backend.
/// An absent section would look exactly like a machine with no disks, and send a
/// person looking for the card reader they can see. A section that says the
/// backend is Linux-only answers the question they have.
fn disks(app: &mut App, ui: &mut egui::Ui) {
    let mut show = app.session.disks.show;
    ui.horizontal(|ui| {
        let arrow = if show { "v" } else { ">" };
        if ui
            .button(format!("{arrow}  Disks"))
            .explain(
                "This machine's block devices, such as an SD card in a reader or a board that \
                 has come up as mass storage. Unlike every other target here, the operating \
                 system also owns them.",
            )
            .clicked()
        {
            show = !show;
        }
        if show && app.session.disks.asked {
            if ui
                .button("Rescan")
                .named("Rescan this machine's disks")
                .clicked()
            {
                app.list_disks();
            }
            ui.weak("(listed on request, not polled)");
        }
    });
    if show != app.session.disks.show {
        app.show_disks(show);
    }
    if !app.session.disks.show {
        return;
    }

    if let Some(problem) = app.session.disks.problem.clone() {
        ui.colored_label(ui.visuals().warn_fg_color, problem);
        return;
    }
    if app.session.disks.listed.is_empty() {
        ui.weak("No block devices are listed.");
        return;
    }

    // Shorter than it was, and the *reason* is what survived the trim. "Nothing is
    // chosen by default" is a rule somebody forgets; "on a laptop with one disk,
    // the only device there is is the one you are running from" is why the rule
    // exists, and the interface cannot demonstrate that -- the disks it disables
    // are the refused ones, and this sentence is about the ones it does not.
    prose(
        ui,
        "A disk is acted on only when it is named, and nothing here is chosen by default. On a \
         laptop with one disk, the only disk listed is the one the system runs from.",
    );
    ui.add_space(4.0);
    let refused = disk_rows(app, ui);

    // **The refusals, said below the grid rather than in it, and grouped.** Each
    // row carries a `refused` chip, and a chip is all a grid cell has room for --
    // so the reason used to live in a tooltip on a `Label`, which is a control no
    // keyboard can reach and no accessibility tree carries. These are the
    // sentences that stand between somebody and a destroyed machine, so they are
    // drawn.
    //
    // **One sentence per kind, not per disk.** A machine whose root is an LVM
    // volume over a LUKS container over a partition refuses four devices for the
    // same reason, and four copies of the same paragraph is how somebody learns to
    // scroll past the paragraph -- the same argument the palette makes for keeping
    // warmth scarce, in words instead of color. Kinds stay apart because their
    // remedies differ; see [`Refusal`].
    for kind in Refusal::ORDER {
        let nodes: Vec<String> = refused
            .iter()
            .filter(|refusal| refusal.kind == kind)
            .map(|refusal| refusal.node.clone())
            .collect();
        if nodes.is_empty() {
            continue;
        }
        ui.add_space(4.0);
        guard(ui, kind.warning(nodes.len()));
        // **The names below the warning, in the grid's own monospace and the
        // grid's own spelling.** The sentence above used to open with them, so a
        // person read four identifiers before reaching the consequence, and
        // they were set in the body font while the rows above set the same devices
        // in monospace with a `/dev/` prefix -- the same disk, spelled two ways,
        // one line apart. That, rather than the commas, is what made the list hard
        // to scan.
        //
        // Monospace and no background: every fill this palette has is between
        // 1.03:1 and 1.22:1 against the canvas, so a `code`-style chip would be
        // one nobody can see, and manufacturing a visible one needs the 3:1 fill
        // step the flat surface direction rules out. A monospace run against
        // proportional prose carries the distinction on its own.
        ui.indent(("refused", kind as usize), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // Labeled with the column word, so the sentence's "named below"
                // lands on a list that says which fact it is a list of -- and so
                // that several groups on screen at once stay apart when they are
                // scanned rather than read.
                ui.weak(format!("{}:", kind.column_word()));
                ui.monospace(nodes.join(", "));
            });
        });
    }

    // **A disk named twice has two things to clear, and that is said rather than
    // left to be noticed.** The lists are complete and each sentence says what to
    // clear rather than promising the disk is then usable -- but somebody who
    // finds their card under `mounted:`, unmounts it and retries has spent the
    // round trip this completeness exists to save. One line, only when it applies.
    let mut named: Vec<&str> = refused
        .iter()
        .map(|refusal| refusal.node.as_str())
        .collect();
    named.sort_unstable();
    let twice: Vec<&str> = named
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .map(|pair| pair[0])
        .collect();
    if !twice.is_empty() {
        ui.add_space(4.0);
        ui.weak(format!(
            "Listed under more than one reason, and each one must be cleared: {}",
            twice.join(", ")
        ));
    }

    // A reason this window could not sort, drawn on its own in core's words. See
    // [`Refusal::Unrecognized`]: grouping must not be able to swallow a reason.
    for refusal in refused
        .iter()
        .filter(|refusal| refusal.kind == Refusal::Unrecognized)
    {
        ui.add_space(4.0);
        // Core's sentence whole, including how core spells the device. The point
        // of this path is that a reason this window cannot sort still reaches the
        // screen worded by the layer that knows; re-spelling it here would be this
        // code editing a sentence it did not understand well enough to group.
        guard(ui, refusal.core_words.clone());
    }

    // **Named, not hidden.** A device with no capacity is an unbound loop device
    // or a reader with no card in it, and it is not a target for anything -- but
    // dropping it silently would hide the second case, which is exactly the
    // sentence somebody wants when the card they just inserted is not in the list
    // above. So they are counted and named on one line instead of taking a row
    // each: a machine that has eight of them would otherwise bury the one disk
    // this section exists for.
    let empty: Vec<&str> = app
        .session
        .disks
        .listed
        .iter()
        .filter(|disk| disk.bytes == 0)
        .map(|disk| disk.node.as_str())
        .collect();
    if !empty.is_empty() {
        ui.add_space(4.0);
        // The count in prose, the devices in the grid's monospace and the grid's
        // spelling, wrapping rather than growing a line each. Eight loop devices
        // are exactly the case this line exists to compress, so it must stay a
        // line and still be scannable.
        //
        // **The count stays out, the names go behind it.** The count is the part
        // that answers the question somebody actually has -- the card I just put
        // in is not in the list, is it being seen at all? -- so it is in the
        // header and readable without opening anything. The names are for
        // debugging and would otherwise take two lines of monospace directly under
        // a safety warning, competing with it for the same attention.
        //
        // Quiet inside, too: these are the noise this line exists to compress, and
        // the refused devices above are drawn at full contrast because they are
        // the operative ones. The difference in weight is what says which is which.
        let count = if empty.len() == 1 {
            "1 device".to_string()
        } else {
            format!("{} devices", empty.len())
        };
        egui::CollapsingHeader::new(format!("{count} with no medium"))
            .id_salt("no_medium")
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(empty.join(", "))
                        .monospace()
                        .color(ui.visuals().weak_text_color()),
                );
            })
            .header_response
            .explain(
                "An unbound loop device, or a card reader with no card in it. These have nothing \
             to read or write, so they get no row. If a card you inserted is among them, the \
             system does not detect it.",
            );
    }
}

/// Why a disk cannot be acted on, as a kind instead of a sentence.
///
/// Core gives the reason, and core decides whether there is one. [`Refusal::of`]
/// only sorts a refusal that `block::write_refusal` has already made. This type
/// adds the kind, which lets several disks share one sentence. A machine whose
/// root is an LVM volume over LUKS refuses `dm-0`, `dm-1`, `dm-2` and `nvme0n1`.
/// Four copies of the same paragraph teach a person to scroll past it.
///
/// Kinds are not merged, because their remedies differ. A held disk is unmounted
/// and opened again. A read-only disk reflects the kernel's view of it. A disk the
/// running system rests on has no remedy at all. One merged list would put "there
/// is no override" next to a disk a person can simply unmount.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Refusal {
    /// The machine's own system rests on it. It has no remedy and no override.
    RunningSystem,
    /// The kernel marks it read-only.
    ReadOnly,
    /// Something else on this machine has it, so `O_EXCL` will refuse it.
    Held,
    /// A refusal from core that this code does not recognize.
    ///
    /// Grouping is a presentation choice, and it must not be able to hide a
    /// reason. A refusal that sorts into none of the other kinds is drawn on its
    /// own, in core's own words, one line per device. A new reason added to
    /// `block::write_refusal` therefore appears in the window as soon as it is
    /// written, worded correctly and ungrouped. It does not disappear until this
    /// enum learns about it.
    Unrecognized,
}

/// One disk's refusal: which disk, what kind, and what core said about it.
struct DiskRefusal {
    /// The node path, spelled as the row spells it and as the CLI takes it.
    ///
    /// `/dev/dm-0`, not `dm-0`. On the command line, the leading slash separates a
    /// disk from a board (`003:12` against `/dev/sdb`). It is therefore the
    /// spelling a person learns to type. One identifier spelled two ways on one
    /// screen makes a person read it twice.
    node: String,
    kind: Refusal,
    /// Core's own sentence, kept. An unrecognized kind is drawn with it, so a
    /// reason this window cannot sort still reaches the screen in core's own words.
    core_words: String,
}

impl Refusal {
    /// Sort core's refusal into a kind, from the same fields core branched on.
    ///
    /// **This function never returns [`Held`](Self::Held), and the drawing of
    /// `core_words` depends on that.** `block::write_refusal` produces exactly two
    /// reasons: the running system, and read-only. A held disk is neither.
    /// `O_EXCL` refuses it at the open, which covers more than any check this code
    /// could make. `Held` therefore has its own producer in [`disk_rows`], which
    /// supplies no `core_words` because core said none. Only the
    /// [`Unrecognized`](Self::Unrecognized) arm draws `core_words`, and that is
    /// safe only because this function cannot reach `Held`.
    fn of(disk: &BlockDevice) -> Self {
        if disk.carries_running_system {
            Self::RunningSystem
        } else if disk.read_only {
            Self::ReadOnly
        } else {
            Self::Unrecognized
        }
    }

    /// The order the groups are drawn in. The worst comes first, because the first
    /// sentence is the one a person reads.
    const ORDER: [Self; 3] = [Self::RunningSystem, Self::ReadOnly, Self::Held];

    /// The warning a group of disks shares: the consequence, with no device names.
    ///
    /// The names are drawn after this sentence, as their own monospace run, for two
    /// reasons. A person who reads four device names before the consequence has
    /// read them without knowing why they matter. A sentence that embeds its list
    /// also grows a line for every disk, where a separate run wraps.
    ///
    /// The plural is not cosmetic. "The disks below hold the running system" is a
    /// different claim from four separate claims that each disk does. On a machine
    /// whose root is a stack of device-mapper layers over a partition, the plural
    /// claim is the true one.
    fn warning(self, count: usize) -> &'static str {
        let one = count == 1;
        match (self, one) {
            (Self::RunningSystem, true) => {
                "The disk below holds the running system, so it cannot be opened. A write would \
                 corrupt this machine without reporting an error at the time. There is no \
                 override."
            }
            (Self::RunningSystem, false) => {
                "The disks below hold the running system, so they cannot be opened. A write to \
                 any of them would corrupt this machine without reporting an error at the time. \
                 There is no override."
            }
            // **Worded as an obstacle, not as a complete remedy.** A disk can
            // carry more than one of these, so "unmount it and open it again" is a
            // promise that fails for one that is also write-protected. Each says
            // what to clear, and clearing *this* one is not a claim that the disk
            // is then usable.
            (Self::ReadOnly, true) => {
                "The kernel marks the disk named below read-only, so it cannot be written. On \
                 removable media, this is usually the card's own write-protect switch. Reading \
                 it with dump, verify, or partitions is unaffected."
            }
            (Self::ReadOnly, false) => {
                "The kernel marks the disks named below read-only, so they cannot be written. On \
                 removable media, this is usually the card's own write-protect switch. Reading \
                 them with dump, verify, or partitions is unaffected."
            }
            (Self::Held, true) => {
                "The kernel is holding the disk named below, so the exclusive open will refuse \
                 it. Unmount it to clear this."
            }
            (Self::Held, false) => {
                "The kernel is holding the disks named below, so the exclusive open will refuse \
                 them. Unmount them to clear this."
            }
            // Never reached: an unrecognized refusal is drawn in core's own words.
            (Self::Unrecognized, _) => "",
        }
    }

    /// The word this group's rows already carry in their own column.
    ///
    /// The list is labeled with this word, so a person matches a group of names to
    /// its rows by a word instead of by position. Several groups on screen at once
    /// then stay readable. It is deliberately the same string the row draws, not a
    /// synonym. `running system`, `read-only` and `mounted at ...` are what the
    /// "What else has it" column says. The label is therefore the join key, not a
    /// second description of the same fact.
    fn column_word(self) -> &'static str {
        match self {
            Self::RunningSystem => "running system",
            Self::ReadOnly => "read-only",
            Self::Held => "mounted",
            Self::Unrecognized => "",
        }
    }
}

/// The disk list, drawn.
///
/// Each row shows the refusal it would meet, before anybody clicks. `Use` and
/// `Clone from` are still offered on a refused disk. The open makes the refusal,
/// in the kernel's words, and a button that vanishes explains nothing. The
/// exception is the running system, whose guard is pre-emptive because there is
/// nothing to catch after the fact.
///
/// A disk that refuses a write, such as a card with its lock switch on, also
/// keeps its buttons and opens for reading. The write is refused where the verbs
/// are drawn.
fn disk_rows(app: &mut App, ui: &mut egui::Ui) -> Vec<DiskRefusal> {
    // Taken and put back, for the reason `device_rows` does it: the rows need
    // `app` mutably and the listing is what they are drawn from.
    let disks = std::mem::take(&mut app.session.disks.listed);
    let mut refused: Vec<DiskRefusal> = Vec::new();

    // **A disk carries every obstacle somebody could act on -- unless one of them
    // is absolute, in which case it carries only that.**
    //
    // The two cases are different in kind. An obstacle that can be cleared costs a
    // round trip if it is shown alone: unmount, retry, discover the card is
    // write-protected, fix that, retry. An obstacle that *cannot* be cleared makes
    // the others irrelevant, and listing steps past an impassable point implies
    // the point is passable.
    //
    // Only the running system is absolute -- it is the one refusal with no
    // override anywhere in this codebase. `read-only` is not: on the media this
    // tool exists for it is usually the card's own write-protect switch, which is
    // a thing a person fixes, just not by clicking anything here. Suppressing a
    // held disk behind it was the round-trip case, and this is the fix.
    //
    // A held disk is a guard as much as a refused one: `O_EXCL` will turn it away.
    // It is not in `write_refusal`, so the grid's own refusal line does not cover
    // it, and the `mounted at` chip says the state without saying the consequence.
    for disk in disks.iter().filter(|disk| disk.bytes > 0) {
        if !disk.carries_running_system && disk.is_mounted() {
            refused.push(DiskRefusal {
                node: disk.node.clone(),
                kind: Refusal::Held,
                core_words: String::new(),
            });
        }
    }

    egui::Grid::new("disks")
        .num_columns(5)
        .striped(true)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            column_headers(ui, &["Device", "Size", "Bus", "What else has it", ""]);
            for disk in disks.iter().filter(|disk| disk.bytes > 0) {
                ui.monospace(&disk.node);
                ui.monospace(human_bytes(disk.bytes));
                ui.label(disk.bus.name());
                ui.horizontal(|ui| disk_label_note(disk, ui));

                ui.horizontal(|ui| {
                    let refusal = pyrographer_core::block::write_refusal(disk);
                    if let Some(why) = &refusal {
                        refused.push(DiskRefusal {
                            node: disk.node.clone(),
                            kind: Refusal::of(disk),
                            core_words: why.clone(),
                        });
                    }
                    // **Drawn in place of the buttons only where nothing at all
                    // can be done with the disk**, which is the running system and
                    // only the running system. That guard is pre-emptive on
                    // purpose: writing that disk succeeds, reads back correctly,
                    // and brings the machine down minutes later with nothing
                    // raised anywhere, so there is no moment at which a person
                    // could be shown a consequence and no override to offer them.
                    //
                    // Every other refusal here is a refusal of the *write*, and a
                    // disk that will not take a write still reads. A card with its
                    // lock switch on is the safest thing there is to image, and it
                    // is exactly what somebody reaches for this to do -- so it is
                    // opened for reading, and the write is grayed out where the
                    // verbs are, in the sentence `block::write_refusal` produced.
                    //
                    // **The one red thing on the row.** The reason beside it is
                    // drawn quiet (see `disk_label_note`), so the destructive
                    // color means exactly one thing in this list: you cannot use
                    // this. Eight red items across four rows is the dilution the
                    // palette is arranged to avoid, in words rather than in
                    // color.
                    //
                    // The reason rides the node as a description as well as the
                    // sentence below, because a screen reader on this row should
                    // not have to find the paragraph to learn why.
                    if disk.carries_running_system {
                        let why = refusal.unwrap_or_default();
                        ui.colored_label(ui.visuals().error_fg_color, "refused")
                            .explain(why);
                        return;
                    }
                    // **Named for the disk, not just for the verb.** A grid row
                    // is a layout concept and not a node -- `end_row` builds no
                    // container -- so nothing between this button and the whole
                    // grid can supply the context. Six buttons reading `Use` and
                    // `Clone from` leave a keyboard user with no way to tell which
                    // disk each one selects, on the control that picks a target
                    // for destructive work.
                    if ui
                        .add_enabled(app.can_select(Which::Target), egui::Button::new("Use"))
                        .named(format!("Use {}", disk.node))
                        .explain_disabled(
                            "The current target is opening or busy with a job. Wait for it to \
                             finish before choosing another.",
                        )
                        .clicked()
                    {
                        app.session.select_target_disk(disk.clone());
                    }
                    if ui
                        .add_enabled(
                            app.can_select(Which::Source),
                            egui::Button::new("Clone from"),
                        )
                        .named(format!("Clone from {}", disk.node))
                        .explain_disabled(
                            "The device to copy is opening or busy with a job. Wait for it to \
                             finish before choosing another.",
                        )
                        .clicked()
                    {
                        app.session.select_source_disk(disk.clone());
                    }
                });
                ui.end_row();
            }
        });

    app.session.disks.listed = disks;
    refused
}

/// What else has this disk, for a row in the list.
///
/// It is the note [`disk_label`] draws beside a chosen disk, without the size and
/// bus the row's own columns already show.
fn disk_label_note(disk: &BlockDevice, ui: &mut egui::Ui) {
    if disk.carries_running_system {
        // **A fact, drawn as one.** This says what the disk *is*; the `refused`
        // chip in the next column says what follows from it, and that one keeps
        // the destructive color. Both were red, which put eight red items on a
        // screen whose job is to help somebody find an SD card -- and a palette
        // where red is common is a palette where red stops being information.
        // Nothing is lost to anyone: the row still says it in words.
        ui.weak("running system").explain(
            "This disk holds the running system, directly or through a stack of device-mapper \
                 layers.",
        );
    } else if disk.is_mounted() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("mounted at {}", disk.mounts.join(", ")),
        )
        .explain("The kernel is holding this disk, so the exclusive open will refuse it.");
    } else if let Some(model) = &disk.model {
        ui.weak(model);
    }
    if disk.read_only {
        ui.weak("read-only");
    }
}

/// The device-acquisition seam, drawn in the web flasher.
///
/// A browser cannot scan a bus, so nothing here discovers a board. A browser can
/// remember one. A board picked from the browser's own chooser grants this origin
/// a permission that outlives the tab, and `getDevices` returns those boards
/// without a gesture. The page therefore draws two things: the boards already
/// granted, as rows to click, and the chooser, which is the only way to add
/// another.
///
/// The rows are the same [`device_rows`] the native window draws, less the
/// coordinate column, because a browser has no place to name.
///
/// **The list is not a way to confirm a write.** The page can draw a row like
/// this for any board. The write gate re-picks through the chooser instead,
/// because the chooser needs a gesture the page cannot manufacture. See
/// [`App::repick`](crate::app::App::repick).
#[cfg(target_arch = "wasm32")]
fn devices(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        section_label(ui, "Devices");
        // No poll behind this, unlike the native window's. `getDevices` is a
        // question with an answer, not a bus that can be watched, so the list is
        // taken at startup and whenever somebody asks -- here, or by granting a
        // new board.
        if ui
            .add_enabled(!app.is_listing(), egui::Button::new("Refresh"))
            .clicked()
        {
            app.relist();
        }
        if app.is_listing() {
            ui.spinner();
        }
    });

    prose(
        ui,
        "A page cannot scan a USB bus. These are the boards you have already allowed this page to \
         use. To add another, use the browser's own chooser, which only you can open.",
    );

    let free = !app.is_choosing() && app.session.job.is_none();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(free, egui::Button::new("Choose a board..."))
            .clicked()
        {
            app.choose(Which::Target);
        }
        if ui
            .add_enabled(free, egui::Button::new("Choose a board to copy..."))
            .clicked()
        {
            app.choose(Which::Source);
        }
        if app.is_choosing() {
            ui.spinner();
        }
    });

    ui.add_space(4.0);

    if app.session.devices.is_empty() {
        ui.weak("No board you have allowed is connected. Choose one to get started.");
        return;
    }

    device_rows(app, ui);
}

/// Column headers, in the quiet text color.
///
/// Neither `egui::Grid` nor a table builder gives an assistive technology any
/// table structure. A grid's nodes come out as `GenericContainer`, `Label` and
/// `TextRun`, with no table, row or cell role and no per-row container. This
/// module's tests verify that. A header row therefore serves a sighted reader
/// only, and the controls carry their own context for everyone else (see
/// [`Explain::named`]). The gap is real, and this is the half of it that layout
/// can close.
///
/// The headers are quiet, not strong, because a header is reference and the rows
/// it heads are the content. `text_mid` is a pairing the palette already
/// measures.
fn column_headers(ui: &mut egui::Ui, headers: &[&str]) {
    for header in headers {
        ui.weak(*header);
    }
    ui.end_row();
}

/// How a board is named on a control that acts on it.
///
/// When the board has a bus coordinate, the name is that coordinate, because a
/// person types it into the write gate. When it has none, as in a browser where
/// there is no place to name, the name is the vendor and product ids. See
/// [`state::coordinate`].
fn board_name(device: &DeviceInfo) -> String {
    let coordinate = state::usb_coordinate(device);
    if coordinate.is_empty() {
        format!("{:04x}:{:04x}", device.vendor_id, device.product_id)
    } else {
        coordinate
    }
}

/// A device's mode, and what pyrographer can do about it.
fn mode_label(device: &DeviceInfo, ui: &mut egui::Ui) {
    match device.mode {
        Mode::Loader => {
            ui.label(device.mode.name());
        }
        Mode::Maskrom => {
            ui.colored_label(ui.visuals().warn_fg_color, "maskrom")
                .explain(
                    "The bcdUSB flag reports that the BootROM is running. If it is, the flash \
                     cannot be reached until a loader is uploaded into SRAM. Some loaders present \
                     the same even flag, so Open probes the board to confirm the mode. Upload \
                     loader is for a board in maskrom.",
                );
        }
        Mode::BootRom => {
            ui.colored_label(ui.visuals().warn_fg_color, "boot ROM")
                .explain(
                    "This Ingenic device is running its USB boot ROM, which has no flash \
                     commands. A DRAM-init stage and a DFU-capable U-Boot must be uploaded \
                     before the flash can be reached.",
                );
        }
        Mode::Dfu => {
            ui.label(device.mode.name()).explain(
                "This Ingenic device is running a DFU-capable U-Boot. Flash is reachable as \
                 named DFU alt-settings.",
            );
        }
        Mode::MassStorage => {
            ui.colored_label(ui.visuals().warn_fg_color, "mass storage")
                .explain(
                    "This board re-enumerated as a USB mass-storage device, and the host \
                     operating system owns it as a disk. Open it from Disks.",
                );
        }
    }
}

/// What a disk is, beside its node path: its size, its bus, and what else has it.
///
/// The mounts matter most. The kernel refuses a held device with `O_EXCL` at the
/// open, which covers more than any check this code could make. That refusal
/// arrives after a person has already picked the disk. This note explains it
/// before then.
fn disk_label(disk: &BlockDevice, ui: &mut egui::Ui) {
    ui.label(human_bytes(disk.bytes));
    ui.label(disk.bus.name());
    if let Some(model) = &disk.model {
        ui.weak(model);
    }
    if disk.carries_running_system {
        ui.colored_label(ui.visuals().error_fg_color, "running system");
        // Drawn, not hovered. There is no override for this one, and a person is
        // owed the reason where they cannot miss it.
        guard(
            ui,
            "This disk holds the running system, directly or through a stack of device-mapper \
             layers. It cannot be opened, and there is no override. A write to it can pass its \
             read-back and still bring the machine down minutes later, with no error reported.",
        );
    } else if disk.is_mounted() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("mounted at {}", disk.mounts.join(", ")),
        );
        // **Worded as an obstacle, not as a complete remedy**, which is the
        // wording `Refusal::warning(Held)` already settled on and the reason it
        // did: a disk can carry more than one of these, so "unmount it and open it
        // again" is a promise that fails for one that is also write-protected.
        // The weaker sentence is the true one, and one fact should not be worded
        // two ways a hundred lines apart.
        guard(ui, Refusal::Held.warning(1));
    }
    if disk.read_only {
        ui.weak("read-only");
    }
}

/// One device, named the way every plan screen names it.
///
/// The write, clone and table plans all name the device about to be overwritten
/// through this function, so their wording cannot drift.
fn device_line(device: &Chosen) -> String {
    match device {
        Chosen::Usb(device) => format!(
            "{:04x}:{:04x}  {}  ({})",
            device.vendor_id,
            device.product_id,
            state::usb_coordinate(device),
            device.mode.name()
        ),
        Chosen::Block(disk) => format!(
            "{}  {}  ({})",
            disk.node,
            human_bytes(disk.bytes),
            disk.bus.name()
        ),
    }
}

/// One device: what it is, and whether it is open.
///
/// A disk is drawn here, not in a section of its own. StarFive recovery has a
/// section of its own, and the same reasoning gives the opposite answer for a
/// disk. A recovery target has no LBAs, no table and no geometry. Grayed `dump`
/// and `verify` buttons would offer it operations that do not apply. A
/// disk has the whole uniform surface and nothing to disable. It is therefore a
/// device in the slot, not a second window pane, and every verb is drawn once
/// for both.
fn board(app: &mut App, ui: &mut egui::Ui, which: Which) {
    let disk = app
        .board(which)
        .device
        .as_ref()
        .is_some_and(|device| device.disk().is_some());
    let title = match (which, disk) {
        (Which::Target, false) => "Board",
        (Which::Target, true) => "Disk",
        (Which::Source, false) => "Board to copy",
        (Which::Source, true) => "Disk to copy",
    };

    ui.group(|ui| {
        ui.horizontal(|ui| {
            section_label(ui, title);

            let Some(device) = app.board(which).device.clone() else {
                ui.weak("none chosen");
                return;
            };

            // The coordinate, where there is one: a board's place on the bus or a
            // disk's node path. A browser hands back a device object rather than a
            // place, so on the web there is nothing to show here -- and nothing to
            // transcribe, which is why the web's write confirmation is a re-pick
            // rather than a typed address.
            let coordinate = state::coordinate(&device);
            if !coordinate.is_empty() {
                ui.monospace(coordinate);
            }
            match &device {
                Chosen::Usb(device) => {
                    ui.label(format!(
                        "{:04x}:{:04x}",
                        device.vendor_id, device.product_id
                    ));
                    mode_label(device, ui);
                }
                Chosen::Block(disk) => disk_label(disk, ui),
            }

            connection_controls(app, ui, which);

            // **A slot that can be filled has to be emptiable.** The target's is
            // emptied by pointing it somewhere else, which is what every `Use`
            // button does; the source has no such traffic -- somebody sets it
            // once, clones, and is done -- so without this the panel and the
            // board it holds open stay for the rest of the session.
            if which == Which::Source
                && ui
                    .add_enabled(app.can_select(Which::Source), egui::Button::new("Forget"))
                    .named("Forget the device to copy")
                    .explain("Stop copying from this device, and close it.")
                    .explain_disabled(
                        "The device to copy is opening or busy with a job. Wait for it to \
                         finish.",
                    )
                    .clicked()
            {
                app.forget_source();
            }
        });

        // Said while the disk is open, because it is the difference between this
        // target and every other one: the kernel is holding it for us, and
        // nothing else on the machine can have it back until this slot is closed.
        // The CLI never has to say this -- it holds the device for the length of
        // one command -- and a window holds it for as long as somebody leaves it
        // open.
        if disk && app.board(which).connection.is_idle() {
            ui.weak(
                "Held exclusively (O_EXCL) while it is open, so nothing else on this machine \
                 can mount or write it. Close it to release it.",
            );
        }

        if let Some(flash) = &app.board(which).flash {
            ui.label(geometry(flash));
        }
        // A disk runs no loader, so there is nothing here that could have
        // answered -- and a row that is reliably empty is a row somebody learns
        // to skip.
        if let Some(version) = &app.board(which).chip_version {
            ui.horizontal(|ui| {
                ui.label("Loader says");
                ui.monospace(hex(version));
                ui.monospace(format!("\"{}\"", ascii(version)));
            })
            .response
            .explain(
                "The loader's own answer about which SoC it runs on, shown raw. The reply's \
                 layout is unverified, so pyrographer shows it without interpreting it.",
            );
        }
        if let Table::Read(table) = &app.board(which).table {
            partitions(
                table,
                app.board(which).sector_size(),
                app.board(which).flash.as_ref(),
                ui,
            );
        }
        if matches!(app.board(which).table, Table::Absent) {
            ui.weak("This board has no partition table.");
        }

        // A maskrom board being sent bare stages: the reveal-on-demand form, drawn
        // under the board it acts on so its trigger and its fields are one reading
        // order -- the same shape the Ingenic bootstrap form below has.
        if which == Which::Target
            && app.maskrom.show
            && app
                .board(which)
                .device
                .as_ref()
                .and_then(Chosen::usb)
                .is_some_and(|device| device.mode == Mode::Maskrom)
        {
            maskrom_stage_form(app, ui);
        }

        // A boot-ROM board being bootstrapped to DFU: the reveal-on-demand form,
        // drawn under the board it acts on so its trigger and its fields are one
        // reading order.
        if which == Which::Target
            && app.ingenic.show
            && app
                .board(which)
                .device
                .as_ref()
                .and_then(Chosen::usb)
                .is_some_and(|device| device.mode == Mode::BootRom)
        {
            ingenic_bootstrap_form(app, ui);
        }
    });
}

/// Open, close, and what state the connection is in.
fn connection_controls(app: &mut App, ui: &mut egui::Ui, which: Which) {
    // A board flagged maskrom may be exactly that, or a loader that keeps the
    // flag even, as the RK3576 SPL does -- so the target offers both doors.
    // Upload loader is for the BootROM the flag may truthfully name; the Open
    // below probes the claim, because a running loader answers TEST_UNIT_READY
    // and a BootROM's endpoints -- the same pair a loader presents -- have
    // nothing behind them and fault. The upload is offered only for the target
    // board: a clone cannot copy from a board that cannot yet be read.
    let is_maskrom = app
        .board(which)
        .device
        .as_ref()
        .and_then(Chosen::usb)
        .is_some_and(|device| device.mode == Mode::Maskrom);
    if which == Which::Target && is_maskrom {
        if app.is_bootstrapping() {
            ui.spinner();
            ui.weak("uploading loader");
            return;
        }
        if ui
            .add_enabled(
                app.session.job.is_none(),
                egui::Button::new("Upload loader..."),
            )
            .explain(
                "Upload a loader into SRAM to bring this board from maskrom to loader mode. \
                 Every verb can then drive it. This button takes an rkbin container \
                 (_loader.bin), which names its own sections.",
            )
            .clicked()
        {
            app.upload_loader();
        }
        // **The other form of the same upload**, and the one the RAM-boot path is
        // built on: mainline U-Boot's binman emits bare 471 and 472 blobs with no
        // container around them, which the picker above parses and refuses. It is
        // a form rather than a button because it takes two files.
        let label = if app.maskrom.show {
            "Hide raw stages"
        } else {
            "Raw stages..."
        };
        if ui
            .add_enabled(app.session.job.is_none(), egui::Button::new(label))
            .explain(
                "Upload bare usb471/usb472 stage files instead of a container. Mainline U-Boot \
                 builds these, and they load a full U-Boot into DRAM over USB.",
            )
            .clicked()
        {
            app.maskrom.show = !app.maskrom.show;
        }
        // No return: the open controls below stay on offer, because the flag
        // may belong to a loader that never set it.
    }

    // An Ingenic boot ROM has no reachable flash and cannot be opened as a
    // FlashAgent -- it must be bootstrapped to DFU first, unlike the maskrom flag
    // above whose board might already be a loader. So the target offers the
    // bootstrap form in place of Open; the block verbs come after the board
    // re-enumerates as a DFU gadget.
    {
        let is_bootrom = app
            .board(which)
            .device
            .as_ref()
            .and_then(Chosen::usb)
            .is_some_and(|device| device.mode == Mode::BootRom);
        if which == Which::Target && is_bootrom {
            if app.is_bootstrapping_ingenic() {
                ui.spinner();
                ui.weak("bootstrapping");
                return;
            }
            let label = if app.ingenic.show {
                "Hide bootstrap"
            } else {
                "Bootstrap to DFU..."
            };
            if ui
                .add_enabled(app.session.job.is_none(), egui::Button::new(label))
                .explain(
                    "Upload a DRAM-init SPL and a DFU-capable U-Boot to bring this board from \
                     its boot ROM to DFU mode. Its flash is then reachable as named \
                     alt-settings.",
                )
                .clicked()
            {
                app.ingenic.show = !app.ingenic.show;
            }
            // No Open: a boot-ROM board has no flash to open until it is DFU.
            return;
        }
    }

    if app.is_opening(which) {
        ui.spinner();
        ui.weak("opening");
        return;
    }

    let connection = &app.board(which).connection;
    if connection.is_busy() {
        ui.weak("busy");
        return;
    }

    if connection.is_desynchronized() {
        ui.colored_label(ui.visuals().error_fg_color, "out of step");
        if ui.button("Reopen").clicked() {
            app.close(which);
            app.open(which);
        }
        return;
    }

    if connection.is_idle() {
        ui.colored_label(ui.visuals().hyperlink_color, "open");
        if ui.button("Close").clicked() {
            app.close(which);
        }
        return;
    }

    if ui.button("Open").clicked() {
        app.open(which);
    }
}

/// The verbs, and which of them this board can perform.
fn verbs_panel(app: &mut App, ui: &mut egui::Ui) {
    let now = app.now();
    // `!is_picking()` as well as idle: a file dialog is off-frame, and a verb pressed
    // while one is up would land a job (or a plan screen) that then runs invisibly
    // behind the dialog's own outcome -- including a dump whose file was already
    // created at pick time. So every verb is disabled until the dialog closes.
    let idle =
        app.session.target.connection.is_idle() && app.session.job.is_none() && !app.is_picking();

    ui.group(|ui| {
        // A finished connection draws the panel disabled with the reason, rather
        // than hiding it. Every button below already grays itself out on a
        // not-idle connection; this says why.
        if app.session.target.connection.is_desynchronized() {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "This board is no longer synchronized with the host, and its connection refuses \
                 further commands. Close it and open it again.",
            );
            ui.add_space(6.0);
        }

        // The two capability facts this panel refuses on, read once: a disk runs
        // no vendor protocol, and no backend has an erase opcode that can be
        // trusted. Both are properties of the device in the slot rather than of
        // the form, and both are drawn as sentences below as well as graying a
        // button, so hoist them out of the closure that draws the buttons.
        let target_is_disk = app.target_is_disk();
        let can_erase = app
            .session
            .target
            .caps
            .as_ref()
            .is_some_and(|caps| caps.can_erase);

        section_label(ui, "Read");
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(idle, egui::Button::new("Flash info"))
                .clicked()
            {
                app.run(Task::Info, now);
            }
            if ui
                .add_enabled(idle, egui::Button::new("Partition table"))
                .clicked()
            {
                app.run(Task::Partitions, now);
            }
            // **The vendor verbs, drawn and disabled on a disk.** `chipver` and
            // `reset` speak a vendor protocol to a board, and a disk runs none --
            // the CLI refuses them by name in one place rather than letting five
            // commands each produce an empty answer, and this is the same refusal
            // where a person can see it. Chip version is the one that most needs
            // saying: core answers a block agent with no bytes rather than an
            // error, and an empty answer read as a fact about the device is worse
            // than a grayed button that explains itself.
            let vendor = idle && !target_is_disk;
            if ui
                .add_enabled(vendor, egui::Button::new("Chip version"))
                .explain_disabled(VENDOR_VERB_ON_A_DISK)
                .clicked()
            {
                app.run(Task::ChipVersion, now);
            }
            // **The device's own account of itself**, as against `Caps`, which is
            // pyrographer's account of what the backend implements. Both are worth
            // having and they answer different questions -- a loader that sets a
            // bit pyrographer has no name for is a finding, and it is one only the
            // device can report.
            if ui
                .add_enabled(vendor, egui::Button::new("Capability"))
                .explain_disabled(VENDOR_VERB_ON_A_DISK)
                .explain(
                    "The running loader's own report of what it supports. Nothing is gated on \
                     the answer.",
                )
                .clicked()
            {
                app.run(Task::Capability, now);
            }
            // Which medium the LBAs currently address. It is a safety fact about
            // every aim on this surface: on a board with more than one populated,
            // the same sector number names more than one place.
            if ui
                .add_enabled(vendor, egui::Button::new("Storage medium"))
                .explain_disabled(VENDOR_VERB_ON_A_DISK)
                .explain(
                    "The medium that the sector numbers on this screen are offsets into. \
                     pyrographer reports which medium is active and never switches it.",
                )
                .clicked()
            {
                app.run(Task::StorageMedium, now);
            }
            // Reset is a *mode*, not one act: the subcode chooses whether the
            // board comes back, comes back as something the host operating system
            // owns, or does not come back at all. So the button says which ending
            // it will ask for rather than saying "Reboot" and doing one of four
            // things, and the choice sits beside it rather than inside it -- a
            // person should not have to open a menu to find out that this one
            // powers the board off.
            let mode = app.form.reset_mode;
            if ui
                .add_enabled(vendor, egui::Button::new(mode.describe()))
                .explain_disabled(VENDOR_VERB_ON_A_DISK)
                .clicked()
            {
                app.run(Task::Reset { mode }, now);
            }
            ui.add_enabled_ui(vendor, |ui| {
                egui::ComboBox::from_label("Mode")
                    .selected_text(mode.name())
                    .show_ui(ui, |ui| {
                        for candidate in ResetMode::ALL {
                            ui.selectable_value(
                                &mut app.form.reset_mode,
                                candidate,
                                candidate.describe(),
                            );
                        }
                    });
            });

            // **Drawn and disabled, and never hidden.** The button is here to be
            // grayed out: a person who wants to erase a board finds out why it
            // is refused, and a missing button tells them nothing. Both the whether and the why come from the cache, not the
            // live agent, so a job holding the agent does not blank them out.
            let erase = ui.add_enabled(can_erase, egui::Button::new("Erase"));
            if let Some(why) = app.session.target.erase_reason {
                erase.explain_disabled(why);
            }
        });

        // **The two capability refusals, on the screen rather than behind a
        // hover.** A tooltip needs a pointer, so a keyboard user never reads one
        // and a screen reader is told only that the button is disabled -- which
        // is the grayed button teaching nothing, the thing this panel exists not
        // to do. Both are permanent facts about the device in the slot, not
        // prompts about the state of the form, so they are drawn while they hold
        // and vanish when they stop holding.
        if target_is_disk {
            ui.add_space(4.0);
            guard(
                ui,
                format!("Chip version and reset are refused. {VENDOR_VERB_ON_A_DISK}"),
            );
        }
        // The refusal names itself -- "rockusb erase:", "block erase:" -- so it is
        // drawn as core wrote it rather than introduced again here.
        if !can_erase && let Some(why) = app.session.target.erase_reason {
            ui.add_space(4.0);
            guard(ui, why);
        }

        // On the screen, not behind a hover. The three modes that are not a plain
        // reboot end the session somewhere none of these verbs can reach, and one
        // of them powers the board off -- a person choosing it should read what it
        // does without having to go looking. The sentence is the mode's own, the
        // same one the report will print afterwards, so what was promised and what
        // is reported cannot drift.
        if app.form.reset_mode.untried() {
            ui.add_space(4.0);
            ui.label(format!(
                "{} No board has answered this mode yet. Its subcode comes from the reference \
                 tools, which agree on it. It writes no flash.",
                app.form.reset_mode.outcome()
            ));
        }

        ui.add_space(6.0);
        ui.separator();
        aim(app, ui);

        ui.add_space(6.0);
        image_row(app, ui);

        ui.add_space(6.0);
        ui.separator();
        section_label(ui, "Act");
        actions(app, ui, idle, now);

        ui.add_space(6.0);
        table_tools(app, ui, idle, now);
    });
}

/// Where a verb is aimed: a name the device gave, or a number a person worked out.
///
/// The two are not equally safe, and the form says so instead of presenting them
/// as a preference. A name is resolved against the device's own table, which
/// knows where the partition ends. A name is therefore the only one of the two
/// that can refuse an image too big to fit.
fn aim(app: &mut App, ui: &mut egui::Ui) {
    // A board that reaches its flash only by named region -- a DFU board, addressed
    // by alt-setting rather than a device-wide LBA -- cannot be aimed by a raw LBA,
    // so that form is disabled and the aim stays by-partition. Unknown caps (no
    // board open) default to allowing it.
    let raw_lba = app
        .session
        .target
        .caps
        .as_ref()
        .is_none_or(|caps| caps.can_address_raw_lba);
    if !raw_lba {
        app.form.by_name = true;
    }

    ui.horizontal(|ui| {
        ui.radio_value(&mut app.form.by_name, true, "By partition")
            .explain(
                "Resolved against the device's own partition table. On a write, only this form \
                 knows where the partition ends, so only this form can refuse an image too large \
                 to fit.",
            );
        ui.add_enabled_ui(raw_lba, |ui| {
            ui.radio_value(&mut app.form.by_name, false, "By LBA")
                .explain(
                    "A sector number. pyrographer cannot check it, because a raw LBA does not \
                     say where a partition ends.",
                )
                .explain_disabled(
                    "This board reaches its flash by named region (its DFU alt-settings), not a \
                     device-wide LBA. Aim it by partition.",
                );
        });
    });

    // Why the choice above is not a choice on this board. A capability of the
    // device rather than a state of the form, so it is a sentence and not a
    // tooltip on a radio nothing can focus.
    if !raw_lba {
        guard(
            ui,
            "This board reaches its flash by named region (its DFU alt-settings), not a \
             device-wide LBA, so it can only be aimed by partition.",
        );
    }

    if app.form.by_name {
        let names: Vec<String> = app
            .session
            .target
            .table
            .get()
            .map(|table| table.names())
            .unwrap_or_default();

        if names.is_empty() {
            ui.weak("Read the partition table first, and the partitions appear here.");
            return;
        }

        let selected = if app.form.partition.is_empty() {
            "-".to_string()
        } else {
            app.form.partition.clone()
        };
        egui::ComboBox::from_label("Partition")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for name in names {
                    ui.selectable_value(&mut app.form.partition, name.clone(), name);
                }
            });
        return;
    }

    ui.horizontal(|ui| {
        named_field(ui, "LBA", |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.form.lba).desired_width(120.0))
        });
        named_field(ui, "Sectors", |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.form.sectors).desired_width(120.0))
        })
        .explain("How many sectors a dump reads. A write takes its length from the image.");
    });
}

/// The image a write writes, and a verify compares against.
fn image_row(app: &mut App, ui: &mut egui::Ui) {
    let chosen = app
        .session
        .image
        .as_ref()
        .map(|image| (image.name.clone(), image.bytes));

    let pressed = file_row(
        ui,
        "Image",
        "an image file",
        chosen,
        app.is_picking(),
        "none chosen",
    );
    if pressed.choose {
        app.pick_image();
    }
    if pressed.forget {
        app.forget_image();
    }
}

/// The refusal a vendor verb gives for a disk.
///
/// One sentence in one place, so `chipver` and `reset` refuse identically. The CLI
/// follows the same rule. It puts the refusal for all five vendor commands in a
/// single function, instead of letting each produce an empty answer.
const VENDOR_VERB_ON_A_DISK: &str = "A vendor protocol command needs a board to answer it. The selected device is a disk the \
     operating system owns. There is no loader on it to ask.";

/// The buttons that read a board, and the ones that overwrite one.
fn actions(app: &mut App, ui: &mut egui::Ui, idle: bool, now: f64) {
    let aimed = app.aim();
    let image_bytes = app.session.image.as_ref().map(|image| image.bytes);

    // The SoC the wrong-loader gate compares the loader's answer against.
    // Collected with the rest of the form, because it is half of what a write
    // plan is made of -- the loader's own answer is the other half.
    //
    // **Not drawn for a disk, and not drawn grayed either.** A disk runs no
    // loader, so this gate has nothing to be about; the CLI refuses `--soc` on a
    // block write rather than ignoring it, and the window's form of that is not
    // to ask. What stands in its place is not nothing -- it is the guard that is
    // actually there, said in the same spot a person is used to reading a guard.
    if app.target_is_disk() {
        ui.horizontal(|ui| {
            ui.label("Guarded by");
            ui.weak(
                "an exclusive open, on a disk that does not hold the running system. A disk runs \
                 no loader, so no SoC is asked for.",
            )
            .explain(
                "A disk runs no loader, so the wrong-loader gate does not apply. Two checks \
                     guard a write to a disk instead. The kernel holds the disk for pyrographer \
                     alone (O_EXCL), and no disk that holds the running system can be opened. \
                     Both are checked when the disk is opened.",
            );
        });
    } else {
        ui.horizontal(|ui| {
            named_field(ui, "SoC", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.form.soc)
                        .desired_width(80.0)
                        .hint_text("rk3576"),
                )
            })
            .explain(
                "The SoC on this board. A write runs only if the loader's own answer matches the \
                 reply pinned for this SoC, byte for byte. If this field is empty, a write is \
                 planned but refused.",
            );
        });
    }
    ui.add_space(4.0);

    // Whether a clone can address both boards by a device-wide LBA at all. Read
    // before the buttons, because it grays one of them and is also said as a
    // sentence after them. The source is asked as well as the target: a clone
    // reads the whole of one as surely as it writes the whole of the other.
    let raw_lba = |board: &state::Board<_>| {
        board
            .caps
            .as_ref()
            .is_none_or(|caps| caps.can_address_raw_lba)
    };
    let clone_refusal = if !raw_lba(&app.session.target) {
        Some(
            "This board reaches its flash by named region (its DFU alt-settings), so there is \
             no whole-device image to clone onto it. Copy a partition with a dump and a write \
             instead.",
        )
    } else if !raw_lba(&app.session.source) {
        Some(
            "The board chosen to clone from reaches its flash by named region (its DFU \
             alt-settings), so there is no whole-device image to copy. Copy a partition with a \
             dump and a write instead.",
        )
    } else {
        None
    };

    ui.horizontal_wrapped(|ui| {
        // A dump needs somewhere to go, and asking for it is asking for the
        // dump: the file dialog is the last thing between the button and the
        // read.
        let can_dump = idle && !app.is_picking() && dump_range(app).is_some();
        let dump = ui
            .add_enabled(can_dump, egui::Button::new("Dump to file..."))
            .explain_disabled("Choose a partition, or enter an LBA and a sector count.");
        if dump.clicked()
            && let Some((lba, sectors)) = dump_range(app)
        {
            app.dump(lba, sectors, suggested_name(app));
        }

        let can_verify = idle && aimed.is_some() && image_bytes.is_some();
        let verify = ui
            .add_enabled(can_verify, egui::Button::new("Verify against image"))
            .explain_disabled(
                "Choose a partition or enter an LBA, then choose an image to compare the flash \
                 against.",
            );
        if verify.clicked() {
            start_verify(app, now);
        }

        // The write. It is a plan first, always -- the plan is the dry run and
        // there is no other one, because a dry run that took a different path
        // would be a rehearsal for a different show.
        let can_plan = idle && aimed.is_some() && image_bytes.is_some();
        let plan = ui
            .add_enabled(can_plan, egui::Button::new("Plan a write..."))
            .explain_disabled("Choose a partition or enter an LBA, then choose an image to write.");
        if plan.clicked()
            && let (Some(aim), Some(image_bytes)) = (aimed.clone(), image_bytes)
        {
            let soc = app.planned_soc();
            app.run(
                Task::PlanWrite {
                    aim,
                    image_bytes,
                    soc,
                },
                now,
            );
        }

        // A clone names both boards and infers neither, because the difference
        // between the two is which one gets destroyed. A clone also spans a whole
        // device by raw LBA at both ends, so a board that reaches its flash only by
        // named region (a DFU board) can be neither copied nor cloned onto.
        let can_clone = idle && app.session.source.connection.is_idle() && clone_refusal.is_none();
        let clone_hover = clone_refusal.unwrap_or(
            "A clone needs both boards open: the one it copies, and the one it overwrites. Neither \
             is inferred.",
        );
        let clone = ui
            .add_enabled(can_clone, egui::Button::new("Plan a clone..."))
            .explain_disabled(clone_hover);
        if clone.clicked() {
            let soc = app.planned_soc();
            app.run(Task::PlanClone { soc }, now);
        }
    });

    // A clone these boards cannot take part in at all, as against a clone that is
    // merely not set up yet: the first is a capability and is drawn, the second is
    // form state and stays on the grayed button.
    if let Some(why) = clone_refusal {
        ui.add_space(4.0);
        guard(ui, format!("A clone is refused. {why}"));
    }

    // Said here, before anybody plans anything, and not sprung on them at the
    // confirmation. Planning still works and is still worth doing -- the plan
    // is the dry run, and it carries the gate's verdict either way.
    match app.gate_soc() {
        Err(error) => {
            ui.add_space(4.0);
            ui.colored_label(
                ui.visuals().error_fg_color,
                "The SoC on the form is not one the gate knows.",
            );
            ui.weak(format!("{error}"));
        }
        Ok(soc) => {
            if let Some(why) = app.session.target.write_refusal(soc) {
                ui.add_space(4.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Writes are refused on this board.",
                );
                ui.weak(why);
            }
        }
    }
}

/// The partition table, as capabilities of an open board.
///
/// Repairing a damaged table and authoring a fresh one are writes a block backend
/// can perform once it is open. They are neither a flow of their own nor a device
/// class. They therefore belong in the verb surface, not behind a screen or a tab.
/// Both are gated exactly like a boot-image write, with a plan first and a
/// read-back after. Both reach the one generic table plan screen.
///
/// A table sits at fixed sectors of a device-wide LBA space. A board without one, a
/// DFU board, is refused every table plan by core. The section's buttons gray on
/// that same answer, cached at open, and the refusal is drawn as a sentence.
fn table_tools(app: &mut App, ui: &mut egui::Ui, idle: bool, now: f64) {
    ui.separator();

    // **Shut until it is asked for**, alone among the verb sections. Repairing a
    // damaged table and authoring a fresh one are what somebody comes here for on
    // a board that is already wrong; they are not part of reading or writing one
    // that is fine, and open they pushed the buttons somebody did come for off the
    // screen. The arrow and the words are the whole control, the way the Disks
    // section's are, so what an assistive technology is told is what is drawn.
    ui.horizontal(|ui| {
        let arrow = if app.show_table_tools { "v" } else { ">" };
        if ui
            .button(format!("{arrow}  Partition table"))
            .explain(
                "Repair a damaged partition table from an intact copy, or author a fresh one. \
                 Both are gated writes that end at a plan screen. Nothing here writes without \
                 your confirmation.",
            )
            .clicked()
        {
            app.show_table_tools = !app.show_table_tools;
        }
    });
    if !app.show_table_tools {
        return;
    }
    ui.add_space(4.0);

    // **On the screen, not behind a hover**, for the reason the capability
    // refusals in the verb panel are: it is a permanent fact about the device in
    // the slot, and a grayed button with its reason in a tooltip teaches a
    // keyboard user nothing.
    let refused = table_refusal(app);
    if let Some(why) = &refused {
        guard(ui, why.clone());
        ui.add_space(4.0);
    }
    let can_table = idle && refused.is_none();
    let disabled_why = refused.unwrap_or_else(|| TABLE_TOOLS_BUSY.to_string());

    // The two repairs: one click each, and safe to offer always. Each plans first,
    // and if that format is absent or its copies all check out, core refuses with a
    // reason rather than writing anything -- so both are shown, and the board's own
    // answer is what tells them apart. Two buttons, not one that routes by the table
    // already read, because the case a repair is for is the table too damaged to name
    // its own format.
    ui.horizontal_wrapped(|ui| {
        ui.label("Repair");

        let gpt = ui
            .add_enabled(can_table, egui::Button::new("Repair GPT..."))
            .explain_disabled(disabled_why.clone())
            .explain(
                "Rewrite a damaged GPT copy from the intact one: the primary from the backup in \
                 the last sector, or the backup from the primary. This is gated like any write, \
                 so name the SoC above.",
            );
        if gpt.clicked() {
            let soc = app.planned_soc();
            app.run(Task::PlanRepairTable { soc }, now);
        }

        let param = ui
            .add_enabled(can_table, egui::Button::new("Repair parameter..."))
            .explain_disabled(disabled_why.clone())
            .explain(
                "Rewrite a damaged Rockchip parameter copy from an intact one. Raw NAND keeps \
                 several copies. An eMMC keeps one, so a damaged copy there has no other copy to \
                 be rebuilt from. Author a fresh table instead. This is gated like any write.",
            );
        if param.clicked() {
            let soc = app.planned_soc();
            app.run(Task::PlanRepairParam { soc }, now);
        }
    });

    ui.add_space(4.0);
    author_tools(app, ui, idle, now);
}

/// What a grayed table tool says when the device is open but cannot take a command.
///
/// The verb panel is drawn only once a device is open, so an unopened device is
/// never the reason. A running job, an open file dialog and a connection out of
/// step are.
const TABLE_TOOLS_BUSY: &str = "Unavailable while a job runs or a file dialog is open, or after the connection falls out \
     of step.";

/// Why the board in the target slot cannot have its table repaired or authored, as
/// a sentence for the screen, or `None`.
///
/// It is core's [`raw_lba_refusal`](pyrographer_core::verbs::raw_lba_refusal),
/// cached when the board was opened. Every table plan refuses on the same answer.
fn table_refusal(app: &App) -> Option<String> {
    app.session.target.raw_lba_reason.map(|why| {
        format!(
            "Repair and authoring are refused: {why}. A partition table sits at fixed sectors \
             of a device-wide LBA space."
        )
    })
}

/// The "Author a fresh table" form: config-heavy, so revealed on demand.
///
/// It is a single button until a person asks for it, the same "declared, not
/// dumped" shape the serial recovery form uses. Once open, it is a small form with
/// these fields:
///
/// - The format
/// - Where the layout comes from
/// - The file itself
/// - The medium the copies go to, for a parameter table only
///
/// Authoring is a gated write like any other, so it ends at the same plan screen
/// the writes and repairs do. Nothing here writes.
fn author_tools(app: &mut App, ui: &mut egui::Ui, idle: bool, now: f64) {
    let refused = table_refusal(app);
    let can_table = idle && refused.is_none();
    if !app.author.show {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(can_table, egui::Button::new("Author a fresh table..."))
                .explain_disabled(refused.unwrap_or_else(|| TABLE_TOOLS_BUSY.to_string()))
                .clicked()
            {
                app.author.show = true;
            }
        });
        return;
    }

    ui.group(|ui| {
        ui.horizontal(|ui| {
            section_label(ui, "Author a fresh table");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Hide")
                    .named("Hide the table authoring form")
                    .clicked()
                {
                    app.author.show = false;
                }
            });
        });
        prose(
            ui,
            "Write a fresh table from a partition layout. This overwrites the table. It is \
             planned and gated like any write, and read back window by window.",
        );

        ui.horizontal(|ui| {
            ui.label("Format");
            ui.radio_value(&mut app.author.format, TableFormat::Gpt, "GPT");
            ui.radio_value(
                &mut app.author.format,
                TableFormat::RockchipParam,
                "Rockchip parameter",
            );
        });

        // A GPT is not built from parameter text, so if the format is switched to
        // GPT while the verbatim-text source was chosen, fall back to a layout file.
        if app.author.format == TableFormat::Gpt && app.author.source == AuthorSourceKind::Text {
            app.author.source = AuthorSourceKind::Native;
        }

        ui.horizontal(|ui| {
            ui.label("From");
            ui.radio_value(
                &mut app.author.source,
                AuthorSourceKind::Native,
                "native layout",
            )
            .explain(
                "A native-format layout: name, first LBA, sectors, and an optional type and \
                     uuid= per line.",
            );
            ui.radio_value(
                &mut app.author.source,
                AuthorSourceKind::Mtdparts,
                "mtdparts line",
            )
            .explain("A file holding a board's own mtdparts= line.");
            if app.author.format == TableFormat::RockchipParam {
                ui.radio_value(
                    &mut app.author.source,
                    AuthorSourceKind::Text,
                    "parameter text",
                )
                .explain(
                    "An existing parameter block's whole text, framed verbatim, so \
                         FIRMWARE_VER and every other key are kept.",
                );
            }
        });

        let chosen = app
            .author
            .file
            .as_ref()
            .map(|file| (file.name.clone(), file.bytes.len() as u64));
        let pressed = file_row(
            ui,
            "File",
            "a partition layout file",
            chosen,
            app.is_picking(),
            "none chosen",
        );
        if pressed.choose {
            app.pick_layout_file();
        }
        if pressed.forget {
            app.author.file = None;
        }

        // The medium is a parameter fact: it decides where the copies go, and what an
        // mtdparts layout's offsets count from. A GPT is absolute and takes none.
        if app.author.format == TableFormat::RockchipParam {
            ui.horizontal(|ui| {
                ui.label("Medium");
                ui.radio_value(&mut app.author.medium, ParamMedium::Emmc, "eMMC")
                    .explain(
                        "One parameter copy, at sector 0x2000. A layout's offsets count from that \
                         sector.",
                    );
                ui.radio_value(&mut app.author.medium, ParamMedium::Nand, "raw NAND")
                    .explain(
                        "Several copies, written from sector 0. A layout's offsets count from \
                         sector 0.",
                    );
            });
        }

        ui.add_space(4.0);
        let can_author = can_table && app.author.file.is_some() && !app.is_picking();
        let plan = ui
            .add_enabled(can_author, egui::Button::new("Author table..."))
            .explain_disabled(match refused {
                Some(why) => why,
                None if !idle => TABLE_TOOLS_BUSY.to_string(),
                None => "Choose a layout file. Authoring writes flash and is gated like any \
                         write, so on a board, name the SoC above."
                    .to_string(),
            });
        if plan.clicked() {
            app.plan_author(now);
        }
    });
}

/// The range a dump reads: the partition's own extent, or the numbers on the
/// form.
fn dump_range(app: &App) -> Option<(u64, u64)> {
    match app.aim()? {
        Aim::Partition(name) => {
            let partition = app.session.target.table.get()?.find(&name).ok()?;
            Some((partition.first_lba, partition.sectors))
        }
        Aim::Lba(lba) => {
            let sectors: u64 = app.form.sectors.trim().parse().ok()?;
            (sectors > 0).then_some((lba, sectors))
        }
    }
}

/// The suggested file name for a dump, before a person renames it.
fn suggested_name(app: &App) -> String {
    match app.aim() {
        Some(Aim::Partition(name)) => format!("{name}.img"),
        _ => "dump.img".to_string(),
    }
}

/// Compare the flash against the image.
fn start_verify(app: &mut App, now: f64) {
    let Some(aim) = app.aim() else { return };
    let Some(image) = app.session.image.as_ref() else {
        return;
    };
    let image_bytes = image.bytes;

    let reader = match image.reader() {
        Ok(reader) => reader,
        Err(error) => {
            app.session.last = Some(Err(error));
            return;
        }
    };

    // A partition is resolved here rather than in core, because `verify` takes an
    // LBA. An image bigger than the partition it names cannot be what is in that
    // partition, and saying so beats comparing on into the next one and reporting
    // a mismatch at whatever byte the two first happen to differ at.
    let lba = match aim {
        Aim::Lba(lba) => lba,
        Aim::Partition(name) => match resolve(app, &name, image_bytes) {
            Ok(lba) => lba,
            Err(error) => {
                app.session.last = Some(Err(error));
                return;
            }
        },
    };

    app.run(
        Task::Verify {
            lba,
            image: reader,
            image_bytes,
        },
        now,
    );
}

/// The first sector of the partition called `name`, refusing an image too big
/// for it.
fn resolve(app: &App, name: &str, image_bytes: u64) -> Result<u64, Error> {
    let board = &app.session.target;

    let (Some(table), Some(sector_size)) = (board.table.get(), board.sector_size()) else {
        return Err(Error::InvalidRequest(
            "this board's partition table has not been read, so there is no name to resolve"
                .to_string(),
        ));
    };

    let partition = table.find(name)?;
    partition.must_hold(image_bytes, sector_size)?;
    Ok(partition.first_lba)
}

/// A job in flight, drawn by one function for every job that moves bytes.
///
/// Four flows put a panel on the screen:
///
/// - A verb
/// - A maskrom upload
/// - An Ingenic bootstrap
/// - A serial recovery
///
/// All four draw the same panel, with a spinner, a name, a Cancel, a bar and a
/// sentence. Only the name, the boundary a cancel stops at, and the sentence
/// differ. [`Measured`] makes that sharing hold in the types as well as on the
/// screen. The progress arithmetic reads core's [`Progress`] the same way,
/// whichever job published it.
///
/// Nothing here is drawn once the job has finished. The panel shows work in
/// flight. What the job produced is the report, which is drawn whichever tab is
/// up.
fn progress_panel(ui: &mut egui::Ui, now: f64, job: &impl Measured, what: &Panel<'_>) {
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.strong(what.title);

            // Canceling is not an undo. What a write has already put on the
            // flash stays there; stopping at a boundary only means the device is
            // never left partway through a command.
            if ui
                .button("Cancel")
                .named(format!("Cancel {}", what.title))
                .clicked()
            {
                job.cancel();
            }
            if job.is_canceling() {
                ui.weak(format!("stopping at the next {}...", what.boundary));
            }
        });

        // Above the bar and outside the early return below, because these are
        // true before a byte moves and stay true after the last one: a file's own
        // claim about itself is not progress.
        for note in what.notes {
            ui.weak(note);
        }

        let Some(progress) = job.progress() else {
            return;
        };
        if matches!(progress, Progress::Finished { .. }) {
            return;
        }

        if let Some(fraction) = job.fraction() {
            let done = job.done_bytes();
            let total = job.total_bytes().unwrap_or(0);
            let text = match job.rate(now) {
                Some(rate) => format!(
                    "{} of {}  ({}/s)",
                    human_bytes(done),
                    human_bytes(total),
                    human_bytes(rate as u64)
                ),
                None => format!("{} of {}", human_bytes(done), human_bytes(total)),
            };
            ui.add(egui::ProgressBar::new(fraction).text(text));
        }

        if let Some(footer) = what.footer {
            ui.weak(footer);
        }
    });
}

/// What one running job's panel says, besides the bar every panel draws.
struct Panel<'a> {
    /// What to call it on screen, and what its Cancel is named for.
    title: &'a str,
    /// The boundary a cancel stops at, such as a window, a chunk or a block. The
    /// words "stopping at the next..." promise how much is left half-done.
    boundary: &'a str,
    /// Lines drawn above the bar whatever the job has published.
    notes: &'a [String],
    /// The sentence under the bar, once there is a bar.
    footer: Option<&'a str>,
}

/// A verb in flight.
///
/// The footer is the backend's own account of when the write is proved to have
/// landed, taken from the plan a person confirmed. Asserting `PerWindow` here would
/// be false on a DFU board. There a download is one session per region, and
/// nothing can be read until it is committed. It would also word a second time a
/// fact [`ReadBack::describe`] already words for the plan screen.
///
/// [`ReadBack::describe`]: pyrographer_core::agent::ReadBack::describe
fn job_panel(app: &App, job: &crate::state::Job<crate::platform::Wire>, ui: &mut egui::Ui) {
    progress_panel(
        ui,
        app.now(),
        job,
        &Panel {
            title: job.label,
            boundary: "window",
            notes: &[],
            footer: job.read_back.map(|read_back| read_back.describe()),
        },
    );
}

/// A maskrom bootstrap running: the same panel, for the job that has no agent.
///
/// It carries what the window's maskrom gate does for most uploads. With a single
/// SoC pinned, the gate refuses only a file that claims another SoC while `rk3576`
/// is named. For every other upload, the gate makes the container's claim about
/// itself visible, and a claim nothing renders leaves the gate with no effect. The
/// CLI prints the claim before it opens anything. This panel is the window's place
/// for it, drawn raw and not interpreted further, as a `chipver` reply is.
fn bootstrap_panel(app: &App, job: &crate::state::BootstrapJob, ui: &mut egui::Ui) {
    let notes = chip_claim(job.chip.as_deref());
    progress_panel(
        ui,
        app.now(),
        job,
        &Panel {
            title: "Uploading loader",
            boundary: "chunk",
            notes: &notes,
            footer: Some("The board re-enumerates in loader mode once the loader runs."),
        },
    );
}

/// What a loader file claims about which SoC it was built for, as a line to draw.
///
/// One place, so the running panel and the report that outlives it cannot word it
/// differently. For bare stage files the claim is empty, because they carry no
/// container. Saying nothing there would read as a check that passed, so the line
/// says there was nothing to check.
fn chip_claim(chip: Option<&[u8]>) -> Vec<String> {
    match chip {
        Some(chip) => {
            let named = match pyrographer_core::soc::by_container_chip(chip) {
                Some(soc) => soc.name().to_string(),
                None => "no pinned SoC".to_string(),
            };
            vec![format!(
                "This loader's chip field holds {} \"{}\" ({named}).",
                hex(chip),
                ascii(chip)
            )]
        }
        None => vec![
            "These are bare stage files, which carry no container and so name no SoC. Nothing \
             checks which board they are for."
                .to_string(),
        ],
    }
}

/// The raw maskrom stage form: `db --code471/--code472`, drawn.
///
/// This is the window's other way of filling a maskrom board's SRAM, and the one
/// the H96 RAM-boot path needs. `Upload loader...` parses an rkbin container and
/// refuses anything else. This form takes the bare stage blobs mainline U-Boot's
/// binman emits, which carry no container and so have nothing to parse. Both paths
/// end in an `rkboot::LoaderImage`, so they share one upload, one gate and one job.
///
/// **A bare stage names no SoC, and the form says so where a person picks it.**
/// That is a property of the files, not a gap in the gate.
/// `verbs::loader_blob_refusal` passes anything it cannot judge. The running panel
/// repeats the statement, because silence there would read as a check that passed.
fn maskrom_stage_form(app: &mut App, ui: &mut egui::Ui) {
    ui.separator();
    section_label(ui, "Raw stages (no container)");
    prose(
        ui,
        "Upload the bare usb471/usb472 files that mainline U-Boot's binman emits, instead of an \
         rkbin container. Stage 471 initializes DRAM, and stage 472 runs after it. At least one \
         is required, and 471 always goes first. This loads a full U-Boot into DRAM over USB. \
         The board then answers on its serial port rather than on the bus.",
    );

    maskrom_stage_row(app, ui, MaskromStageKind::Code471, "471 (DRAM init)");
    maskrom_stage_row(app, ui, MaskromStageKind::Code472, "472 (loader)");

    guard(
        ui,
        "These files carry no container and so name no SoC. Nothing checks which board they are \
         for. The SoC gate has nothing to compare, and a maskrom board neither reports a chip \
         version nor can be read back.",
    );

    let has_stage = app.maskrom.code_471.is_some() || app.maskrom.code_472.is_some();
    let can_upload = has_stage && !app.is_bootstrapping() && app.session.job.is_none();

    ui.add_space(4.0);
    if ui
        .add_enabled(can_upload, egui::Button::new("Upload the stages..."))
        .explain_disabled("Choose at least one stage file: a 471, a 472, or both.")
        .clicked()
    {
        app.upload_raw_stages();
    }
}

/// One of the two raw maskrom stages: what it is, whether it is chosen, and the
/// buttons to choose or forget it.
fn maskrom_stage_row(app: &mut App, ui: &mut egui::Ui, kind: MaskromStageKind, label: &str) {
    // Read what is chosen out first, so the buttons below can take `app` mutably.
    let chosen: Option<(String, u64)> = {
        let blob = match kind {
            MaskromStageKind::Code471 => app.maskrom.code_471.as_ref(),
            MaskromStageKind::Code472 => app.maskrom.code_472.as_ref(),
        };
        blob.map(|blob| (blob.name.clone(), blob.bytes.len() as u64))
    };

    // Neither is "required" on its own: `db` takes either or both, and the upload
    // button is what enforces that one of them is there.
    let pressed = file_row(ui, label, label, chosen, app.is_picking(), "optional");
    if pressed.choose {
        app.pick_maskrom_stage(kind);
    }
    if pressed.forget {
        match kind {
            MaskromStageKind::Code471 => app.maskrom.code_471 = None,
            MaskromStageKind::Code472 => app.maskrom.code_472 = None,
        }
    }
}

/// The Ingenic boot-ROM bootstrap form.
///
/// This is the USB counterpart of the maskrom "Upload loader" button. It is a form,
/// not a button, because a bootstrap needs a two-stage loader and a per-SoC load
/// address for each stage. That is more than one file pick can carry. The form is
/// revealed on demand for a boot-ROM target. The block verbs appear only after the
/// board re-enumerates as a DFU gadget.
///
/// **\[UNVERIFIED\]**: no Ingenic board has driven this GUI flow. The verbs it
/// calls are pinned against a scripted transport. The load addresses are seeded with
/// thingino-dfu's family-wide defaults and stay editable.
fn ingenic_bootstrap_form(app: &mut App, ui: &mut egui::Ui) {
    ui.separator();
    section_label(ui, "Bootstrap to DFU (Ingenic boot ROM)");
    prose(
        ui,
        "This boot ROM has no flash commands. Upload a DRAM-init SPL and a DFU-capable U-Boot, \
         and the board re-enumerates as a DFU gadget. Its flash is then reachable as named \
         alt-settings. The addresses are thingino's family-wide defaults. Change them if your \
         build links its stages elsewhere. This flow is untested against a real board.",
    );

    ingenic_stage_row(
        app,
        ui,
        IngenicStageKind::Stage1,
        "Stage1 (DRAM-init SPL)",
        true,
    );
    ui.horizontal(|ui| {
        // Named for its stage rather than "load address" twice: two identically
        // named fields are a coin toss read aloud, and the indent that tells them
        // apart on the screen is not in the name.
        named_field(ui, "    Stage1 load address", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.ingenic.stage1_addr)
                    .hint_text("0x________")
                    .desired_width(140.0),
            )
        })
        .explain(
            "Where the SPL loads and runs. The default, 0x80001800, is thingino-dfu's \
             family-wide spl_addr, which every XBurst SoC shares. Change it if your build links \
             the SPL elsewhere.",
        );
    });

    ingenic_stage_row(
        app,
        ui,
        IngenicStageKind::Stage2,
        "Stage2 (DFU U-Boot)",
        false,
    );
    ui.horizontal(|ui| {
        named_field(ui, "    Stage2 load address", |ui| {
            ui.add_enabled(
                app.ingenic.stage2.is_some(),
                egui::TextEdit::singleline(&mut app.ingenic.stage2_addr)
                    .hint_text("0x________")
                    .desired_width(140.0),
            )
        })
        .explain(
            "Where U-Boot loads and runs in DRAM. The default, 0x80100000, is thingino-dfu's \
             family-wide uboot_addr (DRAM base + 1 MiB). Change it if your build links U-Boot \
             elsewhere.",
        );
    });

    ui.horizontal(|ui| {
        named_field(ui, "DRAM settle (ms)", |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.ingenic.settle_ms).desired_width(80.0))
        })
        .explain(
            "How long to wait after stage1 for the memory controller to bring up DRAM, \
                 before stage2 is loaded into it. The community value is around 2000 ms.",
        );
    });

    let has_stage1 = app.ingenic.stage1.is_some();
    let has_stage1_addr = !app.ingenic.stage1_addr.trim().is_empty();
    let can_bootstrap = has_stage1
        && has_stage1_addr
        && !app.is_bootstrapping_ingenic()
        && app.session.job.is_none();

    ui.add_space(4.0);
    let go = ui
        .add_enabled(can_bootstrap, egui::Button::new("Bootstrap..."))
        .explain_disabled(
            "Choose a stage1 SPL and enter its hex load address. A stage2 U-Boot is optional. \
             Without it, the board only initializes DRAM and does not re-enumerate.",
        );
    if go.clicked() {
        app.bootstrap_ingenic();
    }
}

/// One of the bootstrap's two stages: what it is, whether it is chosen, and the
/// buttons to choose or forget it.
fn ingenic_stage_row(
    app: &mut App,
    ui: &mut egui::Ui,
    kind: IngenicStageKind,
    label: &str,
    required: bool,
) {
    // Read what is chosen out first, so the buttons below can take `app` mutably.
    let chosen: Option<(String, u64)> = {
        let blob = match kind {
            IngenicStageKind::Stage1 => app.ingenic.stage1.as_ref(),
            IngenicStageKind::Stage2 => app.ingenic.stage2.as_ref(),
        };
        blob.map(|blob| (blob.name.clone(), blob.bytes.len() as u64))
    };

    let pressed = file_row(
        ui,
        label,
        label,
        chosen,
        app.is_picking_ingenic_stage(),
        if required { "required" } else { "optional" },
    );
    if pressed.choose {
        app.pick_ingenic_stage(kind);
    }
    if pressed.forget {
        match kind {
            IngenicStageKind::Stage1 => app.ingenic.stage1 = None,
            IngenicStageKind::Stage2 => app.ingenic.stage2 = None,
        }
    }
}

/// An Ingenic boot-ROM bootstrap running: the same panel, for the two-stage
/// upload that ends in a re-enumeration as a DFU gadget.
fn ingenic_bootstrap_panel(
    app: &App,
    job: &crate::state::BootstrapJob<pyrographer_core::codec::ingenic_boot::CpuInfo>,
    ui: &mut egui::Ui,
) {
    progress_panel(
        ui,
        app.now(),
        job,
        &Panel {
            title: "Bootstrapping to DFU",
            boundary: "chunk",
            notes: &[],
            footer: Some("The board re-enumerates as a DFU gadget once the U-Boot runs."),
        },
    );
}

/// The StarFive serial recovery entry, which opens the serial flow.
///
/// StarFive's JH7110 recovers over a UART, not the USB bus. It is therefore its own
/// section, not a board in the device list with most verbs grayed. A recovery
/// target has no LBAs, no partitions and no geometry, so there is nothing of the
/// uniform verb surface to disable. One fact about it must be stated plainly:
/// **the write is not read back**. This section states it, and the plan states it
/// again.
///
/// Serial recovery cannot be discovered. A JH7110 in UART recovery is just a
/// serial port, and nothing on it announces the board. No scan reveals it as one
/// reveals a USB board, so it is declared. It is a single secondary action until a
/// person says they have a board to recover, and only then does its form appear.
/// Once the flow is in use it stays open, and "Hide" collapses it again while it
/// is idle.
fn recovery_entry(app: &mut App, ui: &mut egui::Ui) {
    if app.show_recovery || app.session.recovery.is_active() {
        recovery_section(app, ui);
        if app.show_recovery && !app.session.recovery.is_active() {
            ui.horizontal(|ui| {
                if ui.small_button("Hide serial recovery").clicked() {
                    app.show_recovery = false;
                }
            });
        }
        return;
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Recovering a StarFive JH7110?");
        if ui.button("Start serial recovery...").clicked() {
            app.show_recovery = true;
        }
    });
}

/// The port row, the one place the two builds draw a serial flow differently.
///
/// Natively, a port is a place, so it is typed, as in `/dev/ttyUSB0` or `COM3`. In
/// a tab there is nothing to type and no field to type it in. The browser does not
/// enumerate serial ports for a page, and does not open a port by name. The row is
/// therefore a button that opens a chooser, and a line saying what came back.
/// Everything after this row (the target, the files, the plan and the transcript)
/// is drawn the same in both builds.
#[cfg(not(target_arch = "wasm32"))]
fn port_field(app: &mut App, ui: &mut egui::Ui, which: PortField, name: &str) {
    let (port, width) = match which {
        PortField::Recovery => (&mut app.recover.port, 220.0),
        PortField::Console => (&mut app.console.port, 200.0),
    };
    named_field(ui, "Port", |ui| {
        ui.add(
            egui::TextEdit::singleline(port)
                .hint_text("/dev/ttyUSB0")
                .desired_width(width),
        )
        .named(name)
    });
}

/// The port row, in a tab: a chooser and what it gave back.
///
/// The button is disabled while a chooser is already open, as the board chooser's
/// button is. `requestPort` needs a fresh user gesture, and a second chooser opened
/// over the first would answer into a slot nobody is holding.
#[cfg(target_arch = "wasm32")]
fn port_field(app: &mut App, ui: &mut egui::Ui, which: PortField, name: &str) {
    let chosen = match which {
        PortField::Recovery => app.recover.port.clone(),
        PortField::Console => app.console.port.clone(),
    };

    named_field(ui, "Port", |ui| {
        let button = ui
            .add_enabled(
                !app.is_choosing_port(),
                egui::Button::new(if chosen.is_empty() {
                    "Choose port..."
                } else {
                    "Choose a different port..."
                }),
            )
            .named(name)
            .explain(
                "A browser does not accept a port path. It lists the serial ports it can see, \
                 and you pick one. The port is named by its USB-serial adapter, not by the \
                 board connected to it.",
            );
        if button.clicked() {
            app.choose_port(which);
        }
        button
    });

    if chosen.is_empty() {
        ui.weak("no port chosen");
    } else {
        ui.label(chosen);
    }
}

fn recovery_section(app: &mut App, ui: &mut egui::Ui) {
    ui.separator();
    section_label(ui, "StarFive recovery (serial)");
    prose(
        ui,
        "The JH7110 BootROM has no USB. Strap the board into UART recovery, connect a USB-serial \
         adapter, and name its port. Unlike a Rockchip write, this board's write is not read \
         back, because the recovery protocol cannot read flash.",
    );

    ui.horizontal(|ui| {
        port_field(
            app,
            ui,
            PortField::Recovery,
            "Port for the StarFive recovery",
        );
    });

    ui.horizontal(|ui| {
        ui.label("Target");
        ui.radio_value(
            &mut app.recover.target,
            RecoveryTarget::NorFlash,
            "QSPI NOR flash",
        );
        ui.radio_value(&mut app.recover.target, RecoveryTarget::Emmc, "eMMC")
            .explain(
                "eMMC and NOR flash need different SPL headers. Sending one to the other's slot \
                 leaves the board unable to boot. The header is built for the medium chosen \
                 here.",
            );
    });

    recovery_file_row(app, ui, RecoveryFileKind::Agent, "Recovery agent", true);
    recovery_file_row(
        app,
        ui,
        RecoveryFileKind::Spl,
        "SPL (u-boot-spl.bin)",
        false,
    );
    recovery_file_row(app, ui, RecoveryFileKind::Uboot, "U-Boot payload", false);

    let has_agent = app.session.recovery.agent.is_some();
    let has_payload = app.session.recovery.spl.is_some() || app.session.recovery.uboot.is_some();
    let has_port = !app.recover.port.trim().is_empty();
    let can_plan = has_agent && has_payload && has_port && !app.is_recovering();

    ui.add_space(4.0);
    let plan = ui
        .add_enabled(can_plan, egui::Button::new("Plan recovery..."))
        .explain_disabled(
            "Name the port, choose the recovery agent, and choose at least one of an SPL or a \
             U-Boot payload to write.",
        );
    if plan.clicked() {
        app.plan_recovery();
    }
}

/// One of a recovery's three files: what it is, whether it is chosen, and the
/// buttons to choose or forget it.
///
/// The agent is required. The SPL and the U-Boot payload are each optional, but at
/// least one of the two is needed. The plan button enforces that, not this row.
fn recovery_file_row(
    app: &mut App,
    ui: &mut egui::Ui,
    kind: RecoveryFileKind,
    label: &str,
    required: bool,
) {
    // Read what is chosen out first, so the buttons below can take `app` mutably.
    let chosen: Option<(String, u64)> = {
        let blob = match kind {
            RecoveryFileKind::Agent => app.session.recovery.agent.as_ref(),
            RecoveryFileKind::Spl => app.session.recovery.spl.as_ref(),
            RecoveryFileKind::Uboot => app.session.recovery.uboot.as_ref(),
        };
        blob.map(|blob| (blob.name.clone(), blob.bytes.len() as u64))
    };

    let pressed = file_row(
        ui,
        label,
        label,
        chosen,
        app.is_picking_recovery_file(),
        if required { "required" } else { "optional" },
    );
    if pressed.choose {
        app.pick_recovery_file(kind);
    }
    if pressed.forget {
        match kind {
            RecoveryFileKind::Agent => app.session.set_recovery_agent(None),
            RecoveryFileKind::Spl => app.session.set_recovery_spl(None),
            RecoveryFileKind::Uboot => app.session.set_recovery_uboot(None),
        }
    }
}

/// One picked file in a form, drawn by one function.
///
/// Five forms draw exactly this row, identically, for these files:
///
/// - An image
/// - A table layout
/// - A recovery's three files
/// - A bootstrap's two stages
/// - A maskrom's raw stages
///
/// The row is a label and a *Choose...* button. After them comes either the
/// file's name and size beside a *Forget* button, or a word saying nothing is
/// chosen. Each caller says only what it is a row of.
///
/// It returns which button was pressed instead of taking closures. Every caller
/// acts on `app`, and the row is drawn from a borrow of it. For the same
/// reason, each caller reads its `chosen` out first. The borrow checker requires
/// this shape, and it is not a preference.
///
/// `what` names the buttons for an assistive technology. Six *Choose...* buttons on
/// one screen say nothing about which file each one picks. `what` is separate from
/// `label`, because a row reading "Image" needs a button announced as "Choose...
/// an image file".
fn file_row(
    ui: &mut egui::Ui,
    label: &str,
    what: &str,
    chosen: Option<(String, u64)>,
    picking: bool,
    empty: &str,
) -> FileRowPressed {
    let mut pressed = FileRowPressed::default();
    ui.horizontal(|ui| {
        ui.label(label);
        // Disabled while a dialog is up: a file dialog is off-frame, and a second
        // one opened behind the first answers into the same slot.
        pressed.choose = ui
            .add_enabled(!picking, egui::Button::new("Choose..."))
            .named(format!("Choose... {what}"))
            .clicked();

        match chosen {
            Some((name, bytes)) => {
                ui.monospace(name);
                ui.weak(human_bytes(bytes));
                pressed.forget = ui
                    .button("Forget")
                    .named(format!("Forget {what}"))
                    .clicked();
            }
            None => {
                ui.weak(empty);
            }
        }
    });
    pressed
}

/// Which of a [`file_row`]'s two buttons was pressed this frame.
#[derive(Default)]
struct FileRowPressed {
    /// The *Choose...* button.
    choose: bool,
    /// The *Forget* button, drawn only for a chosen file.
    forget: bool,
}

/// A recovery running: the same shape as [`bootstrap_panel`], for the serial job.
///
/// It carries the same reminder the plan does. A progress bar filling up reads as
/// a write being verified, and this write is not verified.
fn recovery_job_panel(app: &App, job: &crate::state::RecoveryJob, ui: &mut egui::Ui) {
    progress_panel(
        ui,
        app.now(),
        job,
        &Panel {
            title: "Recovering over serial",
            boundary: "block",
            notes: &[],
            footer: Some(
                "Each block is acknowledged as it is received. The write is not read back.",
            ),
        },
    );
}

/// The serial console entry.
///
/// The console is part of the serial flow, beside StarFive recovery, and is not a
/// third flow. The number of flows is bounded by how a board is physically
/// reached. A console is reached exactly as a recovery is: a person names a port.
/// Only what answers on the port differs.
///
/// It is declared rather than discovered, for the same reason a recovery is. A
/// board with a console on a UART is just a port, and nothing on it announces the
/// board.
fn console_entry(app: &mut App, ui: &mut egui::Ui) {
    if app.show_console || app.session.console.is_active() {
        console_section(app, ui);
        if app.show_console && !app.session.console.is_active() {
            ui.horizontal(|ui| {
                if ui.small_button("Hide serial console").clicked() {
                    app.show_console = false;
                }
            });
        }
        return;
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("A board with a console on a serial port?");
        if ui.button("Open serial console...").clicked() {
            app.show_console = true;
        }
    });
}

/// The console section: the line, the passive watch, and the U-Boot half.
fn console_section(app: &mut App, ui: &mut egui::Ui) {
    ui.separator();
    section_label(ui, "Serial console");
    prose(
        ui,
        "A bootloader prompt is the last stage before an operating system. pyrographer can watch \
         what a board prints, start a gadget so its flash appears on USB, or change where it \
         boots from for one boot. pyrographer does not drive a login prompt.",
    );

    let busy = app.is_console_busy();

    ui.horizontal(|ui| {
        port_field(app, ui, PortField::Console, "Port for the serial console");
        named_field(ui, "Baud", |ui| {
            // **Ranged, and draggable.** It was the only `DragValue` in the crate
            // with no range and the only one with `speed(0.0)`, which made it a
            // click-to-type field wearing a drag control -- and `0` was accepted
            // and reached the open. The floor is a rate a UART can actually be set
            // to; the ceiling is past every rate on the bench.
            ui.add(
                egui::DragValue::new(&mut app.console.baud)
                    .speed(100.0)
                    .range(300..=4_000_000),
            )
        })
        .explain(
            "115200 is the JH7110 recovery UART's rate, and the default. A board's console \
                 runs at the rate its build sets. An RK3576's console runs at 1500000.",
        );
    });

    console_watch(app, ui, busy);
    console_uboot(app, ui, busy);
}

/// The passive watch: the cheapest thing here, and the one that covers the most
/// ground.
///
/// It uses no prompt, no echo handling, no login and no credentials. That is what a
/// node that runs its own self-test at boot needs from a host. It reports what
/// appeared and does not assert, and the button's own words say so.
fn console_watch(app: &mut App, ui: &mut egui::Ui, busy: bool) {
    ui.add_space(6.0);
    ui.strong("Watch");
    prose(
        ui,
        "One pattern per line, matched as bytes with no line splitting, so a trailing space is \
         part of the pattern. Write a byte you cannot type as \\n, \\r, \\t, \\0, \\\\, or \
         \\xNN. The pattern the board printed first is the one reported.",
    );

    ui.horizontal(|ui| {
        named_field(ui, "Worked", |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut app.console.expect)
                    .hint_text("PASS")
                    .desired_rows(2)
                    .desired_width(200.0),
            )
        });
        named_field(ui, "Failed", |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut app.console.fail)
                    .hint_text("FAIL")
                    .desired_rows(2)
                    .desired_width(200.0),
            )
        });
    });

    let has_port = !app.console.port.trim().is_empty();
    let has_pattern = !app.console.expect.trim().is_empty() || !app.console.fail.trim().is_empty();
    if ui
        .add_enabled(
            !busy && has_port && has_pattern,
            egui::Button::new("Watch the console..."),
        )
        .explain_disabled(
            "Name the port, and enter at least one pattern: the text that means the board \
             worked, or the text that means it reported a failure.",
        )
        .clicked()
    {
        let now = app.now();
        app.watch_console(now);
    }
}

/// The U-Boot half: revealed rather than always drawn.
///
/// A watch needs no prompt and no board knowledge. Driving a prompt needs both: a
/// prompt string, and the block device a gadget exposes. This half therefore opens
/// on request, instead of sitting open in front of a person who only wants to read
/// a console.
fn console_uboot(app: &mut App, ui: &mut egui::Ui, busy: bool) {
    ui.add_space(6.0);
    if !app.console.show_uboot {
        if ui.button("Drive a U-Boot prompt...").clicked() {
            app.console.show_uboot = true;
        }
        return;
    }

    ui.horizontal(|ui| {
        ui.strong("U-Boot");
        if ui
            .small_button("Hide")
            .named("Hide the U-Boot controls")
            .clicked()
        {
            app.console.show_uboot = false;
        }
    });
    prose(
        ui,
        "Every action here first interrupts the autoboot by sending a bare newline. The newline \
         stops a countdown. On a board already at a prompt, it only produces another prompt.",
    );

    ui.horizontal(|ui| {
        named_field(ui, "Prompt", |ui| {
            ui.add(egui::TextEdit::singleline(&mut app.console.prompt).desired_width(90.0))
        })
        .explain(
            "Boards use different prompts, such as \"=> \", \"U-Boot> \", or a board-specific \
             CONFIG_SYS_PROMPT. pyrographer has no built-in list of them. The trailing space is \
             part of the prompt.",
        );
        named_field(ui, "Reads per wait", |ui| {
            ui.add(egui::DragValue::new(&mut app.console.reads).range(1..=600))
        })
        .explain(
            "How long to wait, counted in reads rather than seconds. pyrographer's core reads \
                 no clock, because Instant::now() panics in a browser. Each read waits about one \
                 second on an idle line, and returns as soon as bytes arrive.",
        );
    });

    let has_port = !app.console.port.trim().is_empty();

    // The far end of the RAM-boot loop: a mainline U-Boot pushed into DRAM from
    // maskrom hands its flash back to the host through one of two gadgets.
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !busy && has_port,
                egui::Button::new("Start the rockusb gadget"),
            )
            .explain(
                "Exposes the board's flash over USB, so the board appears in the device list as \
                 a loader. This completes a maskrom upload of a full U-Boot. The upload loads \
                 U-Boot into DRAM, and this gadget makes it answer on the bus.",
            )
            .clicked()
        {
            let now = app.now();
            app.start_gadget(Gadget::Rockusb, now);
        }
        if ui
            .add_enabled(
                !busy && has_port,
                egui::Button::new("Start the mass-storage gadget"),
            )
            .explain(
                "Exposes the board's flash to this machine as a USB disk, which appears under \
                 Disks when the disks are listed. On an RK3576, this route reads the whole eMMC, \
                 past the 32 MiB point where rkbin's loader returns fill.",
            )
            .clicked()
        {
            let now = app.now();
            app.start_gadget(Gadget::Ums, now);
        }
        // "on" reads as a preposition on the screen and as nothing at all read
        // aloud, so the name says which field this is.
        named_field(ui, "on block device", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.console.gadget_dev)
                    .hint_text("mmc:0")
                    .desired_width(90.0),
            )
        })
        .explain(
            "Which block device the gadget exposes. Use the form <controller>:<interface>:<index> \
             when the gadget is not on USB controller 0.",
        );
    });

    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !busy && has_port && !app.console.targets.trim().is_empty(),
                egui::Button::new("Boot from..."),
            )
            .explain_disabled("Name the boot order to set, as in mmc0 or `mmc0 usb0`.")
            .clicked()
        {
            let now = app.now();
            app.plan_boot_override(now);
        }
        named_field(ui, "boot order", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.console.targets)
                    .hint_text("mmc0")
                    .desired_width(150.0),
            )
        });
        ui.weak("for one boot, and nothing is saved");
    });

    console_command(app, ui, busy, has_port);
}

/// The raw command line, and the one thing on this screen that is ungated.
///
/// It is useful, and it is the one route by which `saveenv` can be typed. It
/// therefore says so: U-Boot runs whatever is typed. Nothing about it is planned or
/// confirmed, because typing at a prompt through pyrographer is the same act as
/// typing at the prompt directly.
fn console_command(app: &mut App, ui: &mut egui::Ui, busy: bool, has_port: bool) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let typed = named_field(ui, "Command", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.console.command)
                    .hint_text("printenv")
                    .desired_width(240.0),
            )
        });
        let entered = typed.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let clicked = ui
            .add_enabled(
                !busy && has_port && !app.console.command.trim().is_empty(),
                egui::Button::new("Run"),
            )
            .clicked();
        if (clicked || entered) && !busy && has_port {
            let now = app.now();
            app.run_console_command(now);
        }
    });
    ui.colored_label(
        ui.visuals().warn_fg_color,
        "This is ungated. U-Boot runs whatever you type, saveenv included, with no plan and no \
         confirmation.",
    );
}

/// A boot override waiting to be agreed to.
///
/// It is deliberately not a screen of its own. A write plan takes the whole window
/// and asks for a typed coordinate, because it overwrites a board. A clone with its
/// ends swapped is a mistake careful reading does not catch. An override has no
/// second board and destroys nothing: it changes volatile RAM for one boot.
/// Ceremony on a harmless act teaches a person to stop reading the ceremony on a
/// dangerous one. This plan is therefore a group and a plain yes.
fn boot_plan(app: &mut App, ui: &mut egui::Ui) {
    let Some(pending) = &app.session.console.pending else {
        return;
    };
    let plan = pending.plan.clone();
    let port = pending.line.port.clone();

    ui.group(|ui| {
        ui.strong(format!("Change what the board on {port} boots from?"));

        ui.horizontal(|ui| {
            ui.label("boots from now");
            match &plan.current {
                Some(current) => {
                    ui.monospace(current);
                }
                None => {
                    ui.weak("this U-Boot build sets no boot_targets");
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("would boot from");
            ui.monospace(&plan.targets);
        });

        if plan.is_no_change() {
            ui.weak("That is the order it already boots in, so this changes nothing.");
        }
        if !plan.persistent {
            ui.label(
                "The change is not saved. U-Boot keeps its environment in RAM until `saveenv` \
                 writes it to storage, and the override never sends `saveenv`. The next reset \
                 restores the board's own boot order. The board boots immediately after the order \
                 is set.",
            );
        }

        ui.horizontal(|ui| {
            if ui.button("Set it and boot").clicked() {
                let now = app.now();
                app.boot_override(now);
            }
            if ui.button("Cancel").clicked() {
                app.session.dismiss_boot_plan();
            }
        });
    });
}

/// A console session running, and the transcript it has read so far.
///
/// Byte counts do not describe a console session, so there is no progress bar. A
/// person watching a console needs the text, streaming as it arrives. A session
/// that shows nothing while it waits is indistinguishable from a hang.
fn console_job_panel(job: &crate::state::ConsoleJob, ui: &mut egui::Ui) {
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.strong(job.label);
            if ui.button("Stop").clicked() {
                job.cancel();
            }
            if job.is_canceling() {
                ui.weak("stopping at the next read...");
            }
        });
        if job.is_quiet() {
            ui.weak("No output yet. If the board is off, power it on.");
        }
        // **Rendered when it changes, not every frame.** `transcript()` locks the
        // shared cell and runs the codec over up to 16 KiB; at 60 fps that is 16
        // KiB decoded and some sixteen thousand glyphs measured a frame, for text
        // that changes only when a board says something. The key is the count of
        // bytes the session has *seen*, not the tail's length -- the tail stops
        // growing the moment it is full, and a session that has filled it is
        // exactly the one whose text is still moving.
        let id = ui.id().with("console_transcript");
        let seen = job.bytes_seen();
        let cached: Option<(u64, String)> = ui.memory(|memory| memory.data.get_temp(id));
        let text = match cached {
            Some((at, text)) if at == seen => text,
            _ => {
                let text = job.transcript();
                ui.memory_mut(|memory| memory.data.insert_temp(id, (seen, text.clone())));
                text
            }
        };
        transcript_box(ui, &text);
    });
}

/// The console transcript, in a scrolling monospace box that follows the end.
///
/// The codec renders it, so an autoboot countdown's backspaces and carriage returns
/// read as what the board said. They are not passed through as instructions to a
/// terminal that is not there.
fn transcript_box(ui: &mut egui::Ui, transcript: &str) {
    if transcript.is_empty() {
        return;
    }
    egui::ScrollArea::vertical()
        .max_height(200.0)
        .stick_to_bottom(true)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.monospace(transcript);
        });
}

/// What the last job produced, or what went wrong.
fn report(outcome: &Outcome, ui: &mut egui::Ui) {
    ui.group(|ui| match outcome {
        Ok(report) => success(report, ui),
        Err(error) => failure(error, ui),
    });
}

/// A job that worked.
fn success(report: &Report, ui: &mut egui::Ui) {
    match report {
        Report::Info(flash) => {
            ui.strong("Flash");
            ui.label(geometry(flash));
            if let Some(id) = &flash.chip_id {
                ui.horizontal(|ui| {
                    ui.label("Flash ID");
                    ui.monospace(hex(id));
                });
            }
        }
        Report::Partitions(Some(table)) => {
            ui.strong(format!(
                "{} ({} partitions)",
                table.format.name(),
                table.partitions.len()
            ));
        }
        Report::Partitions(None) => {
            ui.strong("This board has no partition table.");
            ui.weak(
                "This is a finding, not a failure. A board that holds a raw image, or a blank \
                 board, has no table and is not broken.",
            );
        }
        Report::ChipVersion(version) => {
            ui.strong("The loader's answer, uninterpreted");
            ui.horizontal(|ui| {
                ui.monospace(hex(version));
                ui.monospace(format!("\"{}\"", ascii(version)));
            });
            ui.weak(
                "The opcode is documented, but the layout of the reply is not. pyrographer shows \
                 the reply as it came back, without interpreting it. The write gate compares \
                 these bytes whole against the reply pinned for the named SoC.",
            );
        }
        Report::Capability(capability) => capability_report(capability.as_ref(), ui),
        Report::StorageMedium(medium) => match medium {
            Some(pyrographer_core::codec::rockusb::StorageMedium::Unknown(index)) => {
                ui.strong(format!(
                    "Storage medium: index {index}, which pyrographer has no name for."
                ));
                ui.weak(
                    "The loader answered, and the answer is outside the table taken from the \
                     reference tools. The index is what it reported.",
                );
            }
            Some(medium) => {
                ui.strong(format!("Storage medium: {}", medium.name()));
                ui.weak(
                    "Every LBA on this screen is an offset into that medium. pyrographer reports \
                     which medium is active and never switches it.",
                );
            }
            None => {
                ui.strong("This device addresses no single storage medium.");
            }
        },
        Report::Reset { mode } => {
            ui.strong(mode.outcome());
        }
        Report::Dumped { bytes, name, fill } => {
            ui.strong(format!("Read {} into {name}.", human_bytes(*bytes)));
            fill_warning(ui, fill);
        }
        Report::Verified { bytes, fill } => {
            ui.strong(format!(
                "The flash matches the image, over all {}.",
                human_bytes(*bytes)
            ));
            fill_warning(ui, fill);
        }
        Report::Wrote { bytes } => {
            ui.strong(format!(
                "Wrote {}, and read every window of it back.",
                human_bytes(*bytes)
            ));
        }
        Report::Cloned { bytes, fill } => {
            ui.strong(format!(
                "Cloned {}, and read every window of it back.",
                human_bytes(*bytes)
            ));
            // The fill is the source's, read as it was copied -- and a clone
            // across it wrote that fill onto the destination, so the warning is
            // about the board just written.
            fill_warning(ui, fill);
        }
        Report::Bootstrapped { chip } => {
            ui.strong("Loader uploaded.");
            // What the file claimed about itself, kept where a person can still
            // read it after the panel that first showed it has gone.
            for note in chip_claim(chip.as_deref()) {
                ui.weak(note);
            }
            ui.weak(
                "The board re-enumerates in loader mode. When it reappears in the device list, \
                 select it.",
            );
            // **Where the two halves meet.** When the loader that was uploaded is a
            // RAM-booted U-Boot rather than a bare rockusb responder, the board's
            // next words come out of a serial port and not the bus -- so nothing
            // reappears here, and a person who does not know that reads a silent
            // device list as a failure.
            ui.weak(
                "If you uploaded a full U-Boot rather than a bare loader, the board answers on \
                 its serial port, not on the bus. Open the serial console on the Serial tab and \
                 start a gadget from its prompt. The rockusb gadget returns the board to this \
                 list. The mass-storage gadget brings its flash up under Disks.",
            );
        }
        Report::IngenicBootstrapped(cpu) => {
            ui.strong("Bootstrapped to DFU.");
            // The SoC magic, shown raw. This is the bench artifact: it is the one
            // point in the flow the identity is readable, and pinning it into the
            // `soc` module is what later lets a write stop refusing -- so it is
            // printed, not interpreted, the way `chipver` is.
            ui.horizontal(|ui| {
                ui.label("The boot ROM reported CPU info");
                ui.monospace(hex(&cpu.magic));
                ui.monospace(format!("\"{}\"", cpu.text()));
            });
            ui.weak(
                "Record those bytes. A write to this board is refused until they are pinned \
                 into the SoC gate. The board re-enumerates as a DFU gadget. When it reappears \
                 in the device list, select it.",
            );
            // **A tab is not handed the board back.** A browser grants USB
            // permission per device, and the DFU gadget is a different device --
            // the mode is in the product ID, so it enumerates at 0x4d44 where the
            // boot ROM was at its family's own. The grant does not follow it, and
            // nothing appears in the list on its own. Natively the next scan of
            // the bus finds it, which is why this sentence is drawn only here.
            #[cfg(target_arch = "wasm32")]
            ui.weak(
                "In a browser, the DFU gadget is a separate device with its own product ID. The \
                 permission granted for the boot ROM does not cover it, so choose the board again \
                 to grant permission.",
            );
        }
        Report::Recovered => {
            ui.strong("Recovery complete.");
            ui.weak(
                "The transfer was acknowledged, but this board's write cannot be read back. \
                 Power off, return the boot strap to normal, and power on.",
            );
        }
        Report::TableWritten { format, authored } => {
            if *authored {
                ui.strong(format!(
                    "Wrote a fresh {} table, and read every window of it back.",
                    format.name()
                ));
                ui.weak(
                    "Read the partition table again to see the board's new map. Nothing outside \
                     the table's own sectors was touched.",
                );
            } else {
                ui.strong(format!(
                    "Rewrote the damaged {} copy, and read every window of it back.",
                    format.name()
                ));
                ui.weak(
                    "The intact copy was left as it was. Read the partition table again to see the \
                     copies now agree.",
                );
            }
        }
        Report::Console(console) => console_report(console, ui),
        // The plan is a screen of its own, and it is showing.
        Report::Planned(_) | Report::PlannedClone(_) | Report::PlannedTable(_) => {}
    }
}

/// The loader's own account of what it can do.
///
/// The flags are the loader's claims, and pyrographer gates nothing on them. The
/// CLI says the same, for the same reason. The last line carries information: a
/// bit the table cannot name is reported as a finding. The loader set something
/// this code has no account of, and reporting it tells a person more than hiding
/// it would.
fn capability_report(
    capability: Option<&pyrographer_core::codec::rockusb::Capability>,
    ui: &mut egui::Ui,
) {
    let Some(capability) = capability else {
        ui.strong("This device does not answer a capability query.");
        return;
    };

    ui.strong("What the loader says it can do");
    ui.horizontal(|ui| {
        ui.label("Capability");
        ui.monospace(hex(capability.raw()));
    });

    let flags = [
        ("direct LBA", capability.direct_lba()),
        ("vendor storage", capability.vendor_storage()),
        ("first 4M access", capability.first_4m_access()),
        ("read LBA", capability.read_lba()),
        ("read COM log", capability.read_com_log()),
        ("read IDB config", capability.read_idb_config()),
        ("read secure mode", capability.read_secure_mode()),
        ("new IDB", capability.new_idb()),
        ("switch storage", capability.switch_storage()),
    ];
    egui::Grid::new("capability")
        .num_columns(2)
        .striped(true)
        .spacing([16.0, 2.0])
        .show(ui, |ui| {
            for (name, set) in flags {
                ui.monospace(if set { "yes" } else { "no" });
                ui.label(name);
                ui.end_row();
            }
        });

    let unnamed = capability.unnamed_bits();
    if unnamed.iter().any(|byte| *byte != 0) {
        guard(
            ui,
            format!(
                "This loader set bits pyrographer has no name for: {}",
                hex(&unnamed)
            ),
        );
    }
}

/// What a console session came back with.
///
/// A watch reports what appeared and does not assert. A failure pattern is
/// therefore drawn in the caution color and named as what the board said. It is
/// not presented as a fault in pyrographer or in the request.
fn console_report(report: &state::ConsoleReport, ui: &mut egui::Ui) {
    use pyrographer_core::console::Seen;

    match report {
        state::ConsoleReport::Watched(watched) => {
            let seen = console_codec::text(&watched.pattern);
            match watched.seen {
                Seen::Expected => ui.strong(format!("Saw \"{seen}\" on the console.")),
                Seen::Failed => ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Saw \"{seen}\", the pattern that means the board reported a failure."),
                ),
            };
        }
        state::ConsoleReport::GadgetStarted { gadget, command } => {
            ui.strong("The gadget is running.");
            ui.horizontal(|ui| {
                ui.label("Ran");
                ui.monospace(command);
            });
            ui.weak(match gadget {
                Gadget::Rockusb => {
                    "The prompt does not return while the gadget runs. To find the board on USB, \
                     rescan the device list on the Boards and disks tab."
                }
                Gadget::Ums => {
                    "The prompt does not return while the gadget runs. To find the board's flash \
                     as a USB disk, open Disks on the Boards and disks tab."
                }
            });
        }
        state::ConsoleReport::Answered(output) => {
            ui.strong("The prompt answered.");
            if output.is_empty() {
                ui.weak("It printed nothing.");
            } else {
                ui.monospace(output);
            }
        }
        state::ConsoleReport::Booted(targets) => {
            ui.strong(format!(
                "The board is booting from {targets}, for this boot only."
            ));
            ui.weak(
                "Nothing was saved. U-Boot keeps its environment in RAM until `saveenv` writes \
                 it, and the override never sends `saveenv`. The next reset restores the board's \
                 previous boot order.",
            );
        }
        // The plan is a screen of its own, and it is showing.
        state::ConsoleReport::BootPlanned(_) => {}
    }
}

/// Draw the fill finding returned with a dump, a verify, or a clone (which scans
/// the source it copied).
///
/// A run of any byte but `0x00`/`0xff` is drawn in the caution color. Constant
/// fill reported as a successful read can be a fill byte instead of data. An image
/// taken across such a run looks complete, and is not. A blank run
/// (`0x00`/`0xff`) is how erased or unallocated flash ordinarily reads, and gets a
/// quiet note. Nothing is drawn for a healthy read. See [`pyrographer_core::fill`].
fn fill_warning(ui: &mut egui::Ui, fill: &FillReport) {
    for run in fill.suspicious() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "{} from sector {} read back as constant {:#04x}, which can be a silent read \
                 failure.",
                human_bytes(run.bytes()),
                run.first_lba(),
                run.byte(),
            ),
        );
    }
    if fill.has_suspicious() {
        ui.weak(
            "A loader can answer a read it cannot serve with constant fill and a success \
             status. Read the region another way, such as over mass storage or from the board's \
             own console, before relying on this image.",
        );
    }

    let blank_bytes: u64 = fill.blank().map(|run| run.bytes()).sum();
    if blank_bytes > 0 {
        ui.weak(format!(
            "{} read back as constant 0x00 or 0xff, consistent with erased or unallocated flash.",
            human_bytes(blank_bytes),
        ));
    }
}

/// A job that did not.
///
/// It draws the error's hint as well as the error. Core diagnoses the error and
/// returns prose instead of printing it, and this callout is where that prose is
/// shown. A device that will not open gets the udev rule that fixes it. A device
/// that stopped answering gets the advice to use a powered USB-2 hub.
fn failure(error: &Error, ui: &mut egui::Ui) {
    ui.colored_label(ui.visuals().error_fg_color, error.to_string());
    if let Some(hint) = error.hint() {
        ui.add_space(4.0);
        ui.label(hint);
    }
}

/// The plan, as a screen.
///
/// The four steps are the CLI's, unchanged, because there is one write path:
///
/// - Plan
/// - Confirm
/// - Write window by window, and read every window back
/// - Report
///
/// The backend sets the read-back's timing: each window before the next goes out,
/// or the whole region after its commit. A write plan states it in its "checked"
/// row, in the words of
/// [`ReadBack::describe`](pyrographer_core::agent::ReadBack::describe).
///
/// The GUI adds two things: the plan is a screen instead of a paragraph, and it
/// cannot be skipped.
fn plan_screen(app: &mut App, ui: &mut egui::Ui) {
    let now = app.now();

    // Copied out, so that the confirmation below can take the session mutably --
    // it has a text field to edit and a plan to consume. A plan is a handful of
    // numbers and a partition list, and copying it once a frame is nothing
    // against the screen that is about to overwrite a board.
    let Some(plan) = app
        .session
        .pending
        .as_ref()
        .map(|pending| pending.plan.clone())
    else {
        return;
    };

    screen_header(
        ui,
        "This overwrites flash",
        "Nothing here can be undone.",
        Tone::Danger,
    );

    egui::ScrollArea::vertical().show(ui, |ui| {
        // **When two gates are waiting, the one that is not drawn is named.**
        // This screen wins over the recovery gate, so dismissing it would
        // otherwise drop somebody straight into a second one -- which reads as
        // this screen changing under them rather than as a different plan.
        // `App::plan_recovery` refuses to make that happen from the form; this
        // covers the race where a plan job lands while a recovery plan is up.
        if app.session.recovery.pending.is_some() {
            guard(
                ui,
                "A StarFive recovery plan is also waiting to be answered. It is shown again when \
                 this plan is confirmed or canceled.",
            );
            ui.add_space(8.0);
        }

        match &plan {
            Plan::Write(plan) => write_plan(app, plan, ui),
            Plan::Clone(plan) => clone_plan(app, plan, ui),
            Plan::Table(plan) => table_plan(app, plan, ui),
        }

        ui.add_space(8.0);
        ui.separator();
        confirmation(app, ui, now);
    });
}

/// What a write would touch.
fn write_plan(app: &App, plan: &WritePlan, ui: &mut egui::Ui) {
    egui::Grid::new("write-plan")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            ui.label("destination");
            if let Some(device) = &app.session.target.device {
                ui.monospace(device_line(device));
            }
            ui.end_row();

            ui.label("image");
            match &app.session.image {
                Some(image) => ui.monospace(format!(
                    "{}  ({})",
                    image.name,
                    human_bytes(plan.image_bytes)
                )),
                None => ui.monospace(human_bytes(plan.image_bytes)),
            };
            ui.end_row();

            if plan.padding_bytes > 0 {
                ui.label("padding");
                ui.monospace(format!(
                    "{} of zeros, out to the end of the last sector",
                    human_bytes(plan.padding_bytes)
                ));
                ui.end_row();
            }

            range_rows(plan, ui);
            ui.end_row();

            ui.label("touches");
            touches(&plan.touches, ui);
            ui.end_row();

            ui.label("flash");
            ui.monospace(geometry(&plan.flash));
            ui.end_row();

            loader_row(app.session.target.device.as_ref(), plan, ui);
            ui.end_row();

            // When the write gets checked, in the backend's own words. It is a row
            // of the plan rather than a footnote because it is not the same answer
            // on every board, and the difference is what a mistake costs.
            ui.label("checked");
            ui.label(plan.read_back.describe());
            ui.end_row();
        });
}

/// What a clone would touch, at both ends.
///
/// Both ends are spelled out, because the mistake this screen exists to catch costs
/// the most. With the source and the destination swapped, a clone destroys the
/// board a person meant to copy.
fn clone_plan(app: &App, plan: &ClonePlan, ui: &mut egui::Ui) {
    let write = &plan.destination;

    egui::Grid::new("clone-plan")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            ui.label("copied");
            if let Some(device) = &app.session.source.device {
                ui.monospace(format!("{}   read only", device_line(device)));
            }
            ui.end_row();

            ui.label("overwritten");
            if let Some(device) = &app.session.target.device {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    egui::RichText::new(device_line(device)).monospace(),
                );
            }
            ui.end_row();

            ui.label("source flash");
            ui.monospace(geometry(&plan.source));
            ui.end_row();

            range_rows(write, ui);
            ui.end_row();

            ui.label("touches");
            touches(&write.touches, ui);
            ui.end_row();

            ui.label("destination flash");
            ui.monospace(geometry(&write.flash));
            ui.end_row();

            loader_row(app.session.target.device.as_ref(), write, ui);
            ui.end_row();

            // The destination's, because the destination is what is read back.
            // The source is only read, and there is nothing to check it against.
            ui.label("checked");
            ui.label(write.read_back.describe());
            ui.end_row();
        });
}

/// What a table write (a repair or an authoring) would do.
///
/// A table write is a write, so this shows what a write's plan shows: the flash
/// geometry, and the loader the gate is enforced against. It also shows what is
/// particular to a table write. That is whether it repairs or authors, the copies
/// it lays down and where, and the partitions the resulting table holds. A person
/// recognizes the table by those partitions.
///
/// Every copy being written is colored for danger. A repair leaves the intact copy
/// it rebuilds from untouched, and says so.
fn table_plan(app: &App, plan: &SegmentedPlan, ui: &mut egui::Ui) {
    let sector_size = u64::from(plan.flash.sector_size);
    let (seg_label, act) = match &plan.action {
        TableAction::Repair { source } => (
            "rewriting",
            format!("repair the {} table, from {source}", plan.format.name()),
        ),
        TableAction::Author => (
            "writing",
            format!("author the {} table", plan.format.name()),
        ),
    };

    egui::Grid::new("table-plan")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            ui.label(match &app.session.target.device {
                Some(Chosen::Block(_)) => "disk",
                _ => "board",
            });
            if let Some(device) = &app.session.target.device {
                ui.monospace(device_line(device));
            }
            ui.end_row();

            ui.label("act");
            ui.label(act);
            ui.end_row();

            for segment in &plan.segments {
                let sectors = (segment.bytes.len() as u64).div_ceil(sector_size);
                let last = (segment.lba + sectors).saturating_sub(1);
                ui.label(seg_label);
                ui.vertical(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, &segment.what);
                    ui.monospace(format!(
                        "LBA {} through {last}  ({sectors} sectors)",
                        segment.lba
                    ));
                });
                ui.end_row();
            }

            ui.label("partitions");
            partition_list(&plan.partitions, ui);
            ui.end_row();

            ui.label("flash");
            ui.monospace(geometry(&plan.flash));
            ui.end_row();

            gate_rows(
                app.session.target.device.as_ref(),
                &plan.chip_version,
                &plan.soc,
                ui,
            );
            ui.end_row();

            // A table write is a write, so it says when it is checked in the
            // same words a write's plan does.
            ui.label("checked");
            ui.label(plan.read_back.describe());
            ui.end_row();
        });
}

/// The partitions a written table would carry, for a person to recognize the
/// table by.
///
/// The GUID and attribute detail a repaired copy holds is copied faithfully onto
/// the flash, but not shown. A person checks a name and a size.
fn partition_list(partitions: &[Partition], ui: &mut egui::Ui) {
    ui.vertical(|ui| {
        if partitions.is_empty() {
            ui.label("nothing named: the table has no partitions");
            return;
        }
        for partition in partitions {
            ui.horizontal(|ui| {
                ui.monospace(egui::RichText::new(&partition.name).strong());
                ui.label(format!("{} sectors", partition.sectors));
            });
        }
    });
}

/// Where the bytes land, in sectors and in bytes.
fn range_rows(plan: &WritePlan, ui: &mut egui::Ui) {
    let sector_size = u64::from(plan.flash.sector_size);
    let last = plan.lba.saturating_add(plan.sectors).saturating_sub(1);

    ui.label("LBA range");
    ui.monospace(format!(
        "{} through {last}  ({} sectors)",
        plan.lba, plan.sectors
    ));
    ui.end_row();

    // Saturating throughout, the way the rest of the crate is. A validated plan
    // bounds these today, so this is consistency rather than a live defect -- but
    // it is arithmetic on the screen a person reads before flash is overwritten,
    // and an overflow panic there takes the frame thread and the window with it.
    ui.label("byte range");
    ui.monospace(format!(
        "{} through {}",
        plan.lba.saturating_mul(sector_size),
        plan.lba
            .saturating_add(plan.sectors)
            .saturating_mul(sector_size)
            .saturating_sub(1)
    ));
}

/// The loader's own account of what it is running on, raw.
///
/// This is the evidence the wrong-loader refusal rests on. It is on this screen
/// because a person deciding whether to confirm needs to see it. It is
/// uninterpreted. The gate compares exact pinned bytes, never a decoded shape, so
/// pyrographer shows the bytes and reads nothing into them.
fn loader_row(device: Option<&Chosen>, plan: &WritePlan, ui: &mut egui::Ui) {
    gate_rows(device, &plan.chip_version, &plan.soc, ui);
}

/// The last rows of a plan: what guards this write.
///
/// **The rows differ on a disk, and the board's rows there would be worse than
/// none.** On a board they are the loader's own answer and the verdict on it. A
/// disk runs no loader, so the wrong-loader gate does not apply. The board form
/// would print *the write is refused until one is named*, which is false for a
/// disk. A plan line that is reliably wrong teaches a person to skip the plan.
///
/// On a disk, the same two rows state what does guard the write. The kernel holds
/// this disk for pyrographer alone, and no disk the running system rests on can be
/// opened at all. The CLI's plan says the same two things, for the same reason.
fn gate_rows(device: Option<&Chosen>, chip_version: &[u8], soc: &Option<Soc>, ui: &mut egui::Ui) {
    let Some(Chosen::Block(disk)) = device else {
        loader_rows(chip_version, soc, ui);
        return;
    };

    ui.label("held");
    ui.vertical(|ui| {
        ui.label(
            "exclusively (O_EXCL) for as long as this disk is open, so nothing else on this \
             machine can mount or write it",
        );
        // Not a tooltip. This screen is the last thing read before flash is
        // overwritten, and a sentence that needs a pointer is a sentence a
        // keyboard user and a screen reader both go without.
        ui.weak(
            "pyrographer asks the kernel for exclusive use rather than checking for itself. The \
             kernel knows about holders that pyrographer cannot list, such as a LUKS container \
             with a volume stacked on it or an active swap device. Neither carries a mount.",
        );
    });
    ui.end_row();

    ui.label("running system");
    if disk.carries_running_system {
        // Unreachable through an open disk -- the open refuses this before a plan
        // exists -- and drawn anyway, because a gate that only speaks when it
        // fires is a gate nobody can see.
        ui.colored_label(
            ui.visuals().error_fg_color,
            "rests on this disk, and it cannot be opened",
        );
    } else {
        ui.label("does not rest on this disk");
    }
}

/// The loader rows, built from the two gate fields, so a plain write's plan and a
/// table write's plan render the loader the same way.
fn loader_rows(chip_version: &[u8], soc: &Option<Soc>, ui: &mut egui::Ui) {
    ui.label("loader says");
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.monospace(hex(chip_version));
            ui.monospace(format!("\"{}\"", ascii(chip_version)));
        });
        // Without this the row is four bytes and a word, and nothing says the
        // bytes are what the gate compares. It used to be a tooltip on a
        // `horizontal`'s response -- a container no keyboard can reach and the
        // accessibility tree gives no name at all.
        ui.weak(
            "The K_FW_GET_CHIP_VER reply, as it came back. The loader that answered is the one \
             that would perform the write. The gate compares the whole reply with the reply \
             pinned for the named SoC.",
        );
    });
    ui.end_row();

    ui.label("named SoC");
    match soc {
        Some(soc) if soc.matches(chip_version) => {
            ui.label(format!("{}: the loader's answer matches", soc.name()));
        }
        Some(soc) => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!(
                    "{}: the loader's answer does not match, and the write is refused",
                    soc.name()
                ),
            );
        }
        None => {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "none: the write is refused until one is named on the form",
            );
        }
    }
}

/// The line the partition table exists for.
///
/// "LBA 16384, 8192 sectors" is a fact about arithmetic, and a person cannot check
/// it against anything they know. "The whole of `uboot`" is a fact about their
/// board, which they can check. This line is therefore the one that stops a write
/// aimed at the wrong place.
fn touches(touches: &Touches, ui: &mut egui::Ui) {
    ui.vertical(|ui| match touches {
        Touches::NoTable => {
            ui.label("nothing that can be named: this board has no partition table");
            ui.weak("A write is planned against the flash's geometry alone.");
        }

        // Damage does not refuse the write -- writing a fresh table is how a
        // damaged one gets repaired -- so it says so, and lets the person decide.
        Touches::UnreadableTable { format, detail } => {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("unknown: the {format} on this board is damaged"),
            );
            ui.weak(detail.clone());
            prose(
                ui,
                "The partition map is missing, not empty. Writing a fresh table is how a damaged \
                 one is repaired, so this write is not refused.",
            );
        }

        Touches::Partitions(overlaps) if overlaps.is_empty() => {
            ui.label("no partition: these sectors lie outside every one of them");
            prose(
                ui,
                "On a Rockchip board, the bootloader is stored there. pyrographer cannot tell \
                 whether this is the range you meant or a write into a gap.",
            );
        }

        Touches::Partitions(overlaps) => {
            for overlap in overlaps {
                let extent = if overlap.is_whole() {
                    format!("the whole of it ({} sectors)", overlap.total)
                } else {
                    format!("{} of its {} sectors", overlap.covered, overlap.total)
                };
                ui.horizontal(|ui| {
                    ui.monospace(egui::RichText::new(&overlap.name).strong());
                    ui.label(extent);
                });
            }
        }
    });
}

/// The act that turns a plan into a write.
fn confirmation(app: &mut App, ui: &mut egui::Ui, now: f64) {
    // The gate, asked rather than tripped over. Core gives the same answer here
    // that the write itself would give a moment later, so nobody is asked to
    // confirm a write that was never going to happen -- which is the thing that
    // teaches somebody the gate is theater.
    if let Some(why) = app.session.pending.as_ref().and_then(|p| p.refused.clone()) {
        ui.colored_label(ui.visuals().error_fg_color, "This write is refused.");
        ui.label(why);
        ui.add_space(8.0);
        if ui.button("Close").clicked() {
            app.session.dismiss_plan();
        }
        return;
    }

    // The act itself, edited in place. Scoped, so the session is free again by
    // the time the buttons below need it.
    let mut repick = false;
    // What the thing being typed *is*, said in the words of the device it names.
    // A board's is an address on the bus and a disk's is a node the kernel gave
    // it; asking somebody to "type the address on the bus" of `/dev/sdb` would be
    // asking them to transcribe a phrase that does not describe what they are
    // looking at.
    let disk = matches!(app.session.target.device, Some(Chosen::Block(_)));
    let (what, hint) = if disk {
        ("the destination disk's node", "/dev/...")
    } else {
        ("the destination's address on the bus", "bus:address")
    };
    {
        let Some(pending) = app.session.pending.as_mut() else {
            return;
        };
        match &mut pending.confirmation {
            // Short, and different for every device -- so reflex cannot learn it,
            // and a clone with its source and destination the wrong way round asks
            // for a different string.
            Confirmation::Typed { expected, typed } => {
                // The sentence is the field's name as well as its instruction:
                // wired with `labelled_by`, so the coordinate to type is what a
                // screen reader announces on reaching the box rather than "text
                // input". This is the last control before flash is overwritten.
                let asked = ui.label(format!("To confirm, type {what}: {expected}"));
                // A format placeholder, not the answer. The expected coordinate is
                // named in the label above and on the plan's destination row; the
                // field itself is the deliberate act, and ghost text that spelled the
                // answer out inside the box would make it pure transcription.
                let field = ui
                    .add(
                        egui::TextEdit::singleline(typed)
                            .hint_text(hint)
                            .desired_width(160.0),
                    )
                    .labelled_by(asked.id);
                place_gate_focus(ui, Gate::Write, &field);
            }

            // On the web there is nothing short and stable to transcribe -- a
            // browser hands back a device object, not a place -- so the act is to
            // open a chooser the page did not draw and pick the destination out
            // of it a second time.
            Confirmation::Repicked { done } => {
                ui.label("To confirm, pick the destination again from the browser's chooser.");
                if *done {
                    ui.colored_label(
                        ui.visuals().hyperlink_color,
                        "Picked, and it is this board.",
                    );
                } else {
                    repick = ui.button("Pick the destination again...").clicked();
                    // See the recovery gate's counterpart: a refusal nobody can
                    // see is indistinguishable from a click that did nothing.
                    if app.repick_missed() {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "That is a different board. Pick the destination this plan names, or \
                             cancel and plan for the board you want.",
                        );
                    }
                }
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    if repick {
        app.repick();
    }
    let _ = repick;

    let can_confirm = app
        .session
        .pending
        .as_ref()
        .is_some_and(|pending| pending.can_confirm());

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            app.session.dismiss_plan();
            return;
        }

        let confirm = ui
            .add_enabled(
                can_confirm,
                egui::Button::new(egui::RichText::new("Overwrite the board").strong()),
            )
            // **The reason follows the device and the build**, because `what` is
            // already in hand and the hard-coded board wording is false twice
            // over: a disk has no address on a bus, and on the web the act is a
            // re-pick with nothing to type at all. `explain_disabled` is one of
            // the two seams that reaches a screen reader, so on a disk that
            // sentence was the *only* thing an assistive technology was told
            // about the control -- and it was about a bus.
            .explain_disabled(disabled_confirm_reason(app, what));

        if confirm.clicked() {
            app.confirm(now);
        }
    });
}

/// Why the confirm button is disabled, in the words of the act this build asks
/// for and the device it names.
///
/// There are three sentences, because there are three acts. A person types a
/// board's place on the bus, or types a disk's node path. In a browser, where a
/// device is an object and not a place, a person picks the destination from the
/// chooser a second time.
fn disabled_confirm_reason(app: &App, what: &str) -> String {
    let repick = matches!(
        app.session.pending.as_ref().map(|p| &p.confirmation),
        Some(Confirmation::Repicked { .. })
    );
    if repick {
        return "Pick the destination again from the browser's chooser. A browser returns a \
                device object rather than an address, so there is nothing to type. Only your \
                click can open the chooser, so the page cannot pick for you."
            .to_string();
    }
    format!(
        "Type {what}. It is different for every device, so a clone with its source and \
         destination swapped asks for a different string. This catches that mistake."
    )
}

/// The medium a recovery writes to, named for a person.
fn medium_name(target: RecoveryTarget) -> &'static str {
    match target {
        RecoveryTarget::NorFlash => "QSPI NOR flash",
        RecoveryTarget::Emmc => "eMMC",
    }
}

/// The recovery plan, as a screen.
///
/// This is the serial flow's counterpart of [`plan_screen`], held to the same
/// standard. It is the last thing read before a bootloader is overwritten, so it
/// takes the whole window and cannot be skipped. It says what a recovery writes
/// and where. It also states what sets it apart from every other write in
/// pyrographer: **the write is not read back**.
fn recovery_plan_screen(app: &mut App, ui: &mut egui::Ui) {
    let now = app.now();

    // Copied out, so the confirmation below can take the session mutably.
    let Some((plan, port)) = app
        .session
        .recovery
        .pending
        .as_ref()
        .map(|pending| (pending.plan.clone(), pending.port.clone()))
    else {
        return;
    };

    screen_header(
        ui,
        "This recovers a StarFive board",
        "Nothing here can be undone.",
        Tone::Danger,
    );

    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("recovery-plan")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("port");
                ui.monospace(&port);
                ui.end_row();

                ui.label("target");
                ui.monospace(medium_name(plan.target));
                ui.end_row();

                ui.label("agent");
                ui.monospace(format!(
                    "{} (uploaded into SRAM first)",
                    human_bytes(plan.agent_bytes)
                ));
                ui.end_row();

                for stage in &plan.stages {
                    ui.label(stage.kind.name());
                    ui.monospace(format!(
                        "{} (agent menu option {}, to {})",
                        human_bytes(stage.image_bytes),
                        stage.menu_option,
                        medium_name(plan.target),
                    ));
                    ui.end_row();
                }
            });

        // The one fact a StarFive recovery is owed that no other write is. It is on
        // this screen, and prominent, because a person deciding whether to say yes
        // is owed it at the moment they decide.
        if !plan.verified {
            ui.add_space(8.0);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                egui::RichText::new("This board's write is not read back.").strong(),
            );
            prose(
                ui,
                "The recovery protocol cannot read flash. Each transfer is confirmed only by the \
                 receiver's per-block acknowledgment. That proves the bytes were received, not \
                 that the flash holds them. A Rockchip write is verified, and this one cannot be.",
            );
        }

        ui.add_space(8.0);
        ui.separator();
        recovery_confirmation(app, ui, now);
    });
}

/// The act that turns a recovery plan into a recovery.
///
/// A recovery has no wrong-loader gate to refuse it, so this is the act alone. It
/// is simpler than [`confirmation`], which first asks whether the write would be
/// refused at all. Natively, the act is to type the serial port that the recovery
/// is written to. The USB gate has a person type a board's address on the bus in
/// the same way. In a tab, the act is a re-pick of the port. The USB gate re-picks
/// a board for the same reason: there is no path to transcribe.
fn recovery_confirmation(app: &mut App, ui: &mut egui::Ui, now: f64) {
    #[allow(unused_mut, unused_assignments)]
    let mut repick = false;

    // The act itself, edited in place. Scoped, so the session is free again by the
    // time the buttons below need it.
    {
        let Some(pending) = app.session.recovery.pending.as_mut() else {
            return;
        };
        match &mut pending.confirmation {
            Confirmation::Typed { expected, typed } => {
                let asked = ui.label(format!("To confirm, type the serial port: {expected}"));
                // **A format placeholder, not the answer**, exactly as the USB
                // gate's is. The port to type is named in the label above and on
                // the plan's own port row; ghost text that spelled it out inside
                // the box would make the act pure transcription -- and this is the
                // one write in pyrographer that is *not read back*, which makes it
                // the last place to weaken it.
                let field = ui
                    .add(
                        egui::TextEdit::singleline(typed)
                            .hint_text("/dev/ttyUSB0")
                            .desired_width(220.0),
                    )
                    .labelled_by(asked.id);
                place_gate_focus(ui, Gate::Recovery, &field);
            }
            // In a tab a port is an object a chooser handed back, not a place, so
            // there is nothing short and stable to transcribe. The act is to open
            // the chooser again and pick the same port out of it a second time.
            Confirmation::Repicked { done } => {
                ui.label("To confirm, pick the port again from the browser's chooser.");
                if *done {
                    ui.colored_label(ui.visuals().hyperlink_color, "Picked, and it is this port.");
                } else {
                    repick = ui.button("Pick the port again...").clicked();
                    // The refusal is correct and it used to be silent: a chooser
                    // that came back with a different port redrew the same button
                    // with no change of any kind, so a wrong pick and a click that
                    // did not register looked identical -- on the control that is
                    // the entire web gate. The native gate needs no counterpart:
                    // the typed string is visibly wrong.
                    if app.repick_missed() {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "That is not the port this plan is for. Pick the same one again, or \
                             cancel and plan for the one you want.",
                        );
                    }
                }
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    if repick {
        app.repick_port();
    }
    let _ = repick;

    let can_confirm = app
        .session
        .recovery
        .pending
        .as_ref()
        .is_some_and(|pending| pending.can_confirm());

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            app.session.dismiss_recovery_plan();
            return;
        }

        let confirm = ui
            .add_enabled(
                can_confirm,
                egui::Button::new(egui::RichText::new("Recover the board").strong()),
            )
            .explain_disabled(
                "Confirm the serial port to continue. It is the destination the recovery is \
                 written to. Type it here, or in a browser, pick it again from the chooser.",
            );

        if confirm.clicked() {
            app.recover(now);
        }
    });
}

/// The partition table, as a person acts on it: a name, a place, a length.
///
/// **The byte column needs a known sector size, and 512 is not a fallback.** A
/// table can be read without ever running `info`. A byte figure computed at a
/// guessed 512 is eight times too small on a 4Kn disk. It would appear on the
/// list a person reads to decide what to overwrite. [`Board::sector_size`] states
/// the rule: there is no sensible default. The sectors column says the same thing
/// without inventing anything.
///
/// [`Board::sector_size`]: crate::state::Board::sector_size
fn partitions(
    table: &PartitionTable,
    sector_size: Option<u32>,
    flash: Option<&FlashInfo>,
    ui: &mut egui::Ui,
) {
    // The backend's own sector size where there is an open agent to ask, and the
    // geometry the device reported where `info` has been run. Either is measured;
    // neither is assumed.
    let sector_size = sector_size
        .map(u64::from)
        .or_else(|| flash.map(|flash| u64::from(flash.sector_size)))
        .filter(|size| *size > 0);

    // A table recovered from the backup leads with why: the partitions are the
    // backup's, and the device's primary copy is damaged. A person about to write
    // to the board is owed that before the list.
    if let Some(recovery) = &table.recovery {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "The primary GPT is damaged. These partitions were recovered from {}.",
                recovery.recovered_from
            ),
        );
        ui.weak(format!("Primary GPT: {}", recovery.primary_detail));
    }

    egui::CollapsingHeader::new(format!(
        "{} ({} partitions)",
        table.format.name(),
        table.partitions.len()
    ))
    .default_open(true)
    .show(ui, |ui| {
        egui::Grid::new("partitions")
            .num_columns(4)
            .striped(true)
            .spacing([16.0, 2.0])
            .show(ui, |ui| {
                for partition in &table.partitions {
                    ui.monospace(&partition.name);
                    ui.monospace(partition.first_lba.to_string());
                    ui.monospace(format!("{} sectors", partition.sectors));
                    match sector_size {
                        Some(size) => {
                            ui.weak(human_bytes(partition.sectors.saturating_mul(size)));
                        }
                        None => {
                            ui.weak("").explain(
                                "The byte size depends on the sector size, which has not been \
                                 read from this device yet. Run Flash info.",
                            );
                        }
                    }
                    ui.end_row();
                }
            });
    });
}

/// A flash's size, in the units a person reads, and the medium it is a size of.
///
/// The medium is not decoration. Every LBA on the verb surface is an offset into
/// one medium. On a board with more than one medium populated, the same sector
/// number names more than one place. `verbs::info` reads the medium and the report
/// carries it. Without it, the size and the sector count do not identify a place.
///
/// The sector size is guarded. `block::list` reads it from sysfs, where a zero is
/// reachable, and a divide by zero on the frame thread crashes the window.
fn geometry(flash: &FlashInfo) -> String {
    let sectors = flash.size_bytes / u64::from(flash.sector_size.max(1));
    let medium = match flash.medium {
        Some(medium) => format!(" on {}", medium.name()),
        None => String::new(),
    };
    format!(
        "{} ({sectors} sectors of {} bytes){medium}",
        human_bytes(flash.size_bytes),
        flash.sector_size
    )
}

/// Bytes as spaced hex.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Bytes as printable ASCII, a dot standing in for everything else.
///
/// It gives one character per byte, so this column lines up with the hex beside it.
/// A chip identifier that happens to be text then shows up in one of the two.
fn ascii(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// A byte count the way a person reads one.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyrographer_core::agent::FlashInfo;
    use pyrographer_core::agent::ReadBack;
    use pyrographer_core::agent::{DfuAgent, FlashAgent};
    use pyrographer_core::partition::{Overlap, TableFormat};
    use pyrographer_core::recovery::RecoveryTarget;
    use pyrographer_core::soc::Soc;
    use pyrographer_core::transport::testing::ScriptedTransport;
    use pyrographer_core::verbs::{ClonePlan, Segment, SegmentedPlan, TableAction, WritePlan};

    use crate::platform::PickedBlob;
    use crate::state::{ConfirmBy, Confirmation, Pending, Plan};

    /// Draw a frame, and read back every word of it.
    ///
    /// egui is pure: a `Context` runs a frame and lays out real text with no
    /// window, no GPU and no event loop. A test can therefore read the screen that
    /// matters most here. The plan screen exists to show a person four or five
    /// specific things before they overwrite a board. A layer this thin has little
    /// else worth pinning, and this function pins that part.
    fn words(app: &mut App) -> String {
        // `run_ui` hands back the same root `Ui` that `eframe::App::ui` is given,
        // so what this draws is what the window draws, through the same call.
        drawn(|ui| draw(app, ui))
    }

    /// Draw a frame with one flow chosen, and read back every word of it.
    ///
    /// The window splits by what answers, so a test about the serial side must
    /// name the side it means. It does so as a person does, by choosing the tab. A
    /// test that did not would read the other flow's screen and report the absence
    /// as a defect.
    fn words_on(app: &mut App, tab: Tab) -> String {
        app.tab = tab;
        words(app)
    }

    /// Every string egui laid out, in the order it laid them out.
    fn read(shape: &egui::Shape, into: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                into.push_str(text.galley.text());
                into.push('\n');
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    read(shape, into);
                }
            }
            _ => {}
        }
    }

    /// The accessibility tree a screen reader would walk, built with no window.
    ///
    /// eframe passes egui's `TreeUpdate` to AccessKit, which passes it to the
    /// platform: AT-SPI on Linux, and Orca from there. None of that is needed to
    /// see what would be passed. [`egui::Context::enable_accesskit`] turns the
    /// tree on, and a headless frame then carries a complete one in its
    /// `platform_output`. What a screen reader would be told is therefore a value
    /// this crate can assert on, in the same shape as [`drawn`]. It needs no
    /// window, no hardware and no assistive technology installed.
    ///
    /// This keeps the labeling honest. A missing name is invisible in a
    /// screenshot, and in a `cargo test` that only reads text shapes. Here it is a
    /// `None`.
    fn tree(mut f: impl FnMut(&mut egui::Ui)) -> Tree {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        ctx.enable_accesskit();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 4000.0),
            )),
            ..Default::default()
        };
        // Twice, for the reason `drawn` draws twice.
        let mut first = ctx.run_ui(input.clone(), |ui| f(ui));
        first.textures_delta.clear();
        let mut output = ctx.run_ui(input, |ui| f(ui));
        output.textures_delta.clear();

        Tree(
            output
                .platform_output
                .accesskit_update
                .expect("accesskit was enabled, so the frame carries a tree")
                .nodes
                .into_iter()
                .collect(),
        )
    }

    /// One frame's accessibility tree, with the node ids kept.
    ///
    /// The tests depend on the ids, because a name is not always a field on the
    /// node that carries it. A `labelled_by` relation points at another node, and
    /// an assistive technology composes what it announces from both. A tree without
    /// the ids could not answer the question these tests ask: what a node is
    /// announced as.
    struct Tree(Vec<(egui::accesskit::NodeId, egui::accesskit::Node)>);

    impl Tree {
        /// Every node a person can operate.
        ///
        /// Nodes are selected by role, not by whether they accept a click. An egui
        /// `Label` senses clicks so that text can be dragged and selected.
        /// Selecting on "supports Click" would therefore include every piece of
        /// prose on the screen. These are the roles egui gives the widgets a person
        /// operates, which is the set that needs a name.
        fn controls(&self) -> Vec<&egui::accesskit::Node> {
            use egui::accesskit::Role;
            self.0
                .iter()
                .map(|(_, node)| node)
                .filter(|node| {
                    matches!(
                        node.role(),
                        Role::Button
                            | Role::CheckBox
                            | Role::ComboBox
                            | Role::Link
                            | Role::MultilineTextInput
                            | Role::RadioButton
                            | Role::Slider
                            | Role::SpinButton
                            | Role::TextInput
                    )
                })
                .collect()
        }

        fn node(&self, id: egui::accesskit::NodeId) -> Option<&egui::accesskit::Node> {
            self.0
                .iter()
                .find(|(other, _)| *other == id)
                .map(|(_, n)| n)
        }

        /// What an assistive technology would announce for a node.
        ///
        /// It is composed the way `accesskit_consumer::write_label` composes it.
        /// An assertion on any other value would test something no screen reader
        /// sees. If the node has a direct label, that label wins. When it has none,
        /// the `labelled_by` targets are read instead. For a target whose label comes
        /// from its value, such as a `Role::Label` node, the value is taken. That
        /// is why the relation works on a field and would be inert on a button.
        fn announced(&self, node: &egui::accesskit::Node) -> Option<String> {
            if let Some(label) = node.label() {
                return Some(label.to_string());
            }
            let composed: Vec<String> = node
                .labelled_by()
                .iter()
                .filter_map(|id| self.node(*id))
                .filter_map(|target| {
                    if target.role() == egui::accesskit::Role::Label {
                        target.value().map(str::to_string)
                    } else {
                        target.label().map(str::to_string)
                    }
                })
                .collect();
            if composed.is_empty() {
                // A field with neither is genuinely nameless. Its own value is not
                // a name: "115200" does not say what it counts.
                None
            } else {
                Some(composed.join(" "))
            }
        }
    }

    /// Press a control by the name an assistive technology would announce, and
    /// read back the screen afterwards.
    ///
    /// The control is found in the accessibility tree, which holds both its bounds
    /// and its name, and then clicked where it is drawn. This exercises the same
    /// route a person takes. It finds only a control that has a name, which is the
    /// property the rest of these tests check.
    fn after_clicking(mut f: impl FnMut(&mut egui::Ui), name: &str) -> String {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        ctx.enable_accesskit();
        let base = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 4000.0),
            )),
            ..Default::default()
        };

        let mut first = ctx.run_ui(base.clone(), |ui| f(ui));
        first.textures_delta.clear();
        let mut located = ctx.run_ui(base.clone(), |ui| f(ui));
        located.textures_delta.clear();

        let update = located
            .platform_output
            .accesskit_update
            .expect("accesskit was enabled");
        let bounds = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label()
                    .or_else(|| node.value())
                    .is_some_and(|it| it == name)
            })
            .and_then(|(_, node)| node.bounds())
            .unwrap_or_else(|| panic!("no control is named {name:?}"));
        let at = egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );

        let mut click = base.clone();
        click.events = vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let mut clicked = ctx.run_ui(click, |ui| f(ui));
        clicked.textures_delta.clear();
        let mut settled = ctx.run_ui(base, |ui| f(ui));
        settled.textures_delta.clear();

        let mut text = String::new();
        for shape in &settled.shapes {
            read(&shape.shape, &mut text);
        }
        text
    }

    /// Draw an arbitrary closure into a headless frame, and read back every word it
    /// laid out.
    ///
    /// It draws twice, for the same reason [`words`] does.
    fn drawn(mut f: impl FnMut(&mut egui::Ui)) -> String {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 2000.0),
            )),
            ..Default::default()
        };

        // Twice: egui sizes some things against what the last frame measured, and
        // a first frame lays out before it knows how big anything is.
        //
        // A frame's `textures_delta` is the font atlas egui just rasterized, and
        // epaint asserts in `Drop` that a renderer took it. Nothing renders here,
        // so each frame's is cleared by hand -- the headless counterpart of the
        // upload a window would do.
        let mut first = ctx.run_ui(input.clone(), |ui| f(ui));
        first.textures_delta.clear();
        let mut output = ctx.run_ui(input, |ui| f(ui));
        output.textures_delta.clear();

        let mut text = String::new();
        for clipped in &output.shapes {
            read(&clipped.shape, &mut text);
        }
        text
    }

    /// Every run of text a frame laid out, with the color it was drawn in.
    ///
    /// If the shape has an override, the color comes from it. Otherwise it comes
    /// from the layout section's format, which is the order epaint resolves it in.
    /// The color read here is therefore the one that reaches the screen, not a
    /// color the source happens to mention.
    fn colored(mut f: impl FnMut(&mut egui::Ui)) -> Vec<(String, egui::Color32)> {
        fn scan(shape: &egui::Shape, into: &mut Vec<(String, egui::Color32)>) {
            match shape {
                egui::Shape::Text(text) => {
                    let color = text.override_text_color.unwrap_or_else(|| {
                        text.galley
                            .job
                            .sections
                            .first()
                            .map_or(text.fallback_color, |section| section.format.color)
                    });
                    into.push((text.galley.text().to_string(), color));
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        scan(shape, into);
                    }
                }
                _ => {}
            }
        }

        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 4000.0),
            )),
            ..Default::default()
        };
        let mut first = ctx.run_ui(input.clone(), |ui| f(ui));
        first.textures_delta.clear();
        let mut output = ctx.run_ui(input, |ui| f(ui));
        output.textures_delta.clear();

        let mut runs = Vec::new();
        for clipped in &output.shapes {
            scan(&clipped.shape, &mut runs);
        }
        runs
    }

    /// Draw one job's report into a headless frame and read its words back.
    fn drawn_report(report: &Report) -> String {
        drawn(|ui| success(report, ui))
    }

    /// A dump that came back as constant fill draws a caution, not a bare
    /// "Read N bytes".
    ///
    /// The finding is built through the real scanner, so this pins the whole path
    /// from a constant-fill read to the words a person sees.
    #[test]
    fn a_dump_report_with_constant_fill_draws_a_caution() {
        let mut scanner = pyrographer_core::fill::FillScanner::new(1 << 20);
        let poison = vec![0xcc; 1 << 20]; // one 1 MiB sector of it
        scanner.observe(65536, &poison);
        let fill = scanner.finish();
        assert!(fill.has_suspicious());

        let words = drawn_report(&Report::Dumped {
            bytes: 1 << 20,
            name: "backup.img".to_string(),
            fill,
        });

        assert!(words.contains("0xcc"), "the fill byte is named: {words}");
        assert!(
            words.contains("silent read failure"),
            "the caution is drawn: {words}"
        );
    }

    /// An Ingenic bootstrap report shows the SoC magic raw, as both bytes and text.
    ///
    /// Reading it from a board produces the bench record that pins the write gate,
    /// so the report must not hide or interpret it.
    #[test]
    fn an_ingenic_bootstrap_report_shows_the_soc_magic_raw() {
        let cpu = pyrographer_core::codec::ingenic_boot::CpuInfo {
            magic: *b"T31\0\0\0\0\0",
        };
        let words = drawn_report(&Report::IngenicBootstrapped(cpu));

        assert!(
            words.contains("54 33 31"),
            "the magic bytes are shown raw: {words}"
        );
        assert!(
            words.contains("T31"),
            "and rendered as text for a person: {words}"
        );
        assert!(
            words.to_lowercase().contains("record"),
            "and the person is told to record it: {words}"
        );
    }

    /// Each mode's report says what that mode did.
    ///
    /// A single "The board is rebooting." for a board that was asked to power off
    /// would misreport what the board did. The sentence therefore comes from the
    /// mode.
    #[test]
    fn a_reset_report_says_which_ending_the_board_took() {
        for mode in ResetMode::ALL {
            let words = drawn_report(&Report::Reset { mode });
            assert_eq!(
                words.trim(),
                mode.outcome(),
                "{} draws its own sentence",
                mode.name()
            );
        }

        assert!(
            drawn_report(&Report::Reset {
                mode: ResetMode::PowerOff
            })
            .contains("powering off")
        );
    }

    /// The button names the ending it will ask for.
    ///
    /// A button that said "Reboot" and powered the board off is the mistake this
    /// surface exists to prevent.
    #[test]
    fn the_reset_button_names_the_ending_it_will_ask_for() {
        for mode in ResetMode::ALL {
            let mut app = an_app();
            app.form.reset_mode = mode;

            let words = drawn(|ui| verbs_panel(&mut app, ui));
            assert!(
                words.contains(mode.describe()),
                "{} labels the button: {words}",
                mode.name()
            );
        }
    }

    /// The plan screen says when the write is checked.
    ///
    /// It says it differently for a board that can be checked only after
    /// committing. A person deciding whether to overwrite a board needs the
    /// difference before typing the coordinate.
    #[test]
    fn the_plan_screen_says_when_the_write_would_be_checked() {
        let app = an_app();
        let mut plan = a_plan();

        plan.read_back = ReadBack::PerWindow;
        let words = drawn(|ui| write_plan(&app, &plan, ui));
        assert!(
            words.contains("stops the write at that window"),
            "a per-window board: {words}"
        );

        plan.read_back = ReadBack::AfterCommit;
        let words = drawn(|ui| write_plan(&app, &plan, ui));
        assert!(
            words.contains("can be read back only after the write is committed"),
            "a commit-first board: {words}"
        );
        assert!(
            words.contains("already written"),
            "and what that costs: {words}"
        );
    }

    /// The caution for an untried mode is drawn, not hidden behind a hover.
    ///
    /// The plain reboot, which a board has answered, draws none. A caution on every
    /// mode would teach a person to ignore it.
    #[test]
    fn an_untried_reset_mode_draws_its_caution_and_a_tried_one_does_not() {
        let mut app = an_app();
        app.form.reset_mode = ResetMode::Reset;
        let words = drawn(|ui| verbs_panel(&mut app, ui));
        assert!(
            !words.contains("No board has answered"),
            "the plain reboot has met hardware: {words}"
        );

        for mode in ResetMode::ALL.iter().filter(|mode| mode.untried()) {
            let mut app = an_app();
            app.form.reset_mode = *mode;

            let words = drawn(|ui| verbs_panel(&mut app, ui));
            assert!(
                words.contains("No board has answered"),
                "{} is cautioned: {words}",
                mode.name()
            );
            assert!(
                words.contains("writes no flash"),
                "{} says what it does not do: {words}",
                mode.name()
            );
        }
    }

    fn a_device(address: u8) -> DeviceInfo {
        DeviceInfo {
            vendor: pyrographer_core::discovery::Vendor::Rockchip,
            vendor_id: 0x2207,
            product_id: 0x350e,
            bcd_usb: 0x0201,
            mode: Mode::Loader,
            bus_id: "003".to_string(),
            device_address: address,
        }
    }

    /// An app with nothing open.
    fn an_app() -> App {
        App::new(egui::Context::default())
    }

    /// A disk, as a listing would report one: not the running system, not
    /// mounted, and therefore openable.
    fn a_disk(node: &str, bytes: u64) -> BlockDevice {
        BlockDevice {
            name: node.trim_start_matches("/dev/").to_string(),
            node: node.to_string(),
            bytes,
            logical_block: 512,
            physical_block: 512,
            removable: true,
            read_only: false,
            bus: pyrographer_core::block::Bus::Usb,
            model: Some("Generic  SD/MMC".to_string()),
            mounts: Vec::new(),
            carries_running_system: false,
        }
    }

    /// A write into the whole of `uboot` and a hundred sectors of `trust` after it.
    ///
    /// That is the shape of the mistake this screen exists to catch.
    fn a_plan() -> WritePlan {
        WritePlan {
            lba: 16384,
            image_bytes: 4_194_304,
            padding_bytes: 0,
            sectors: 8192,
            flash: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: vec![0x38, 0x38, 0x35, 0x33],
            soc: None,
            touches: Touches::Partitions(vec![
                Overlap {
                    name: "uboot".to_string(),
                    first_lba: 16384,
                    covered: 8192,
                    total: 8192,
                },
                Overlap {
                    name: "trust".to_string(),
                    first_lba: 24576,
                    covered: 100,
                    total: 8192,
                },
            ]),
            read_back: ReadBack::PerWindow,
        }
    }

    /// A repair of a damaged primary GPT, restoring a Rockchip-shaped table.
    ///
    /// The loader answers as the SoC the plan names, so the gate's verdict is a
    /// match. Typing the coordinate is the only step left.
    fn a_repair_plan() -> SegmentedPlan {
        SegmentedPlan {
            format: TableFormat::Gpt,
            action: TableAction::Repair {
                source: "the backup GPT in the device's last sector".to_string(),
            },
            partitions: vec![
                Partition {
                    name: "uboot".to_string(),
                    first_lba: 16384,
                    sectors: 8192,
                },
                Partition {
                    name: "trust".to_string(),
                    first_lba: 24576,
                    sectors: 8192,
                },
            ],
            flash: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: vec![0x36, 0x37, 0x35, 0x33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            soc: Some(Soc::parse("rk3576").expect("pinned")),
            read_back: ReadBack::PerWindow,
            segments: vec![Segment {
                what: "the primary GPT (sector 1 onward)".to_string(),
                lba: 1,
                bytes: vec![0u8; 16896],
                touches: Touches::Partitions(Vec::new()),
            }],
        }
    }

    /// A row leads with the board's place on the bus.
    ///
    /// A place tells two identical boards apart, and it is what a person types
    /// before a write.
    #[test]
    fn a_device_row_leads_with_its_place() {
        let mut app = an_app();
        app.session.devices_seen(vec![a_device(12)], 0.0);

        let text = drawn(|ui| device_rows(&mut app, ui));
        assert!(text.contains("003:12"), "{text}");
        assert!(text.contains("2207:350e"), "{text}");
    }

    /// A listing with no place to name drops the column instead of drawing it
    /// empty, and says everything else a row says.
    ///
    /// This is the browser's shape, drawn natively. WebUSB returns a device object
    /// instead of a place, so `state::coordinate` is empty there. A tab draws its
    /// rows with this same function, so a headless frame can pin them without a
    /// browser. Only a browser can settle whether `getDevices` answers at all.
    #[test]
    fn a_device_row_with_no_place_drops_the_column() {
        let mut app = an_app();
        let mut device = a_device(12);
        device.bus_id = String::new();
        device.device_address = 0;
        app.session.devices_seen(vec![device], 0.0);

        let text = drawn(|ui| device_rows(&mut app, ui));
        assert!(!text.contains("003"), "{text}");
        assert!(text.contains("2207:350e"), "{text}");
        assert!(text.contains("loader"), "{text}");
        assert!(text.contains("Use"), "{text}");
        assert!(text.contains("Clone from"), "{text}");
    }

    /// A board pyrographer can see and cannot drive is listed, and refused at the
    /// open.
    ///
    /// A missing row would tell a person nothing about the board.
    #[test]
    fn a_maskrom_board_is_listed_rather_than_hidden() {
        let mut app = an_app();
        let mut maskrom = a_device(14);
        maskrom.mode = Mode::Maskrom;
        maskrom.bcd_usb = 0x0200; // even: the BootROM is running

        app.session.devices_seen(vec![a_device(12), maskrom], 0.0);

        let words = words(&mut app);
        assert!(words.contains("003:12"), "the loader board: {words}");
        assert!(words.contains("003:14"), "and the maskrom one: {words}");
        assert!(words.contains("maskrom"), "named for what it is: {words}");
    }

    /// A maskrom-flagged board, once selected, offers both actions.
    ///
    /// The flag can be true: a BootROM whose flash is out of reach until a loader
    /// is uploaded. It can also come from a loader that keeps the flag even, as
    /// the RK3576 SPL does. The panel therefore offers the upload and an `Open`
    /// that probes the claim, and the board answers for itself.
    #[test]
    fn a_selected_maskrom_board_offers_upload_and_open() {
        let mut app = an_app();
        let mut maskrom = a_device(14);
        maskrom.mode = Mode::Maskrom;
        maskrom.bcd_usb = 0x0200;

        app.session.devices_seen(vec![maskrom.clone()], 0.0);
        app.session.select_target(maskrom.clone(), maskrom);

        let words = words(&mut app);
        assert!(
            words.contains("Upload loader"),
            "the selected maskrom board offers the download-boot: {words}"
        );
        assert!(
            words.contains("Open"),
            "and the open that probes the flag's claim: {words}"
        );
    }

    /// The plan screen shows everything a person needs before flash is
    /// overwritten.
    ///
    /// It shows these items:
    ///
    /// - Which board
    /// - Where the bytes land
    /// - Which partitions they land in, and how much of each
    /// - The loader's own account of itself
    /// - The coordinate to type to go ahead
    #[test]
    fn the_plan_screen_shows_everything_a_person_needs_to_refuse_it() {
        let mut app = an_app();
        app.session.devices_seen(vec![a_device(12)], 0.0);
        app.session.select_target(a_device(12), a_device(12));
        app.session.pending = Some(Pending {
            plan: Plan::Write(a_plan()),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(12))),
            refused: None,
        });

        let words = words(&mut app);

        assert!(words.contains("This overwrites flash"), "{words}");
        assert!(words.contains("Nothing here can be undone"), "{words}");
        assert!(words.contains("2207:350e"), "which board: {words}");

        // The line the partition table exists for. "Sector 16384" is a fact about
        // arithmetic; "the whole of uboot" is a fact about their board.
        assert!(words.contains("uboot"), "what it destroys: {words}");
        assert!(
            words.contains("the whole of it (8192 sectors)"),
            "how much of it: {words}"
        );
        assert!(words.contains("trust"), "and what it runs into: {words}");
        assert!(
            words.contains("100 of its 8192 sectors"),
            "and how far into it: {words}"
        );

        assert!(
            words.contains("16384 through 24575"),
            "where it lands: {words}"
        );
        assert!(
            words.contains("38 38 35 33") && words.contains("8853"),
            "which loader is answering, raw: {words}"
        );
        assert!(
            words.contains("type the destination's address on the bus: 003:12"),
            "and the act that has to be performed: {words}"
        );
    }

    /// The repair plan screen shows what it rewrites and from what.
    ///
    /// A repair is a write, so it goes through the same screen with the same typed
    /// coordinate. It names the copy being overwritten, the intact copy it rebuilds
    /// from, and the partitions the recovered table restores. A person recognizes
    /// the table by those partitions.
    #[test]
    fn the_repair_plan_screen_shows_what_it_rewrites_and_restores() {
        let mut app = an_app();
        app.session.devices_seen(vec![a_device(12)], 0.0);
        app.session.select_target(a_device(12), a_device(12));
        app.session.pending = Some(Pending {
            plan: Plan::Table(a_repair_plan()),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(12))),
            refused: None,
        });

        let words = words(&mut app);

        assert!(words.contains("This overwrites flash"), "{words}");
        assert!(words.contains("2207:350e"), "which board: {words}");
        assert!(
            words.contains("primary GPT"),
            "the copy it rewrites: {words}"
        );
        assert!(
            words.contains("backup GPT in the device's last sector"),
            "the copy it rebuilds from: {words}"
        );
        assert!(
            words.contains("uboot") && words.contains("trust"),
            "what it restores: {words}"
        );
        assert!(words.contains("1 through 33"), "where it lands: {words}");
        assert!(
            words.contains("36 37 35 33") && words.contains("6753"),
            "the loader's own account of itself, raw: {words}"
        );
        assert!(
            words.contains("rk3576: the loader's answer matches"),
            "the gate's verdict: {words}"
        );
        assert!(
            words.contains("type the destination's address on the bus: 003:12"),
            "and the act that has to be performed: {words}"
        );
    }

    /// The clone plan screen names both boards: the one copied and the one
    /// overwritten.
    ///
    /// The mistake it exists to catch is the source and destination swapped. The
    /// confirmation is typed against the destination, the board that is destroyed.
    #[test]
    fn the_clone_plan_screen_names_both_boards() {
        let mut app = an_app();
        app.session
            .devices_seen(vec![a_device(12), a_device(14)], 0.0);
        app.session.select_source(a_device(12), a_device(12)); // copied
        app.session.select_target(a_device(14), a_device(14)); // overwritten
        app.session.pending = Some(Pending {
            plan: Plan::Clone(ClonePlan {
                source: FlashInfo {
                    size_bytes: 122_142_720 * 512,
                    sector_size: 512,
                    medium: None,
                    chip_id: None,
                },
                destination: a_plan(),
            }),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(14))),
            refused: None,
        });

        let words = words(&mut app);
        assert!(words.contains("This overwrites flash"), "{words}");
        assert!(words.contains("copied"), "the source row is drawn: {words}");
        assert!(
            words.contains("overwritten"),
            "and the destination row: {words}"
        );
        assert!(words.contains("003:12"), "the board being copied: {words}");
        assert!(
            words.contains("003:14"),
            "the board being overwritten: {words}"
        );
        assert!(
            words.contains("type the destination's address on the bus: 003:14"),
            "and the confirmation is against the destination, not the source: {words}"
        );
    }

    /// *Clone from* is reachable, and the board it names can be opened.
    ///
    /// The clone flow depends on a panel for the source board. If `ui::board` is
    /// called only for the target, `session.source.connection` never leaves
    /// `Disconnected`. "Plan a clone..." then stays grayed, with a hover describing
    /// a state the window offers no way to reach. Pressing *Clone from* appears to
    /// do nothing.
    ///
    /// This test presses the button by the name an assistive technology would
    /// announce. It then checks for a panel for the board being copied, with an
    /// `Open` button on it.
    #[test]
    fn clone_from_draws_the_source_board_and_offers_to_open_it() {
        let mut app = an_app();
        app.session
            .devices_seen(vec![a_device(12), a_device(14)], 0.0);
        app.session.select_target(a_device(14), a_device(14));

        let before = words(&mut app);
        assert!(
            !before.contains("Board to copy"),
            "nothing is being copied yet, so there is no panel: {before}"
        );

        let after = after_clicking(|ui| draw(&mut app, ui), "Clone from 003:12");
        assert!(
            after.contains("Board to copy"),
            "the source board gets a panel of its own: {after}"
        );
        assert!(
            after.contains("003:12"),
            "and it names the board being copied: {after}"
        );

        // The control the flow was missing. `connection_controls` always handled
        // `Which` correctly; nothing drew it for the source.
        let controls = tree(|ui| draw(&mut app, ui));
        let names: Vec<String> = controls
            .controls()
            .iter()
            .filter_map(|node| controls.announced(node))
            .collect();
        assert!(
            names.iter().any(|name| name == "Open"),
            "the board being copied can be opened: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == "Forget the device to copy"),
            "and the slot it fills can be emptied again: {names:?}"
        );
    }

    /// A running job's Cancel survives a plan screen, and the other plan waiting
    /// behind the gate is named.
    ///
    /// The gate takes the whole window, but the job strip is not a way out of the
    /// gate. The strip is the only control a person has over a job already under
    /// way. A recovery started before the plan appeared keeps running behind it,
    /// and stopping it between blocks is all that can be done. If the strip were
    /// drawn from `main_screen`, it would leave the screen as soon as a plan
    /// arrived.
    ///
    /// The second half covers the same kind of defect from the other side. Two
    /// gates can be waiting, and only one is drawn. The one not drawn is named, so
    /// it does not appear without warning after the first is dismissed.
    ///
    /// The recovery is driven over a scripted serial, so a real job can be in
    /// flight with no port and no board.
    #[test]
    fn a_plan_screen_hides_neither_a_running_job_nor_a_second_plan() {
        use pyrographer_core::transport::testing::ScriptedSerial;

        let mut app = an_app();
        app.session
            .set_recovery_agent(Some(a_recovery_blob("agent.bin", 4096)));
        app.session
            .set_recovery_spl(Some(a_recovery_blob("spl.bin", 2048)));
        app.session
            .plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash)
            .expect("a plan");

        // Say yes to it and start it, so a real job is in flight. The scripted
        // serial answers nothing, which is fine: the job is running either way,
        // and running is the whole of what this is about.
        let (_, request, confirmed) = {
            if let Confirmation::Typed { expected, typed } = &mut app
                .session
                .recovery
                .pending
                .as_mut()
                .expect("a plan")
                .confirmation
            {
                *typed = expected.clone();
            }
            app.session.confirm_recovery().expect("it is confirmed")
        };
        let work = app
            .session
            .start_recovery(
                ScriptedSerial::new(Vec::new()),
                request,
                confirmed,
                0.0,
                Box::new(|| {}),
            )
            .expect("nothing else is running");
        assert!(app.session.recovery.job.is_some(), "a job is in flight");

        // A second recovery plan behind it, and a write plan over the top.
        app.session
            .plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash)
            .expect("a plan");
        app.session.devices_seen(vec![a_device(12)], 0.0);
        app.session.select_target(a_device(12), a_device(12));
        app.session.pending = Some(Pending {
            plan: Plan::Write(a_plan()),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(12))),
            refused: None,
        });

        let words = words(&mut app);
        assert!(
            words.contains("This overwrites flash"),
            "the gate is up: {words}"
        );
        assert!(
            words.contains("Recovering over serial"),
            "and the running job is still reported: {words}"
        );
        assert!(
            words.contains("recovery plan is also waiting"),
            "as is the plan that is not on the screen: {words}"
        );

        let tree = tree(|ui| draw(&mut app, ui));
        let names: Vec<String> = tree
            .controls()
            .iter()
            .filter_map(|node| tree.announced(node))
            .collect();
        assert!(
            names
                .iter()
                .any(|name| name == "Cancel Recovering over serial"),
            "and its Cancel is still reachable: {names:?}"
        );

        drop(work);
    }

    /// The partition list does not guess a sector size.
    ///
    /// A table can be read without `Flash info` ever being run. A byte figure
    /// computed at an assumed 512 is eight times too small on a 4Kn disk. It would
    /// appear on the list a person reads to decide what to overwrite. The sectors
    /// column says the same thing without inventing anything, so the byte column
    /// waits for a measured size.
    #[test]
    fn the_partition_list_omits_bytes_until_the_sector_size_is_known() {
        let table = PartitionTable {
            format: TableFormat::Gpt,
            partitions: vec![Partition {
                name: "uboot".to_string(),
                first_lba: 16384,
                sectors: 8192,
            }],
            recovery: None,
        };

        let guessed = drawn(|ui| partitions(&table, None, None, ui));
        assert!(
            guessed.contains("8192 sectors"),
            "the sectors are always drawn: {guessed}"
        );
        assert!(
            !guessed.contains("MiB"),
            "and no byte figure is invented from a sector size nothing read: {guessed}"
        );

        let known = drawn(|ui| partitions(&table, Some(4096), None, ui));
        assert!(
            known.contains("32.00 MiB"),
            "with a measured 4Kn geometry the bytes are the device's: {known}"
        );
    }

    /// The reason the confirm button is disabled follows the device.
    ///
    /// The reason is one of the two seams that reach a screen reader. With a
    /// hard-coded board wording, all an assistive technology learns about the
    /// control on a disk is an address on a bus. A disk has no such address.
    #[test]
    fn the_disabled_confirm_reason_names_what_this_device_asks_for() {
        for (device, expect) in [
            (Chosen::Usb(a_device(12)), "address on the bus"),
            (Chosen::Block(a_disk("/dev/sdb", 1 << 30)), "disk's node"),
        ] {
            let mut app = an_app();
            match &device {
                Chosen::Usb(usb) => app.session.select_target(usb.clone(), usb.clone()),
                Chosen::Block(disk) => app.session.select_target_disk(disk.clone()),
            };
            app.session.pending = Some(Pending {
                plan: Plan::Write(a_plan()),
                confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &device),
                refused: None,
            });

            let tree = tree(|ui| draw(&mut app, ui));
            let reasons: Vec<String> = tree
                .0
                .iter()
                .filter_map(|(_, node)| node.description().map(str::to_string))
                .collect();
            assert!(
                reasons.iter().any(|reason| reason.contains(expect)),
                "a {device:?} is asked for {expect:?}: {reasons:?}"
            );
        }
    }

    /// An empty device list covers every vendor, and points at the disks.
    ///
    /// The scan sweeps Rockchip and Ingenic. A person with only a card reader
    /// plugged in has found no board, and is one closed section away from what
    /// they came for. The CLI says the same at the same moment.
    #[test]
    fn an_empty_device_list_names_no_vendor_and_points_at_the_disks() {
        let mut app = an_app();
        app.session.devices_seen(Vec::new(), 0.0);

        let words = words_on(&mut app, Tab::Flash);
        assert!(
            !words.contains("No Rockchip board"),
            "the scan covers more than one vendor: {words}"
        );
        assert!(
            words.contains("No board in a boot or recovery mode"),
            "so the sentence names none of them: {words}"
        );
        assert!(
            words.contains("disks are under Disks"),
            "and the card reader somebody is looking for is pointed at: {words}"
        );
    }

    /// The maskrom gate's one visible act: the container's claim, rendered.
    ///
    /// With a single SoC pinned, the gate refuses only a file that claims another
    /// SoC while `rk3576` is named. For every other upload, the gate's effect is to
    /// make the claim readable. If the window renders the claim nowhere, its maskrom
    /// gate does nothing observable there, while the CLI's gate still does its job.
    ///
    /// The other half covers the bare stages. They carry no container and so name
    /// no SoC, and saying nothing there would read as a check that passed.
    #[test]
    fn the_maskrom_upload_shows_what_the_file_claims_about_itself() {
        // The RK3576 container field, which is also the bytes an RK3576 loader
        // answers `chipver` with -- an agreement that is a measurement, pinned in
        // two fields for that reason.
        let claimed = drawn_report(&Report::Bootstrapped {
            chip: Some(vec![0x36, 0x37, 0x35, 0x33]),
        });
        assert!(
            claimed.contains("36 37 35 33"),
            "the claim is shown raw: {claimed}"
        );
        assert!(
            claimed.contains("6753") && claimed.contains("rk3576"),
            "and resolved to the SoC pinned for it: {claimed}"
        );

        let bare = drawn_report(&Report::Bootstrapped { chip: None });
        assert!(
            bare.contains("name no SoC"),
            "bare stages say there was nothing to check: {bare}"
        );
    }

    /// A maskrom board can be sent bare 471/472 stages, not only a container.
    ///
    /// `db --code471/--code472` is the path RAM-booting is built on. It is how a
    /// mainline U-Boot is loaded into a maskrom board's DRAM. The loader picker
    /// parses what it is handed and refuses anything that is not an RKBOOT
    /// container. The window therefore needs a second form for this path.
    #[test]
    fn a_maskrom_board_is_offered_the_raw_stages_as_well_as_a_container() {
        let mut app = an_app();
        let mut maskrom = a_device(12);
        maskrom.mode = Mode::Maskrom;
        maskrom.bcd_usb = 0x0200;
        app.session.devices_seen(vec![maskrom.clone()], 0.0);
        app.session.select_target(maskrom.clone(), maskrom);

        let shut = words_on(&mut app, Tab::Flash);
        assert!(
            shut.contains("Upload loader..."),
            "the container path is offered: {shut}"
        );
        assert!(
            shut.contains("Raw stages..."),
            "and so is the other one: {shut}"
        );

        app.maskrom.show = true;
        let open = words_on(&mut app, Tab::Flash);
        assert!(
            open.contains("471 (DRAM init)") && open.contains("472 (loader)"),
            "the form takes both stages: {open}"
        );
        assert!(
            open.contains("name no SoC"),
            "and says these files claim nothing, so nothing checks them: {open}"
        );
    }

    /// The gate, on the screen.
    ///
    /// A write the backend will refuse says so, and does not ask for a
    /// confirmation it would then throw away.
    #[test]
    fn a_refused_write_says_so_rather_than_asking_to_be_confirmed() {
        let mut app = an_app();
        app.session.devices_seen(vec![a_device(12)], 0.0);
        app.session.select_target(a_device(12), a_device(12));
        app.session.pending = Some(Pending {
            plan: Plan::Write(a_plan()),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(12))),
            refused: Some("the loader is not the one this write was planned for".to_string()),
        });

        let words = words(&mut app);
        assert!(words.contains("This write is refused"), "{words}");
        assert!(
            words.contains("not the one this write was planned for"),
            "and why: {words}"
        );
        assert!(
            !words.contains("type the destination's address"),
            "and it does not ask for a confirmation it would throw away: {words}"
        );
    }

    /// A failure is shown with the hint core wrote for it.
    ///
    /// Core returns its diagnosis as data instead of printing it, so a front-end
    /// can show the hint. A device that will not open gets the udev rule that
    /// fixes it.
    #[test]
    fn a_failure_is_shown_with_the_next_step_core_gave_it() {
        let mut app = an_app();
        app.session.last = Some(Err(Error::AccessDenied {
            vendor_id: 0x2207,
            product_id: 0x350e,
        }));

        let words = words(&mut app);
        assert!(words.contains("access denied"), "{words}");
        assert!(
            words.contains("SUBSYSTEM==\"usb\""),
            "and the rule that fixes it: {words}"
        );
        assert!(
            words.contains("2207"),
            "for the vendor that was denied: {words}"
        );
    }

    /// A recovery file, as a person would have picked one.
    fn a_recovery_blob(name: &str, len: usize) -> PickedBlob {
        PickedBlob {
            name: name.to_string(),
            bytes: vec![0u8; len],
        }
    }

    /// The recovery section names the serial flow and states its one limit up
    /// front.
    ///
    /// It is its own section, not grayed USB verbs. It states that the write is not
    /// read back before anybody plans anything.
    #[test]
    fn the_revealed_recovery_section_names_the_flow_and_its_one_limit() {
        let mut app = an_app();
        // Serial recovery is declared, not discovered: the form appears only once a
        // person opens it.
        app.show_recovery = true;
        let words = words_on(&mut app, Tab::Serial);
        assert!(
            words.contains("StarFive recovery (serial)"),
            "the flow is named: {words}"
        );
        assert!(
            words.contains("board's write is not read back"),
            "and its one limit is stated up front: {words}"
        );
    }

    /// The console section is inside the serial flow, and declared the same way the
    /// recovery form is.
    ///
    /// A console on a UART is just a port, and nothing on it announces the board.
    #[test]
    fn the_idle_screen_offers_the_serial_console_without_dumping_its_form() {
        let mut app = an_app();
        let words = words_on(&mut app, Tab::Serial);
        assert!(
            words.contains("Open serial console"),
            "the console is offered as a deliberate action: {words}"
        );
        assert!(
            !words.contains("Watch the console"),
            "and its form is not dumped on an idle screen: {words}"
        );
    }

    /// The revealed console section names the flow and its boundary.
    ///
    /// Once the U-Boot half is open, it says plainly that the raw command line is
    /// ungated and that a boot override is not saved.
    #[test]
    fn the_revealed_console_section_names_its_boundary_and_what_is_ungated() {
        let mut app = an_app();
        app.show_console = true;
        app.console.show_uboot = true;
        let words = words_on(&mut app, Tab::Serial);

        assert!(
            words.contains("Serial console"),
            "the flow is named: {words}"
        );
        assert!(
            words.contains("login prompt"),
            "and where its boundary falls is stated: {words}"
        );
        assert!(
            words.contains("ungated"),
            "the raw command line says what it is: {words}"
        );
        assert!(
            words.contains("saveenv"),
            "and names the one thing that could be typed through it: {words}"
        );
        assert!(
            words.contains("for one boot, and nothing is saved"),
            "the boot override says it is volatile before it is even planned: {words}"
        );
    }

    /// The boot override is a group and a plain yes, not a window-taking screen with
    /// a coordinate to type.
    ///
    /// A typed coordinate exists because a clone with its ends swapped is a mistake
    /// careful reading does not catch. An override has no second board and
    /// destroys nothing. Ceremony on a harmless act teaches a person to stop
    /// reading the ceremony on a dangerous one.
    #[test]
    fn the_boot_override_plan_says_it_is_not_saved_and_asks_for_a_plain_yes() {
        let mut app = an_app();
        app.show_console = true;
        app.session.console.pending = Some(crate::state::BootPending {
            plan: pyrographer_core::uboot::BootPlan {
                current: Some("mmc1 usb0".to_string()),
                targets: "mmc0".to_string(),
                persistent: false,
            },
            line: crate::state::ConsoleLine {
                port: "/dev/ttyUSB0".to_string(),
                baud: 115_200,
                prompt: "=> ".to_string(),
                reads: 30,
            },
        });

        let words = words_on(&mut app, Tab::Serial);
        assert!(
            words.contains("/dev/ttyUSB0"),
            "the line it would run over is named: {words}"
        );
        assert!(
            words.contains("mmc1 usb0"),
            "what the board answered is shown beside what would be set: {words}"
        );
        assert!(
            words.contains("not saved"),
            "and the one thing that makes this different is stated: {words}"
        );
        assert!(
            words.contains("Set it and boot"),
            "the act is a plain yes: {words}"
        );
        // The window is not taken over, so the rest of the screen is still there.
        assert!(
            words.contains("pyrographer"),
            "an override is not a gate that hides the window: {words}"
        );
    }

    /// Where the two halves of the RAM-boot loop meet.
    ///
    /// A maskrom board handed a full U-Boot instead of a bare loader does not come
    /// back on the bus. Its next output arrives on a serial port. A person who is
    /// not told that reads a silent device list as a failure.
    #[test]
    fn the_maskrom_report_says_where_a_ram_booted_u_boot_answers() {
        let words = drawn_report(&Report::Bootstrapped { chip: None });
        assert!(
            words.contains("serial port"),
            "the handoff to the console flow is named: {words}"
        );
        assert!(
            words.contains("rockusb gadget") && words.contains("mass-storage gadget"),
            "and so is what to do there, with either gadget: {words}"
        );
    }

    #[test]
    fn the_idle_screen_offers_serial_recovery_without_dumping_its_form() {
        let mut app = an_app();
        let words = words_on(&mut app, Tab::Serial);
        assert!(
            words.contains("Start serial recovery"),
            "a JH7110 cannot be discovered, so the flow is offered as a deliberate action: {words}"
        );
        assert!(
            !words.contains("board's write is not read back"),
            "and its form is not dumped on an idle screen that has found no such board: {words}"
        );
    }

    /// The recovery plan is a screen, and it shows everything a person needs.
    ///
    /// It shows what a recovery writes and where, and the port to type to go
    /// ahead. It also shows what sets it apart from every other write in
    /// pyrographer: the write is not read back.
    #[test]
    fn the_recovery_plan_screen_shows_what_it_writes_and_that_it_is_not_read_back() {
        let mut app = an_app();
        app.session
            .set_recovery_agent(Some(a_recovery_blob("agent.bin", 4096)));
        app.session
            .set_recovery_spl(Some(a_recovery_blob("spl.bin", 2048)));
        app.session
            .plan_recovery("/dev/ttyUSB0".to_string(), RecoveryTarget::NorFlash)
            .expect("a plan");

        let words = words(&mut app);

        assert!(words.contains("This recovers a StarFive board"), "{words}");
        assert!(words.contains("QSPI NOR flash"), "the medium: {words}");
        assert!(words.contains("SPL"), "the stage: {words}");
        assert!(
            words.contains("agent menu option 0"),
            "and how it is written: {words}"
        );
        assert!(
            words.contains("This board's write is not read back"),
            "the one fact a recovery is owed: {words}"
        );
        assert!(
            words.contains("type the serial port: /dev/ttyUSB0"),
            "and the act that confirms it: {words}"
        );
    }

    /// A serial job reaches its line through a task, and a failed open is reported
    /// rather than swallowed.
    ///
    /// The open is a task in both builds, and in a tab it is a promise. The
    /// transport therefore arrives on a later frame, and the job starts then. This
    /// gives one path for all three serial jobs, at the cost of a frame between
    /// asking and running. This test pins both ends of that frame. While the open
    /// is in flight, the flow reports itself busy, so a second click cannot open
    /// the port twice. An open that fails clears the pending work and reports the
    /// failure, instead of leaving a job that never starts.
    ///
    /// It is driven against a port that cannot exist, so it needs no hardware and
    /// no adapter plugged in.
    #[test]
    fn a_console_session_over_a_port_that_will_not_open_reports_it_and_starts_nothing() {
        let mut app = an_app();
        app.console.port = "/dev/pyrographer-no-such-port".to_string();
        app.console.expect = "boot ok".to_string();

        app.watch_console(0.0);
        assert!(
            app.is_console_busy(),
            "the port is being opened, so the flow is busy before any job exists"
        );
        assert!(
            app.session.console.job.is_none(),
            "and there is no job yet -- the transport has not arrived"
        );

        // The frame the open answers on. Natively the future is ready the first
        // time it is polled, so one turn of the loop is enough.
        for _ in 0..200 {
            app.collect_for_test(0.0);
            if !app.is_console_busy() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            !app.is_console_busy(),
            "an open that failed leaves nothing in flight"
        );
        assert!(
            app.session.console.job.is_none(),
            "and starts no session over a line it never opened"
        );
        assert!(
            matches!(app.session.last, Some(Err(_))),
            "the failure is reported, not swallowed"
        );
    }

    /// A recovery whose port will not open loses its plan, and says so.
    ///
    /// It has the same shape as a write whose image will not open. The
    /// confirmation is spent where the person gave it. An open that fails
    /// afterwards reports the failure and leaves no half-confirmed recovery
    /// behind. A person who still wants the recovery confirms again.
    #[test]
    fn a_recovery_over_a_port_that_will_not_open_spends_its_plan_and_reports() {
        let mut app = an_app();
        app.recover.port = "/dev/pyrographer-no-such-port".to_string();
        app.session
            .set_recovery_agent(Some(a_recovery_blob("agent.bin", 4096)));
        app.session
            .set_recovery_spl(Some(a_recovery_blob("spl.bin", 2048)));
        app.plan_recovery();

        // Type the port back, which is what this build's gate asks for.
        if let Some(pending) = app.session.recovery.pending.as_mut()
            && let Confirmation::Typed { expected, typed } = &mut pending.confirmation
        {
            *typed = expected.clone();
        }

        app.recover(0.0);
        assert!(
            app.session.recovery.pending.is_none(),
            "the plan is spent at the confirmation, not at the open"
        );

        for _ in 0..200 {
            app.collect_for_test(0.0);
            if !app.is_recovering() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            !app.is_recovering(),
            "an open that failed leaves no recovery in flight"
        );
        assert!(
            matches!(app.session.last, Some(Err(_))),
            "and the failure is reported"
        );
    }

    /// A recovery runs over the port it was confirmed for, not over a later edit.
    ///
    /// The plan remembers its port, and the confirmation transcribes that port.
    /// Nothing stops a person typing a different one into the form afterwards. If
    /// the open read the form, the recovery would go somewhere nobody confirmed, so
    /// the open reads the plan. The test is driven against ports that cannot exist,
    /// so it asserts on which one the failure names.
    #[test]
    fn a_recovery_opens_the_port_it_was_confirmed_for_not_the_form() {
        let mut app = an_app();
        app.recover.port = "/dev/pyrographer-agreed".to_string();
        app.session
            .set_recovery_agent(Some(a_recovery_blob("agent.bin", 4096)));
        app.session
            .set_recovery_spl(Some(a_recovery_blob("spl.bin", 2048)));
        app.plan_recovery();

        if let Some(pending) = app.session.recovery.pending.as_mut()
            && let Confirmation::Typed { expected, typed } = &mut pending.confirmation
        {
            *typed = expected.clone();
        }

        // Edited after the plan was agreed to, and before it runs.
        app.recover.port = "/dev/pyrographer-not-agreed".to_string();
        app.recover(0.0);

        for _ in 0..200 {
            app.collect_for_test(0.0);
            if !app.is_recovering() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let Some(Err(error)) = &app.session.last else {
            panic!("an open against a port that cannot exist reports a failure");
        };
        let said = error.to_string();
        assert!(
            said.contains("pyrographer-agreed"),
            "the port opened is the one the plan was agreed to about: {said}"
        );
    }

    /// A form naming no port is refused before a plan screen appears.
    ///
    /// The check runs at the plan instead of at the open, so nobody confirms a
    /// recovery that cannot reach a line.
    #[test]
    fn planning_a_recovery_with_no_port_refuses_and_shows_no_plan() {
        let mut app = an_app();
        app.session
            .set_recovery_agent(Some(a_recovery_blob("agent.bin", 4096)));
        app.session
            .set_recovery_spl(Some(a_recovery_blob("spl.bin", 2048)));

        app.plan_recovery();

        assert!(
            app.session.recovery.pending.is_none(),
            "no plan screen for a recovery with nowhere to go"
        );
        assert!(matches!(app.session.last, Some(Err(_))), "and it says so");
    }

    /// The partition table section offers both repairs and an authoring entry.
    ///
    /// It has two explicit repair buttons, GPT and Rockchip parameter. A table too
    /// damaged to name its own format still needs the right repair on offer. The
    /// author form stays a single button until it is asked for.
    ///
    /// The section is closed until it is asked for, and the first half of the test
    /// pins that. These are the tools for a board that is already wrong. Drawn
    /// open, they put 119 px between a person and the verb they came for. The
    /// closed section still shows its name, so nobody has to guess where the tools
    /// are.
    #[test]
    fn the_partition_table_section_offers_both_repairs_and_an_author_entry() {
        let mut app = an_app();
        let shut = drawn(|ui| table_tools(&mut app, ui, true, 0.0));
        assert!(
            shut.contains("Partition table"),
            "shut, the section still names itself: {shut}"
        );
        assert!(
            !shut.contains("Repair GPT"),
            "but its tools are not dumped on the verb surface: {shut}"
        );

        app.show_table_tools = true;
        let words = drawn(|ui| table_tools(&mut app, ui, true, 0.0));

        assert!(words.contains("Partition table"), "the section: {words}");
        assert!(words.contains("Repair GPT"), "the GPT repair: {words}");
        assert!(
            words.contains("Repair parameter"),
            "the parameter repair, its own button not a routed one: {words}"
        );
        assert!(
            words.contains("Author a fresh table"),
            "and the author entry, still a single button: {words}"
        );
    }

    /// A DFU board, as core builds one, with an empty script: it is asked nothing.
    fn a_dfu_board() -> FlashAgent<ScriptedTransport> {
        FlashAgent::Dfu(DfuAgent::new(
            ScriptedTransport::new(Vec::new()),
            0,
            pyrographer_core::testing::dfu_capable(512),
            Vec::new(),
        ))
    }

    /// The partition table tools on a board with no device-wide LBA space are
    /// grayed, and the refusal is a sentence on the screen.
    ///
    /// Core refuses every table plan on such a board, a DFU board among them. A
    /// button that planned and then failed would teach a person nothing until
    /// clicked. The sentence is core's own, taken from a real DFU agent, so this test
    /// cannot pass on wording core does not use. Each button also carries it into
    /// the accessibility tree.
    #[test]
    fn the_table_tools_on_a_board_with_no_device_wide_lba_space_are_refused_on_the_screen() {
        let mut app = an_app();
        app.session.target.raw_lba_reason =
            pyrographer_core::verbs::raw_lba_refusal(&a_dfu_board());
        assert!(app.session.target.raw_lba_reason.is_some(), "the premise");
        app.show_table_tools = true;

        let words = drawn(|ui| table_tools(&mut app, ui, true, 0.0));
        assert!(
            words.contains("Repair and authoring are refused"),
            "the refusal is drawn: {words}"
        );
        assert!(words.contains("named region"), "in core's words: {words}");

        let tree = tree(|ui| table_tools(&mut app, ui, true, 0.0));
        for name in [
            "Repair GPT...",
            "Repair parameter...",
            "Author a fresh table...",
        ] {
            let button = tree
                .controls()
                .into_iter()
                .find(|node| tree.announced(node).as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} is drawn and grayed rather than hidden"));
            assert!(button.is_disabled(), "{name} is grayed");
            assert!(
                button
                    .description()
                    .is_some_and(|why| why.contains("named region")),
                "{name} says why: {:?}",
                button.description()
            );
        }
    }

    /// A clone from a board with no device-wide LBA space is refused on the
    /// screen, not only at the plan.
    ///
    /// A clone reads the whole source by raw LBA, as surely as it writes the whole
    /// destination. The source panel's caps are a DFU board's, and the target's are
    /// unknown, so the sentence drawn is the one about the source.
    #[test]
    fn a_clone_from_a_board_with_no_device_wide_lba_space_is_refused_on_the_screen() {
        let mut app = an_app();
        app.session.source.caps = Some(a_dfu_board().caps());

        let words = drawn(|ui| verbs_panel(&mut app, ui));
        assert!(
            words.contains("A clone is refused. The board chosen to clone from"),
            "{words}"
        );
    }

    /// The clone and table plan screens say when the write is checked, as the write
    /// plan screen does.
    ///
    /// Each is a write a person confirms. Each therefore carries the backend's own
    /// sentence for when the read-back happens.
    #[test]
    fn the_clone_and_table_plan_screens_say_when_the_write_would_be_checked() {
        let app = an_app();

        let clone = ClonePlan {
            source: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            destination: a_plan(),
        };
        let words = drawn(|ui| clone_plan(&app, &clone, ui));
        assert!(
            words.contains("checked") && words.contains("stops the write at that window"),
            "a clone: {words}"
        );

        let words = drawn(|ui| table_plan(&app, &a_repair_plan(), ui));
        assert!(
            words.contains("checked") && words.contains("stops the write at that window"),
            "a table write: {words}"
        );
    }

    /// The author form adapts to the format.
    ///
    /// A Rockchip parameter authoring shows the medium picker and the verbatim-text
    /// source. A GPT, which is absolute and is not built from parameter text, shows
    /// neither.
    #[test]
    fn the_author_form_adapts_to_the_format() {
        let mut app = an_app();
        app.author.show = true;

        app.author.format = TableFormat::RockchipParam;
        let param = drawn(|ui| author_tools(&mut app, ui, true, 0.0));
        assert!(
            param.contains("Medium") && param.contains("raw NAND"),
            "a parameter table takes a medium: {param}"
        );
        assert!(
            param.contains("parameter text"),
            "and can be authored from an existing block's text: {param}"
        );

        app.author.format = TableFormat::Gpt;
        let gpt = drawn(|ui| author_tools(&mut app, ui, true, 0.0));
        assert!(
            gpt.contains("Format") && gpt.contains("GPT"),
            "the format row is drawn: {gpt}"
        );
        assert!(
            !gpt.contains("Medium"),
            "a GPT is absolute and takes no medium: {gpt}"
        );
        assert!(
            !gpt.contains("parameter text"),
            "and is not built from parameter text: {gpt}"
        );
        assert!(
            gpt.contains("Author table"),
            "the plan button is there: {gpt}"
        );
    }

    /// A table write reports what it did.
    ///
    /// The report says whether it authored a fresh table or repaired damaged
    /// copies, and in which format. A progress bar filling up looks the same either
    /// way.
    #[test]
    fn a_table_write_report_says_what_it_did() {
        let authored = drawn_report(&Report::TableWritten {
            format: TableFormat::Gpt,
            authored: true,
        });
        assert!(
            authored.contains("Wrote a fresh GPT table"),
            "an authoring says it authored: {authored}"
        );

        let repaired = drawn_report(&Report::TableWritten {
            format: TableFormat::RockchipParam,
            authored: false,
        });
        assert!(
            repaired.contains("Rewrote the damaged Rockchip parameter copy"),
            "a repair says it repaired, and in which format: {repaired}"
        );
    }

    /// The disk section is closed until it is opened.
    ///
    /// This is the window's form of `list` against `list --blocks`. A board is on
    /// the bus because somebody put it there. The machine's disks are different.
    /// Drawing them beside a board, with nothing to tell the two apart, invites the
    /// mistake the Block backend's refusals exist to prevent.
    #[test]
    fn the_disk_section_is_closed_until_it_is_opened() {
        let mut app = an_app();
        let shut = words(&mut app);
        assert!(shut.contains("Disks"), "the section is there: {shut}");
        assert!(
            !shut.contains("acted on only when it is named"),
            "and shut, so no disk is drawn: {shut}"
        );

        app.session.disks.show = true;
        app.session.disks.asked = true;
        app.session.disks.listed = vec![
            a_disk("/dev/sdb", 32 << 30),
            BlockDevice {
                carries_running_system: true,
                ..a_disk("/dev/nvme0n1", 1000 << 30)
            },
        ];

        let open = words(&mut app);
        assert!(
            open.contains("acted on only when it is named"),
            "nothing here is chosen by default: {open}"
        );
        assert!(
            open.contains("/dev/sdb"),
            "the disk that can be used: {open}"
        );
        assert!(
            open.contains("running system") && open.contains("refused"),
            "and the one that cannot, refused in place of its buttons: {open}"
        );
    }

    /// A listing that failed says so where the rows would be.
    ///
    /// An empty section would look exactly like a machine with no disks, and send
    /// a person looking for the card reader they can see.
    #[test]
    fn a_disk_section_with_no_backend_says_which_platform_it_is_built_for() {
        let mut app = an_app();
        app.session.disks.show = true;
        app.session.disks.asked = true;
        app.session.disks.problem = Some("the Block backend is built for Linux only".to_string());

        let words = words(&mut app);
        assert!(words.contains("built for Linux only"), "{words}");
    }

    /// A disk's plan renders the guard that applies to it.
    ///
    /// On a board, the last two rows are the loader's own answer and the verdict on
    /// it. A disk runs no loader. The board's rows would print *the write is
    /// refused until one is named*, which is false for a disk. A plan line that is
    /// reliably wrong teaches a person to skip the plan. The confirmation is also
    /// worded for what it names, because a node is not an address on a bus.
    #[test]
    fn a_disks_plan_shows_the_exclusive_open_where_a_boards_shows_its_loader() {
        let mut app = an_app();
        let disk = a_disk("/dev/sdb", 32 << 30);
        app.session.select_target_disk(disk.clone());
        app.session.pending = Some(Pending {
            plan: Plan::Write(a_plan()),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Block(disk)),
            refused: None,
        });

        let words = words(&mut app);

        assert!(words.contains("/dev/sdb"), "which disk: {words}");
        assert!(
            words.contains("exclusively (O_EXCL)"),
            "what holds it: {words}"
        );
        assert!(
            words.contains("does not rest on this disk"),
            "and the refusal that has no override: {words}"
        );
        assert!(
            !words.contains("refused until one is named"),
            "and never the board's line, which is false here: {words}"
        );
        assert!(
            words.contains("type the destination disk's node: /dev/sdb"),
            "the act, worded for what it names: {words}"
        );
    }

    /// A disk is offered no SoC field and no vendor verbs.
    ///
    /// The CLI refuses `--soc` on a block write instead of ignoring it. A flag
    /// quietly dropped leaves a person believing in a gate that is not there. The
    /// window's form of that refusal is not to ask. `chipver` and `reset` speak a
    /// vendor protocol a disk does not run, so they are drawn and disabled. Each is
    /// a grayed button that explains itself, as `erase` is everywhere else in this
    /// crate.
    #[test]
    fn a_disk_is_asked_for_no_soc_and_offered_no_vendor_verbs() {
        let mut app = an_app();
        app.session.select_target_disk(a_disk("/dev/sdb", 32 << 30));

        // Nothing is open, so the verb surface is not drawn -- but the two
        // questions this pins are answered without one.
        assert!(app.target_is_disk());
        assert!(
            matches!(app.gate_soc(), Ok(None)),
            "a name left over from a board cannot ride into a disk's plan"
        );

        app.form.soc = "rk3576".to_string();
        assert!(
            matches!(app.gate_soc(), Ok(None)),
            "not even one that parses"
        );
    }

    /// A device with no medium is named on one line, not given a row.
    ///
    /// This machine has eight unbound loop devices, and a row each would bury the
    /// one disk the section exists for. Dropping them silently would be worse. A
    /// reader with no card in it also reports zero capacity. A person whose newly
    /// inserted card is missing from the list needs exactly that line.
    #[test]
    fn devices_with_no_medium_are_counted_and_named_rather_than_given_rows() {
        fn a_machine_with_empty_slots() -> App {
            let mut app = an_app();
            app.session.disks.show = true;
            app.session.disks.asked = true;
            app.session.disks.listed = vec![
                a_disk("/dev/sdb", 32 << 30),
                BlockDevice {
                    bytes: 0,
                    ..a_disk("/dev/loop0", 0)
                },
                BlockDevice {
                    bytes: 0,
                    ..a_disk("/dev/loop1", 0)
                },
            ];
            app
        }

        let mut app = a_machine_with_empty_slots();
        let words = words(&mut app);
        assert!(
            words.contains("/dev/sdb"),
            "the disk keeps its row: {words}"
        );

        // **The count is out, the names are behind it.** The count answers the
        // question somebody actually has -- the card I just inserted is not in the
        // list, is it being seen at all? -- so it is readable without opening
        // anything. The names are for debugging, and eight of them set in
        // monospace directly under a safety warning compete with the warning for
        // the same attention.
        assert!(
            words.contains("2 devices with no medium"),
            "the count is readable without opening anything: {words}"
        );
        assert!(
            !words.contains("/dev/loop0"),
            "and the names are not, until they are asked for: {words}"
        );

        // Asked for, they appear, spelled the way the rows spell them. Clicked by
        // the name an assistive technology would announce, which also pins that
        // the disclosure is a control one can find and operate.
        let mut app = a_machine_with_empty_slots();
        let opened = after_clicking(|ui| draw(&mut app, ui), "2 devices with no medium");
        assert!(
            opened.contains("/dev/loop0, /dev/loop1"),
            "opening it names them: {opened}"
        );
        assert_eq!(
            opened.matches("/dev/loop0").count(),
            1,
            "still on one line rather than a row each: {opened}"
        );
    }

    /// A verb surface with a disk in the slot, drawn far enough to carry the
    /// capability refusals.
    ///
    /// Nothing is open, so every verb is grayed. The facts these sentences state
    /// are properties of the device and the backend, not of the connection.
    fn a_disk_verb_surface() -> App {
        let mut app = an_app();
        // Three usable disks in the list as well, so the per-row controls are
        // under test: `Use` and `Clone from` repeat once per row, and a name that
        // does not distinguish them is the failure this surface exists to catch.
        app.session.disks.asked = true;
        app.session.disks.listed = vec![
            a_disk("/dev/sdb", 32 << 30),
            a_disk("/dev/sdc", 64 << 30),
            a_disk("/dev/sdd", 16 << 30),
        ];
        app.session.select_target_disk(a_disk("/dev/sdb", 32 << 30));
        app.session.target.connection = state::Connection::Desynchronized;
        app.session.target.erase_reason = Some(
            "block erase: the block layer presents storage that is always writable, with no \
             erase to drive.",
        );
        app
    }

    /// Every control a person can reach has something to announce.
    ///
    /// A `TextEdit` and a `DragValue` carry no text of their own, so egui gives
    /// their accessibility nodes no name. A screen reader reaches the write gate's
    /// confirmation field and says "text input". The field is unlabeled, and no
    /// other test in this crate can see that. A screenshot shows the `ui.label`
    /// beside the box, and the tree does not carry it.
    ///
    /// This test walks the tree and requires every operable node to have a name.
    #[test]
    fn every_control_has_a_name_a_screen_reader_can_announce() {
        // **Once per tab.** A tab that is not up draws nothing, so a single walk
        // would leave every control on the other flow unexamined -- and the two
        // halves of this test pull in opposite directions across the split:
        // hiding a flow makes *names* easier to keep distinct while making it
        // easier for an unnamed control to go unnoticed. What a person meets is
        // one tab at a time, which is also the set a name has to distinguish
        // within.
        for tab in Tab::ALL {
            every_control_on_one_tab_has_a_distinct_name(tab);
        }
    }

    /// One tab's worth of the check above.
    fn every_control_on_one_tab_has_a_distinct_name(tab: Tab) {
        let mut app = a_disk_verb_surface();
        app.tab = tab;
        app.session.disks.show = true;
        app.show_recovery = true;
        app.show_console = true;
        app.console.show_uboot = true;
        app.author.show = true;

        let tree = tree(|ui| draw(&mut app, ui));
        let nameless: Vec<_> = tree
            .controls()
            .into_iter()
            .filter(|node| {
                tree.announced(node)
                    .is_none_or(|name| name.trim().is_empty())
            })
            .map(|node| format!("{:?}", node.role()))
            .collect();

        assert!(
            nameless.is_empty(),
            "every operable control needs a name; on {} these have none: {nameless:?}",
            tab.name()
        );

        // **And a name only helps if it tells one control from another.** Six
        // buttons called `Use` on a list of disks each have a name and none of
        // them says which disk -- which this test passed for, because it asked
        // whether a name existed rather than whether it distinguished. A grid row
        // is a layout concept and not a node, so there is no container between the
        // button and the whole grid to supply the context; the button has to carry
        // it. See [`Explain::named`].
        let mut seen: Vec<String> = tree
            .controls()
            .into_iter()
            .filter(|node| !node.is_disabled())
            .filter_map(|node| tree.announced(node))
            .collect();
        seen.sort();
        let ambiguous: Vec<&String> = seen
            .windows(2)
            .filter(|pair| pair[0] == pair[1])
            .map(|pair| &pair[0])
            .collect();
        assert!(
            ambiguous.is_empty(),
            "two controls a person can reach must not answer to the same name; on {} these do: \
             {ambiguous:?}",
            tab.name()
        );
    }

    /// The write gate's confirmation field says what to type.
    ///
    /// It is the most consequential control in the application. Without a name, a
    /// screen reader reaches it with nothing to say about it. The sentence above
    /// the box is its name, wired with `labelled_by`. The relation resolves because
    /// a `TextEdit` has no direct label of its own to take precedence.
    #[test]
    fn the_write_gates_confirmation_field_announces_the_coordinate_to_type() {
        let mut app = an_app();
        app.session.select_target(a_device(12), a_device(12));
        app.session.pending = Some(Pending {
            plan: Plan::Write(a_plan()),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(12))),
            refused: None,
        });

        let tree = tree(|ui| draw(&mut app, ui));
        let field = tree
            .controls()
            .into_iter()
            .find(|node| node.role() == egui::accesskit::Role::TextInput)
            .expect("the gate draws a field to type the coordinate into");

        let name = tree.announced(field).unwrap_or_default();
        assert!(
            name.contains("To confirm, type") && name.contains("003:12"),
            "the field announces the act and the coordinate, not just its role: {name:?}"
        );
    }

    /// A grayed control carries its reason into the tree.
    ///
    /// The house rule is that a disabled button explains itself. `on_hover_text`
    /// alone needs a pointer and reaches no accessibility tree. A screen reader
    /// would then announce that `Chip version` is disabled, and never say why.
    /// [`Explain::explain_disabled`] puts the same words on the node as its
    /// description, which AT-SPI exposes as `Accessible.Description`.
    #[test]
    fn a_grayed_control_carries_its_reason_into_the_tree() {
        let mut app = a_disk_verb_surface();

        let tree = tree(|ui| draw(&mut app, ui));
        let chipver = tree
            .controls()
            .into_iter()
            .find(|node| tree.announced(node).as_deref() == Some("Chip version"))
            .expect("the vendor verb is drawn and disabled rather than hidden");

        assert!(chipver.is_disabled(), "a disk runs no vendor protocol");
        assert!(
            chipver
                .description()
                .is_some_and(|why| why.contains("vendor protocol")),
            "and says why, where something other than a mouse can read it: {:?}",
            chipver.description()
        );
    }

    /// The reasons that guard a destructive act are drawn, not hovered.
    ///
    /// A description reaches a screen reader and not a sighted keyboard user. It
    /// does not exist at all in the web build, which has no accessibility tree. The
    /// capability refusals, which stand between a person and overwritten flash,
    /// are therefore sentences on the screen. That is how the house style ("a
    /// grayed button that explains itself") holds for them.
    #[test]
    fn the_capability_refusals_are_sentences_rather_than_tooltips() {
        let mut app = a_disk_verb_surface();
        let words = words(&mut app);

        assert!(
            words.contains("There is no loader on it to ask"),
            "a disk refuses the vendor verbs, on the screen: {words}"
        );
        assert!(
            words.contains("block erase"),
            "and the erase refusal is drawn as core worded it: {words}"
        );
    }

    /// A disk the running system rests on says so where it cannot be missed, once
    /// for all such disks.
    ///
    /// The row carries a `refused` chip, and a chip is all a grid cell has room
    /// for. A tooltip on that `Label` would reach no keyboard. egui deliberately
    /// keeps a label out of the tab order (`label.rs`: "Don't move focus to labels
    /// with TAB key").
    ///
    /// The reason is a sentence after the grid, one per kind and not one per disk.
    /// A root on an LVM volume over LUKS refuses four devices for the same reason.
    /// Four copies of one paragraph teach a person to scroll past it. Kinds are
    /// kept apart, because "There is no override" must not appear beside a disk a
    /// person can simply unmount.
    #[test]
    fn refused_disks_say_why_below_the_list_once_per_kind() {
        let mut app = an_app();
        let system: Vec<BlockDevice> = ["/dev/dm-0", "/dev/dm-1", "/dev/nvme0n1"]
            .into_iter()
            .map(|node| {
                let mut disk = a_disk(node, 512 << 30);
                disk.carries_running_system = true;
                disk
            })
            .collect();
        let mut held = a_disk("/dev/sdc", 64 << 30);
        held.mounts = vec!["/media/greg/CARD".to_string()];

        app.session.disks.show = true;
        app.session.disks.asked = true;
        app.session.disks.listed = system.into_iter().chain([held]).collect();

        let words = words(&mut app);

        // One warning for the three of them, in the plural, and it leads with the
        // consequence rather than with four identifiers.
        assert_eq!(
            words
                .matches("The disks below hold the running system")
                .count(),
            1,
            "three disks refused for one reason share one warning: {words}"
        );
        assert!(
            words.contains("The disks below hold the running system, so they cannot be opened")
                && words.contains("A write to any of them would corrupt this machine"),
            "and the warning opens with what happens, and points at its own list: {words}"
        );
        assert!(
            words.contains("There is no override"),
            "the refusal with no override is drawn: {words}"
        );

        // The devices are named under it, in the spelling the rows use and the
        // command line takes -- one identifier, one rendering.
        assert!(
            words.contains("/dev/dm-0, /dev/dm-1, /dev/nvme0n1"),
            "and every one of them is named, as `/dev/...`: {words}"
        );
        assert!(
            !words.contains("dm-0, dm-1, nvme0n1 hold"),
            "never the bare spelling the grid does not use: {words}"
        );

        // The held disk keeps its own warning and its own remedy, because
        // flattening the kinds would attach "no override" to a disk that has one.
        let held = words
            .lines()
            .find(|line| line.starts_with("The kernel is holding"))
            .expect("the held disk has a warning of its own");
        assert!(
            held.contains("Unmount it to clear this"),
            "and it says what clearing it does, without promising more: {held}"
        );
        assert!(
            !held.contains("no override") && !held.contains("running system"),
            "the kinds are not merged: {held}"
        );
        assert!(
            words.contains("/dev/sdc"),
            "and the held disk is named under it: {words}"
        );
    }

    /// SC 2.5.8, measured on the rectangles a person aims at.
    ///
    /// The criterion concerns drawn geometry and what is next to it, so a constant
    /// cannot answer it. `interact_size.y` is a floor for a radio. It is inert for
    /// these widgets:
    ///
    /// - A `Button`, whose text and padding already exceed it
    /// - A `Button::small`, which skips the floor by construction
    /// - A `TextEdit`, whose height is its rows plus its own margin
    ///
    /// A test that pinned that number would guard a proxy, and leave two widget
    /// classes in real use unmeasured.
    ///
    /// This test walks the accessibility tree, whose nodes carry the bounds egui
    /// laid out. It applies the criterion with its spacing exception. If a 24 px
    /// circle centered on an undersized target meets no other target's rectangle
    /// and no other undersized target's circle, the target passes. That exception
    /// makes a 19 px `small_button` conformant here, and crowding one would break
    /// it. A future layout change can break that condition without notice, so this
    /// test checks it.
    #[test]
    fn every_target_clears_the_minimum_size_or_its_spacing_exception() {
        let mut app = a_disk_verb_surface();
        app.session.disks.show = true;
        app.session.disks.asked = true;
        app.session.disks.listed = vec![a_disk("/dev/sdc", 64 << 30)];
        app.show_recovery = true;
        app.show_console = true;
        app.console.show_uboot = true;
        app.author.show = true;

        let targets = aimed_at(&tree(|ui| draw(&mut app, ui)));
        assert!(
            targets.len() > 20,
            "the surface under test should be a dense one, not an empty screen: {} targets",
            targets.len()
        );

        let failures = crowded_targets(&targets);
        assert!(
            failures.is_empty(),
            "targets under 24 px must keep their spacing exception:\n  {}",
            failures.join("\n  ")
        );
    }

    /// The criterion, proven against a layout built to fail it.
    ///
    /// [`every_target_clears_the_minimum_size_or_its_spacing_exception`] passes,
    /// and a check that has never failed proves nothing about what it catches.
    /// This test points the same function at two small buttons drawn against each
    /// other, with no spacing between them. Each is 19 px tall, the height of a
    /// `Button::small`. The check must catch them.
    #[test]
    fn the_target_size_check_catches_two_small_controls_drawn_too_close() {
        let tree = tree(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            ui.vertical(|ui| {
                let _ = ui.small_button("Hide");
                let _ = ui.small_button("Clear");
            });
        });

        let targets = aimed_at(&tree);
        assert_eq!(targets.len(), 2, "two small buttons: {targets:?}");
        assert!(
            !crowded_targets(&targets).is_empty(),
            "two 19 px controls stacked with no gap must fail the spacing exception: {targets:?}"
        );
    }

    /// Every target on a drawn screen, named, with the rectangle a person aims at.
    fn aimed_at(tree: &Tree) -> Vec<(String, egui::accesskit::Rect)> {
        tree.controls()
            .into_iter()
            .filter_map(|node| {
                let name = tree
                    .announced(node)
                    .unwrap_or_else(|| format!("{:?}", node.role()));
                node.bounds().map(|bounds| (name, bounds))
            })
            .collect()
    }

    /// SC 2.5.8's spacing exception, applied.
    ///
    /// If a 24 px circle centered on an undersized target meets neither another
    /// target's rectangle nor another undersized target's circle, the target keeps
    /// the exception. Returns one line per pair that fails, so a failure names
    /// both.
    fn crowded_targets(targets: &[(String, egui::accesskit::Rect)]) -> Vec<String> {
        const MIN: f64 = 24.0;
        const RADIUS: f64 = MIN / 2.0;

        // How far a point is from a rectangle: zero inside it.
        fn gap(x: f64, y: f64, r: egui::accesskit::Rect) -> f64 {
            let dx = (r.x0 - x).max(0.0).max(x - r.x1);
            let dy = (r.y0 - y).max(0.0).max(y - r.y1);
            (dx * dx + dy * dy).sqrt()
        }
        fn undersized(r: egui::accesskit::Rect) -> bool {
            (r.x1 - r.x0) < MIN || (r.y1 - r.y0) < MIN
        }
        fn center(r: egui::accesskit::Rect) -> (f64, f64) {
            ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0)
        }

        let mut failures = Vec::new();
        for (index, (name, rect)) in targets.iter().enumerate() {
            if !undersized(*rect) {
                continue;
            }
            let (cx, cy) = center(*rect);
            for (other_index, (other_name, other)) in targets.iter().enumerate() {
                if other_index == index {
                    continue;
                }
                let crowded = if undersized(*other) {
                    // Two circles of radius 12: they miss if their centers are 24
                    // apart.
                    let (ox, oy) = center(*other);
                    ((cx - ox).powi(2) + (cy - oy).powi(2)).sqrt() < MIN
                } else {
                    // A circle against a rectangle.
                    gap(cx, cy, *other) < RADIUS
                };
                if crowded {
                    failures.push(format!(
                        "`{name}` is {:.0}x{:.0} px and `{other_name}` is inside its {MIN} px \
                         circle",
                        rect.x1 - rect.x0,
                        rect.y1 - rect.y0
                    ));
                }
            }
        }
        failures
    }

    /// A focused control is drawn differently from a held-down one.
    ///
    /// egui answers `has_focus()` with the `active` visuals, so keyboard focus and
    /// a pressed button draw the same pixels. A person tabbing through sees the
    /// held-down look move from control to control. That mapping is hard-coded, so
    /// [`crate::theme::focus_ring`] adds what the pressed look lacks: a ring
    /// outside the widget, clear of it.
    ///
    /// The test draws twice, once with nothing focused and once after a Tab. The
    /// ring must appear only in the second, and must lie outside the control it
    /// rings. A ring on the border would be the pressed border again.
    #[test]
    fn keyboard_focus_draws_a_ring_outside_the_control_and_pressed_does_not() {
        fn frame(tab: bool) -> (Vec<egui::epaint::ClippedShape>, egui::Rect) {
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            let mut button = egui::Rect::NOTHING;
            let mut draw_one = |ui: &mut egui::Ui| {
                button = ui.button("Dump to file...").rect;
            };

            let base = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 200.0),
                )),
                ..Default::default()
            };
            let mut first = ctx.run_ui(base.clone(), |ui| draw_one(ui));
            first.textures_delta.clear();

            let mut input = base;
            if tab {
                input.events.push(egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let mut second = ctx.run_ui(input, |ui| {
                draw_one(ui);
                crate::theme::focus_ring(ui.ctx());
            });
            second.textures_delta.clear();
            (second.shapes, button)
        }

        // A ring: a stroked rectangle with no fill, standing off the control.
        fn ring_around(
            shapes: &[egui::epaint::ClippedShape],
            control: egui::Rect,
        ) -> Option<egui::Rect> {
            fn scan(shape: &egui::Shape, control: egui::Rect, found: &mut Option<egui::Rect>) {
                match shape {
                    egui::Shape::Rect(rect) => {
                        if rect.stroke.width > 0.0
                            && rect.fill == egui::Color32::TRANSPARENT
                            && rect.rect.contains_rect(control)
                            && rect.rect != control
                        {
                            *found = Some(rect.rect);
                        }
                    }
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            scan(shape, control, found);
                        }
                    }
                    _ => {}
                }
            }
            let mut found = None;
            for clipped in shapes {
                scan(&clipped.shape, control, &mut found);
            }
            found
        }

        let (idle, control) = frame(false);
        assert!(
            ring_around(&idle, control).is_none(),
            "nothing is focused, so nothing is ringed"
        );

        let (focused, control) = frame(true);
        let ring = ring_around(&focused, control)
            .expect("Tab moves focus to the button, and a focused control is ringed");
        assert!(
            ring.min.x < control.min.x && ring.min.y < control.min.y,
            "the ring stands outside the control rather than on its border: \
             ring {ring:?} against control {control:?}"
        );
    }

    /// Walk the tab order, and report what keyboard focus lands on at each stop.
    ///
    /// It sends real `Tab` events through a real `Context`, so this is the order a
    /// person gets, not the order the source reads in.
    fn tab_order(mut f: impl FnMut(&mut egui::Ui), steps: usize) -> Vec<String> {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        ctx.enable_accesskit();
        let base = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 4000.0),
            )),
            ..Default::default()
        };

        let mut first = ctx.run_ui(base.clone(), |ui| f(ui));
        first.textures_delta.clear();

        let mut stops = Vec::new();
        for _ in 0..steps {
            let mut input = base.clone();
            input.events.push(egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
            let mut output = ctx.run_ui(input, |ui| f(ui));
            output.textures_delta.clear();

            let focused = ctx.memory(|memory| memory.focused());
            // **Composed the way [`Tree::announced`] composes it**, and not from
            // the node alone. A `DragValue` labeled by the text beside it carries
            // no label of its own, so reading one off the node gives back its
            // *value* -- "115200" for the baud field -- which is not a name and
            // does not match what the same control is called anywhere else. The
            // set this produces is compared against a set of announced names, so
            // it has to be built the same way or the comparison reports controls
            // as unreachable that a Tab lands on perfectly well.
            let name = focused.and_then(|id| {
                let update = output.platform_output.accesskit_update.as_ref()?;
                let tree = Tree(update.nodes.to_vec());
                let (_, node) = update
                    .nodes
                    .iter()
                    .find(|(node_id, _)| *node_id == id.accesskit_id())?;
                tree.announced(node)
            });
            stops.push(name.unwrap_or_default());
        }
        stops
    }

    /// What keyboard focus rests on as a screen appears, before anybody presses
    /// anything.
    ///
    /// It runs two frames and sends no events. It answers where the keyboard points
    /// on arrival, which differs from where the first Tab goes.
    fn focus_on_arrival(mut f: impl FnMut(&mut egui::Ui)) -> Option<String> {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        ctx.enable_accesskit();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 4000.0),
            )),
            ..Default::default()
        };
        let mut first = ctx.run_ui(input.clone(), |ui| f(ui));
        first.textures_delta.clear();
        let mut output = ctx.run_ui(input, |ui| f(ui));
        output.textures_delta.clear();

        let id = ctx.memory(|memory| memory.focused())?;
        let update = output.platform_output.accesskit_update.as_ref()?;
        let (_, node) = update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id.accesskit_id())?;
        Some(format!("{:?}", node.role()))
    }

    /// Every control the main screen offers can be reached with Tab.
    ///
    /// A control that draws but cannot be focused is unavailable to a keyboard
    /// user. The order is walked far enough to wrap. The set of stops must account
    /// for every enabled control the same frame put in the accessibility tree. A
    /// control added inside a layout that swallows focus therefore fails here,
    /// before it reaches a person.
    #[test]
    fn every_control_on_the_main_screen_is_reachable_with_tab() {
        // **Once per flow.** The window draws one tab at a time, so a single walk
        // would report a control on the other flow as reachable by never having
        // looked at it -- which is exactly the failure the split makes possible
        // and this test exists to catch.
        for tab in Tab::ALL {
            every_control_on_one_tab_is_reachable_with_tab(tab);
        }
    }

    /// One tab's worth of the walk above.
    fn every_control_on_one_tab_is_reachable_with_tab(tab: Tab) {
        let mut app = a_disk_verb_surface();
        app.tab = tab;
        app.session.disks.show = true;
        app.show_recovery = true;
        app.show_console = true;

        let drawn: Vec<String> = {
            let tree = tree(|ui| draw(&mut app, ui));
            tree.controls()
                .into_iter()
                .filter(|node| !node.is_disabled())
                .filter_map(|node| tree.announced(node))
                .collect()
        };

        // Twice round, so a stop that only appears after the wrap is still seen.
        let reached = tab_order(|ui| draw(&mut app, ui), drawn.len() * 2 + 4);
        let missed: Vec<&String> = drawn
            .iter()
            .filter(|name| !reached.iter().any(|stop| stop == *name))
            .collect();

        assert!(
            !drawn.is_empty(),
            "{} should offer some controls",
            tab.name()
        );
        assert!(
            missed.is_empty(),
            "on {} these controls are drawn and enabled but no Tab reaches them: {missed:?}",
            tab.name()
        );
    }

    /// The write gate puts keyboard focus on its confirmation field.
    ///
    /// When a screen is replaced, egui does not clear focus. A person who tabbed
    /// onto a button arrives at the gate with focus still on a widget that is no
    /// longer drawn. Space does nothing and no ring appears. The gate places focus
    /// on the field the coordinate is typed into.
    ///
    /// Focus is not consent, and this test also pins what consent requires. The
    /// confirm button is **not** in the tab order until the coordinate has been
    /// typed.
    #[test]
    fn the_write_gate_focuses_its_field_and_offers_confirm_only_once_it_is_typed() {
        fn gate(typed_correctly: bool) -> Vec<String> {
            let mut app = an_app();
            app.session.select_target(a_device(12), a_device(12));
            let mut confirmation =
                Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(12)));
            if typed_correctly && let Confirmation::Typed { expected, typed } = &mut confirmation {
                *typed = expected.clone();
            }
            app.session.pending = Some(Pending {
                plan: Plan::Write(a_plan()),
                confirmation,
                refused: None,
            });
            tab_order(move |ui| draw(&mut app, ui), 6)
        }

        let untyped = gate(false);
        assert!(
            !untyped.iter().any(|stop| stop == "Overwrite the board"),
            "an unconfirmed gate does not offer the write to the keyboard: {untyped:?}"
        );
        assert!(
            untyped.iter().any(|stop| stop == "Cancel"),
            "but leaving is always reachable: {untyped:?}"
        );

        let typed = gate(true);
        assert!(
            typed.iter().any(|stop| stop == "Overwrite the board"),
            "once the coordinate is typed the act is reachable without a mouse: {typed:?}"
        );

        // And on arrival -- before any key is pressed -- the keyboard is already
        // pointed at the field, rather than at a widget from the screen this one
        // replaced.
        let mut app = an_app();
        app.session.select_target(a_device(12), a_device(12));
        app.session.pending = Some(Pending {
            plan: Plan::Write(a_plan()),
            confirmation: Confirmation::asked_of(ConfirmBy::Coordinate, &Chosen::Usb(a_device(12))),
            refused: None,
        });
        assert_eq!(
            focus_on_arrival(move |ui| draw(&mut app, ui)).as_deref(),
            Some("TextInput"),
            "the gate places the keyboard on the field the coordinate is typed into"
        );
    }

    /// Warmth is scarce, and this count keeps it scarce.
    ///
    /// The palette spends the warm scale only where flash is at stake. If amber and
    /// red were also decoration, a red row would stop being information. Without a
    /// test, nothing enforces that principle. With both its reason and its verdict
    /// in the destructive color, four refused disks would put eight red items on
    /// screen. That screen's job is to help a person find an SD card.
    ///
    /// This test therefore counts the destructive color. On a listing of six disks,
    /// it belongs to the `refused` verdict and nothing else. The reason beside it
    /// is a fact and is drawn quiet. The warning after the grid is caution, not
    /// destruction. Each element still states its meaning in words, which is what
    /// makes it safe to take the color away.
    #[test]
    fn the_destructive_color_is_spent_only_on_the_verdict() {
        let mut app = an_app();
        let system: Vec<BlockDevice> = ["/dev/dm-0", "/dev/dm-1", "/dev/dm-2", "/dev/nvme0n1"]
            .into_iter()
            .map(|node| {
                let mut disk = a_disk(node, 1 << 40);
                disk.carries_running_system = true;
                disk
            })
            .collect();
        let mut held = a_disk("/dev/sdc", 64 << 30);
        held.mounts = vec!["/media/greg/CARD".to_string()];
        app.session.disks.show = true;
        app.session.disks.asked = true;
        app.session.disks.listed = system
            .into_iter()
            .chain([held, a_disk("/dev/zram0", 16 << 30)])
            .collect();

        // The installed palette's destructive color, not a literal: the test
        // must follow the theme rather than pin a hex nobody would remember to
        // change here.
        let themed = egui::Context::default();
        crate::theme::install(&themed);
        let danger = themed.style_of(themed.theme()).visuals.error_fg_color;
        let runs = colored(|ui| draw(&mut app, ui));
        let hot: Vec<&String> = runs
            .iter()
            .filter(|(_, color)| *color == danger)
            .map(|(text, _)| text)
            .collect();

        assert!(
            hot.iter().all(|text| text.as_str() == "refused"),
            "the destructive color belongs to the verdict and to nothing else \
             on this screen: {hot:?}"
        );
        assert_eq!(hot.len(), 4, "one per refused disk, and no more: {hot:?}");
    }

    /// Every obstacle a person can clear is named, and an absolute one stands alone.
    ///
    /// The two cases differ in kind. A clearable obstacle shown by itself costs a
    /// round trip: unmount, retry, discover the card is write-protected, fix that,
    /// retry. A disk carrying two of them is therefore named under both. Each
    /// sentence says what that one obstacle clears, and does not promise the disk
    /// is then usable.
    ///
    /// An obstacle that cannot be cleared makes the others irrelevant. Listing
    /// steps past an impassable point implies the point is passable. The running
    /// system, the one refusal with no override anywhere, is therefore shown on its
    /// own, even on a disk that is also mounted.
    #[test]
    fn every_clearable_obstacle_is_named_and_an_absolute_one_stands_alone() {
        let mut app = an_app();

        // The running system, and it is mounted as well -- which a root filesystem
        // always is. Only the absolute obstacle should be drawn for it.
        let mut system = a_disk("/dev/dm-0", 1 << 40);
        system.carries_running_system = true;
        system.mounts = vec!["/".to_string()];

        let mut read_only = a_disk("/dev/sdb", 32 << 30);
        read_only.read_only = true;
        let mut held = a_disk("/dev/sdc", 64 << 30);
        held.mounts = vec!["/media/greg/CARD".to_string()];

        // Two clearable obstacles at once: a write-protected card that is also
        // mounted. Both have to go, and both are somebody's to clear.
        let mut both = a_disk("/dev/sdd", 8 << 30);
        both.read_only = true;
        both.mounts = vec!["/mnt/x".to_string()];

        app.session.disks.show = true;
        app.session.disks.asked = true;
        app.session.disks.listed = vec![system, read_only, held, both];

        let words = words(&mut app);
        let list_after = |label: &str| -> String {
            let lines: Vec<&str> = words.lines().collect();
            let at = lines
                .iter()
                .position(|line| *line == label)
                .unwrap_or_else(|| panic!("no {label:?} group in {words}"));
            lines[at + 1].to_string()
        };

        // The absolute one stands alone: named under its own reason, and nowhere
        // else, even though it is mounted.
        assert_eq!(list_after("running system:"), "/dev/dm-0");
        assert!(
            !list_after("mounted:").contains("/dev/dm-0"),
            "the running system is not offered a remedy that cannot help: {words}"
        );

        // The clearable ones are complete: the disk with two is under both.
        assert_eq!(list_after("read-only:"), "/dev/sdb, /dev/sdd");
        assert_eq!(list_after("mounted:"), "/dev/sdc, /dev/sdd");

        // And that it has two is said, rather than left to be noticed by somebody
        // scanning for their own device name.
        assert!(
            words.contains("Listed under more than one reason, and each one must be cleared")
                && words.contains("/dev/sdd"),
            "a disk with two obstacles says so: {words}"
        );

        // No sentence promises that clearing one obstacle is enough.
        assert!(
            !words.contains("and open it again") && !words.contains("and open them again"),
            "no sentence promises the disk is usable after one step: {words}"
        );
        // The write-protect switch is named, because on this tool's media it is
        // almost always the cause and it is not otherwise discoverable.
        assert!(
            words.contains("write-protect switch"),
            "read-only says where to look: {words}"
        );
    }

    /// The flow bar is a tab bar to an assistive technology, not two buttons.
    ///
    /// egui has no tab widget. `Button::selectable` reaches the tree as
    /// `Role::Button` with a `Toggled`. A screen reader then says "pressed", and
    /// nothing about being one of a set or which one is current. The roles exist in
    /// AccessKit and the node builder can set them. This test pins the result: a
    /// `TabList` over `Tab` children, with exactly one of them selected.
    ///
    /// The structure matters most. A `Tab` with no `TabList` parent is an orphan,
    /// and an assistive technology that cannot count the set cannot say "1 of 2".
    #[test]
    fn the_flow_bar_publishes_a_tab_list_and_says_which_tab_is_current() {
        use egui::accesskit::Role;

        for chosen in Tab::ALL {
            let mut app = an_app();
            app.tab = chosen;
            let tree = tree(|ui| draw(&mut app, ui));

            let tabs: Vec<_> = tree
                .0
                .iter()
                .filter(|(_, node)| node.role() == Role::Tab)
                .collect();
            assert_eq!(
                tabs.len(),
                Tab::ALL.len(),
                "every flow is published as a tab, not as a button"
            );

            let lists: Vec<_> = tree
                .0
                .iter()
                .filter(|(_, node)| node.role() == Role::TabList)
                .collect();
            assert_eq!(
                lists.len(),
                1,
                "and the tabs sit inside one list, so an AT can say which of how many"
            );
            let (_, list) = lists[0];
            assert_eq!(
                list.children().len(),
                Tab::ALL.len(),
                "the list holds every tab"
            );

            let selected: Vec<String> = tabs
                .iter()
                .filter(|(_, node)| node.is_selected() == Some(true))
                .filter_map(|(_, node)| node.label().map(str::to_owned))
                .collect();
            assert_eq!(
                selected,
                vec![chosen.name().to_string()],
                "exactly one tab is selected, and it is the one being shown"
            );
        }
    }

    /// The current tab is marked by a shape, not only by a color.
    ///
    /// The choice follows from measurement. egui paints a selected button in
    /// `selection.bg_fill`. Against this palette's canvas, that fill measures
    /// 2.11:1 in dark and 1.61:1 in light, under the 3:1 a sole non-text indicator
    /// needs. The light theme's selected text measures 2.84:1, under 4.5:1. The
    /// resting control boundary measured the same way: fills in this palette are
    /// too faint to carry an indicator.
    ///
    /// A rule is therefore drawn under the current tab. This test asserts that the
    /// rule is there and moves with the tab. A color test would pass on a palette
    /// nobody can see. SC 1.4.1 asks for a shape.
    #[test]
    fn the_current_tab_is_marked_by_a_rule_that_moves_with_it() {
        /// Every horizontal line the frame drew, as (y, x-center).
        fn rules(shapes: &[egui::epaint::ClippedShape]) -> Vec<(f32, f32)> {
            fn scan(shape: &egui::Shape, into: &mut Vec<(f32, f32)>) {
                match shape {
                    egui::Shape::LineSegment { points, stroke } => {
                        let flat = (points[0].y - points[1].y).abs() < 0.5;
                        let wide = (points[1].x - points[0].x).abs() > 8.0;
                        if flat && wide && stroke.width > 1.0 {
                            into.push((points[0].y, (points[0].x + points[1].x) / 2.0));
                        }
                    }
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            scan(shape, into);
                        }
                    }
                    _ => {}
                }
            }
            let mut found = Vec::new();
            for clipped in shapes {
                scan(&clipped.shape, &mut found);
            }
            found
        }

        fn marked(tab: Tab) -> Vec<(f32, f32)> {
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            let mut app = an_app();
            app.tab = tab;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 900.0),
                )),
                ..Default::default()
            };
            let mut first = ctx.run_ui(input.clone(), |ui| draw(&mut app, ui));
            first.textures_delta.clear();
            let mut output = ctx.run_ui(input, |ui| draw(&mut app, ui));
            output.textures_delta.clear();
            rules(&output.shapes)
        }

        let flash = marked(Tab::Flash);
        let serial = marked(Tab::Serial);

        assert!(
            !flash.is_empty(),
            "the current tab carries a rule under it, which is what marks it"
        );
        assert!(
            !serial.is_empty(),
            "and so does the other one when it is up"
        );

        // The mark moves with the tab. Comparing the *set* of rules rather than
        // one of them keeps this honest if another rule is ever drawn: what must
        // change is where the marks are, and if this ever stops differing the two
        // screens are marking the same place.
        assert_ne!(
            flash, serial,
            "the mark is under whichever tab is current, so it moves when the tab does"
        );
    }

    /// Widening the window does not lengthen a line of prose, and does not wrap a
    /// row of device names either.
    ///
    /// A line that runs the width of a wide monitor is hard to read. The eye must
    /// travel back across the whole window to find the next line. This window's
    /// prose measures 6.29 pt per character. An unheld paragraph is
    /// therefore 121 characters wide at 800 pt, and 541 maximized on an ultrawide.
    /// A comfortable measure is 45 to 75.
    ///
    /// The second half keeps the first from overreaching. **The cap is for text
    /// that is language, not for text that is data.** A list of device nodes after
    /// a safety warning is scanned, not read. Wrapping `/dev/nvme0n1, /dev/dm-0`
    /// across two lines to satisfy a typographic rule would make the device a
    /// person is looking for harder to find.
    #[test]
    fn prose_is_held_to_a_measure_and_a_row_of_names_is_not() {
        /// Every text run a frame laid out, as (width, rows, text), at a given
        /// window width.
        ///
        /// The row count says whether a run wrapped. egui wraps a galley into rows
        /// and leaves its text alone. A newline search in the string therefore
        /// finds nothing, however many lines the run took.
        fn runs(width: f32) -> Vec<(f32, usize, String)> {
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            let mut app = an_app();
            app.show_disks(true);
            app.session.disks.asked = true;
            // Enough refused disks that their names, joined, are longer than the
            // measure -- otherwise the second half of this test would pass whether
            // or not the cap were wrongly applied to them.
            let mut listed: Vec<BlockDevice> = ["nvme0n1", "dm-0", "dm-1", "dm-2", "dm-3", "dm-4"]
                .iter()
                .map(|name| {
                    let mut disk = a_disk(&format!("/dev/{name}"), 2 << 40);
                    disk.carries_running_system = true;
                    disk
                })
                .collect();
            listed.push(a_disk("/dev/sdb", 32 << 30));
            app.session.disks.listed = listed;

            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 1400.0),
                )),
                ..Default::default()
            };
            let mut first = ctx.run_ui(input.clone(), |ui| draw(&mut app, ui));
            first.textures_delta.clear();
            let mut out = ctx.run_ui(input, |ui| draw(&mut app, ui));
            out.textures_delta.clear();

            let mut runs = Vec::new();
            for clipped in &out.shapes {
                if let egui::Shape::Text(text) = &clipped.shape {
                    runs.push((
                        text.galley.size().x,
                        text.galley.rows.len(),
                        text.galley.text().to_string(),
                    ));
                }
            }
            runs
        }

        fn find(runs: &[(f32, usize, String)], starts_with: &str) -> (f32, usize) {
            let (width, rows, _) = runs
                .iter()
                .find(|(_, _, text)| text.starts_with(starts_with))
                .unwrap_or_else(|| panic!("no run starting {starts_with:?}"));
            (*width, *rows)
        }

        let ordinary = runs(1100.0);
        let ultrawide = runs(3440.0);

        // The paragraph and the safety warning are language, and neither grows.
        for opening in [
            "A disk is acted on only when it is named",
            "The disks below hold the running system",
        ] {
            let (narrow, _) = find(&ordinary, opening);
            let (wide, _) = find(&ultrawide, opening);
            assert!(
                narrow <= crate::theme::MEASURE + 1.0,
                "{opening:?} is held to the measure: {narrow} pt"
            );
            assert!(
                (narrow - wide).abs() < 1.0,
                "{opening:?} does not grow with the window: {narrow} pt at 1100, {wide} pt at 3440"
            );
        }

        // The device names under that warning are data, and stay on one line -- at
        // the narrow window too, where the measure would otherwise bite first.
        //
        // The run wanted is the **joined list**, not one of the table cells that
        // also begin with a node name: those are short and never wrap, so matching
        // one of them would pass whether or not the cap were wrongly applied here.
        for (label, runs) in [("1100", &ordinary), ("3440", &ultrawide)] {
            let (width, rows, text) = runs
                .iter()
                .find(|(_, _, text)| text.contains(", /dev/"))
                .expect("the refused disks are named together under the warning");
            assert!(
                *width > crate::theme::MEASURE,
                "the list under test has to be longer than the measure or this proves nothing: \
                 {width} pt, {text:?}"
            );
            assert_eq!(
                *rows, 1,
                "at {label} pt the refused disks are named on one line, not wrapped to satisfy a \
                 rule meant for prose: {text:?}"
            );
        }
    }

    /// The page stops growing, which governs the elements that span it.
    ///
    /// On a maximized ultrawide, a hairline running three thousand points beside a
    /// column of controls six hundred wide looks unfinished. Below the cap, the
    /// page is the window width. Above it, the page is the cap, anchored left, so a
    /// resize moves nothing sideways.
    #[test]
    fn the_page_stops_growing_and_stays_anchored_left() {
        fn widest_rule(width: f32) -> (f32, f32) {
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            let mut app = an_app();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 900.0),
                )),
                ..Default::default()
            };
            let mut first = ctx.run_ui(input.clone(), |ui| draw(&mut app, ui));
            first.textures_delta.clear();
            let mut out = ctx.run_ui(input, |ui| draw(&mut app, ui));
            out.textures_delta.clear();

            let mut left = f32::MAX;
            let mut span = 0.0f32;
            for clipped in &out.shapes {
                if let egui::Shape::LineSegment { points, .. } = &clipped.shape
                    && (points[0].y - points[1].y).abs() < 0.5
                {
                    let w = points[1].x - points[0].x;
                    if w > span {
                        span = w;
                    }
                    left = left.min(points[0].x);
                }
            }
            (span, left)
        }

        let margin = f32::from(crate::theme::PAGE_MARGIN.left);

        // Below the cap the rules span the window, less the page's own inset.
        let (narrow, narrow_left) = widest_rule(900.0);
        assert!(
            (narrow - (900.0 - 2.0 * margin)).abs() < 1.0,
            "a window narrower than the cap gets the whole of itself: {narrow} pt"
        );

        // Above it they stop, and the page is still against the left margin --
        // centring would have moved this to (3440 - PAGE_WIDTH) / 2.
        let (wide, wide_left) = widest_rule(3440.0);
        assert!(
            (wide - crate::theme::PAGE_WIDTH).abs() < 1.0,
            "a window wider than the cap gets the cap: {wide} pt"
        );
        assert!(
            (narrow_left - margin).abs() < 1.0 && (wide_left - margin).abs() < 1.0,
            "and the page starts at the same place either way: {narrow_left} pt, {wide_left} pt"
        );
    }

    /// A tab is not drawn as a button, and the rail marks the difference.
    ///
    /// The two are the same widget. A tab is a `Button`, because a `Button`
    /// carries the padding that keeps the target big enough to hit. What separates
    /// them is what is painted. A tab has no frame at rest and stands on a rail
    /// that runs the width of the page. An ordinary button has a frame and stands
    /// on nothing.
    ///
    /// The test compares against a real button drawn on the same screen, not a
    /// color written here, so a palette change moves both together.
    #[test]
    fn a_tab_is_drawn_without_a_frame_and_stands_on_a_rail() {
        fn frames(app: &mut App) -> (Vec<egui::Rect>, Vec<egui::Rect>, Vec<(f32, f32)>) {
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 700.0),
                )),
                ..Default::default()
            };
            let mut first = ctx.run_ui(input.clone(), |ui| draw(app, ui));
            first.textures_delta.clear();
            let mut out = ctx.run_ui(input, |ui| draw(app, ui));
            out.textures_delta.clear();

            let (mut bare, mut framed, mut rails): (
                Vec<egui::Rect>,
                Vec<egui::Rect>,
                Vec<(f32, f32)>,
            ) = (Vec::new(), Vec::new(), Vec::new());
            for clipped in &out.shapes {
                match &clipped.shape {
                    // The page's own background is the window, not a control.
                    egui::Shape::Rect(rect) if rect.rect.width() < 900.0 => {
                        let invisible = rect.fill.a() == 0 && rect.stroke.width == 0.0;
                        if invisible {
                            bare.push(rect.rect);
                        } else {
                            framed.push(rect.rect);
                        }
                    }
                    egui::Shape::LineSegment { points, .. }
                        if (points[0].y - points[1].y).abs() < 0.5 =>
                    {
                        rails.push((points[0].y, points[1].x - points[0].x));
                    }
                    _ => {}
                }
            }
            (bare, framed, rails)
        }

        // Button-sized: tall enough to be a control and short enough not to be the
        // scroll area, which is also drawn as a rectangle nothing is painted into.
        fn is_tab_sized(rect: &egui::Rect) -> bool {
            (24.0..40.0).contains(&rect.height()) && rect.width() < 400.0
        }

        let mut app = an_app();
        let (bare, framed, rails) = frames(&mut app);

        // Both tabs draw a rectangle and neither paints anything into it.
        let tab_row = bare.iter().filter(|rect| is_tab_sized(rect)).count();
        assert!(
            tab_row >= Tab::ALL.len(),
            "each tab draws its rectangle and fills none of it; found {tab_row} bare"
        );

        // And the screen does still draw framed controls, so the assertion above
        // is about the tabs rather than about a palette that paints nothing.
        assert!(
            !framed.is_empty(),
            "an ordinary button on the same screen keeps its frame"
        );

        // The rail runs the width of the page **under the tabs**, which is what a
        // row of words needs in order to read as tabs rather than as links. The
        // position is half the assertion: the screen's header draws a full-width
        // separator of its own, and a test that only counted widths would be
        // satisfied by that one and would pass with no rail at all.
        let page = 1000.0 - 2.0 * f32::from(crate::theme::PAGE_MARGIN.left);
        let foot = bare
            .iter()
            .filter(|rect| is_tab_sized(rect))
            .map(|rect| rect.bottom())
            .fold(f32::MIN, f32::max);
        assert!(
            rails
                .iter()
                .any(|(y, width)| *y > foot && (width - page).abs() < 1.0),
            "a full-width rail is drawn under the tabs, which end at {foot}; lines seen: {rails:?}"
        );
    }

    /// A job running on the tab nobody is looking at still offers its Cancel.
    ///
    /// This is the one hazard the tab split introduces that is not cosmetic.
    /// Progress out of sight is a nuisance. Cancel out of sight takes away the one
    /// control a person has over a write already under way. The strip is therefore
    /// drawn outside the tabs, and this test pins it from the tab the job is not
    /// on.
    #[test]
    fn a_job_on_the_other_tab_still_shows_its_progress_and_its_cancel() {
        use pyrographer_core::transport::testing::{ScriptedSerial, SerialStep};

        let mut app = an_app();
        app.session
            .start_console(
                ScriptedSerial::new(vec![SerialStep::Rx(b"=> ".to_vec())]),
                &crate::state::ConsoleLine {
                    port: "/dev/ttyUSB0".to_string(),
                    baud: 115_200,
                    prompt: pyrographer_core::uboot::DEFAULT_PROMPT.to_string(),
                    reads: 8,
                },
                crate::state::ConsoleTask::Watch {
                    expect: vec![b"PASS".to_vec()],
                    fail: vec![],
                },
                0.0,
                Box::new(|| {}),
            )
            .expect("nothing else is running");

        // The console lives on the serial side. Look at the other one.
        assert!(app.session.anything_running(), "the session is running");

        let words = words_on(&mut app, Tab::Flash);
        assert!(
            words.contains("Cancel"),
            "the cancel is reachable from the tab the job is not on: {words}"
        );
        assert!(
            words.contains(Tab::Serial.name()),
            "and the strip says which side to look at for the detail: {words}"
        );

        // A control called only "Cancel" would not say which job it stops, and two
        // can run at once.
        let tree = tree(|ui| draw(&mut app, ui));
        let cancels: Vec<String> = tree
            .controls()
            .into_iter()
            .filter_map(|node| tree.announced(node))
            .filter(|name| name.starts_with("Cancel"))
            .collect();
        assert!(
            cancels.iter().any(|name| name != "Cancel"),
            "the strip's cancel names the job it stops: {cancels:?}"
        );
    }

    /// With nothing running, no strip is drawn at all.
    ///
    /// Checking only that no Cancel is offered is not enough. An empty panel would
    /// pass that check while still painting a body and a separator along the bottom
    /// of the window every frame. A permanent empty status bar is chrome, and a
    /// person learns to stop reading chrome, including on the day it has something
    /// to say. The assertion is therefore geometric: with nothing running, the
    /// bottom of the window is untouched.
    #[test]
    fn an_idle_window_draws_nothing_along_its_bottom_edge() {
        const HEIGHT: f32 = 700.0;
        // Generous enough that the assertion is about the strip and not about a
        // pixel: the strip's own body is ~19 px plus its separator.
        const FOOT: f32 = HEIGHT - 60.0;

        fn lowest(app: &mut App) -> f32 {
            fn scan(shape: &egui::Shape, low: &mut f32) {
                match shape {
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            scan(shape, low);
                        }
                    }
                    // The central panel's own background is the window, not
                    // content drawn into it, so it is not evidence of a strip.
                    egui::Shape::Rect(rect) if rect.rect.width() >= 999.0 => {}
                    other => {
                        let rect = other.visual_bounding_rect();
                        if rect.is_finite() && rect.max.y > *low {
                            *low = rect.max.y;
                        }
                    }
                }
            }

            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, HEIGHT),
                )),
                ..Default::default()
            };
            let mut first = ctx.run_ui(input.clone(), |ui| draw(app, ui));
            first.textures_delta.clear();
            let mut out = ctx.run_ui(input, |ui| draw(app, ui));
            out.textures_delta.clear();

            let mut low = 0.0;
            for clipped in &out.shapes {
                scan(&clipped.shape, &mut low);
            }
            low
        }

        let mut idle = an_app();
        assert!(!idle.session.anything_running());
        let quiet = lowest(&mut idle);
        assert!(
            quiet < FOOT,
            "nothing is running, so nothing is drawn along the bottom: something reaches {quiet}"
        );

        // And the same window with a job in it does reach down there, so the
        // measurement above is of an absent strip rather than of a screen too
        // short to have one.
        let mut busy = an_app();
        busy.session
            .start_console(
                pyrographer_core::transport::testing::ScriptedSerial::new(vec![]),
                &crate::state::ConsoleLine {
                    port: "/dev/ttyUSB0".to_string(),
                    baud: 115_200,
                    prompt: pyrographer_core::uboot::DEFAULT_PROMPT.to_string(),
                    reads: 8,
                },
                crate::state::ConsoleTask::Watch {
                    expect: vec![b"PASS".to_vec()],
                    fail: vec![],
                },
                0.0,
                Box::new(|| {}),
            )
            .expect("nothing else is running");
        let running = lowest(&mut busy);
        assert!(
            running > FOOT,
            "a job in flight puts the strip along the bottom: it only reaches {running}"
        );
    }
}
