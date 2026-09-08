//! The window.
//!
//! Everything here is presentation and thread plumbing. What may be written,
//! and what happens when it is, lives in `argos-session` -- the same code the
//! CLI runs, so there is one implementation of "may I write to this disk?"
//! rather than one per interface.

use crate::devices::{offerable, reconcile, Selection, SelectionOutcome};
use crate::state::{confirmation_matches, AppState, RunState, WorkerMsg};
use crate::theme::{self, metric};
use argos_core::device::Device;
use argos_platform::PlatformOps;
use argos_privileged::protocol::{Plan, WindowsLayout};
use argos_session::{
    self as session, human_size, ElevationUi, EventSink, ImageKind, Outcome, SessionEvent,
    WritePreview,
};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The app id, which the desktop uses to match a window to its `.desktop`
/// entry. Must equal `StartupWMClass` in `packaging/linux/argos.desktop`
/// (#94) or the taskbar shows a generic icon instead of ours.
pub const APP_ID: &str = "argos-gui";

/// How often the device list is refreshed while the user is choosing.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct ArgosApp {
    platform: Arc<dyn PlatformOps + Send + Sync>,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    state: AppState,

    devices: Vec<Device>,
    device_error: Option<String>,
    selection: Option<Selection>,
    /// Set when the selected drive vanished or was swapped for another.
    selection_warning: Option<String>,
    show_all_devices: bool,
    enumerating: bool,
    last_enumeration: Option<Instant>,

    iso: Option<PathBuf>,
    iso_kind: Option<ImageKind>,
    iso_error: Option<String>,
    classifying: bool,

    /// The layout checkbox: unchecked is GPT/UEFI, the CLI's default.
    bios_layout: bool,

    /// Which theme is currently applied, so the palette is rebuilt only when
    /// the system actually switches rather than every frame.
    applied_dark: Option<bool>,
}

impl ArgosApp {
    pub fn new(platform: Arc<dyn PlatformOps + Send + Sync>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        ArgosApp {
            platform,
            tx,
            rx,
            state: AppState::Idle,
            devices: Vec::new(),
            device_error: None,
            selection: None,
            selection_warning: None,
            show_all_devices: false,
            enumerating: false,
            last_enumeration: None,
            iso: None,
            iso_kind: None,
            iso_error: None,
            classifying: false,
            bios_layout: false,
            applied_dark: None,
        }
    }

    fn layout(&self) -> WindowsLayout {
        if self.bios_layout {
            WindowsLayout::Fat32Bios
        } else {
            WindowsLayout::Fat32
        }
    }

    /// Enumeration runs on a worker because it is not cheap: on macOS it is a
    /// `diskutil` per disk, on Linux sysfs plus the udev database plus
    /// `/proc/mounts` plus possibly UDisks2 over D-Bus.
    ///
    /// Single-flighted, and **never while a write is running**: on macOS that
    /// would mean `diskutil` poking a disk the helper holds `O_EXCL`, and
    /// this project has already been bitten by Disk Arbitration touching a
    /// freshly written partition. That is a correctness rule, not a
    /// performance one.
    fn maybe_enumerate(&mut self, ctx: &egui::Context, force: bool) {
        if self.enumerating || matches!(self.state, AppState::Running(_)) {
            return;
        }
        let due = self
            .last_enumeration
            .is_none_or(|at| at.elapsed() >= POLL_INTERVAL);
        if !(force || due) {
            return;
        }

        self.enumerating = true;
        self.last_enumeration = Some(Instant::now());
        let platform = Arc::clone(&self.platform);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(WorkerMsg::Devices(platform.list_removable_disks()));
            ctx.request_repaint();
        });
    }

    fn classify(&mut self, ctx: &egui::Context, iso: PathBuf) {
        self.iso = Some(iso.clone());
        self.iso_kind = None;
        self.iso_error = None;
        self.classifying = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        // Off the UI thread: for a Windows installer this reads the image's
        // whole directory tree.
        std::thread::spawn(move || {
            let result = session::canonicalize_iso_path(&iso)
                .and_then(|resolved| session::classify_image(&resolved));
            let _ = tx.send(WorkerMsg::Classified(result));
            ctx.request_repaint();
        });
    }

    fn begin_preparation(&mut self, ctx: &egui::Context) {
        let (Some(iso), Some(selection)) = (self.iso.clone(), self.selection.clone()) else {
            return;
        };
        self.state = AppState::Preparing;
        let platform = Arc::clone(&self.platform);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let request = session::WriteRequest {
            iso,
            device_id: selection.platform_id,
            no_verify: false,
            eject: true,
            allow_non_removable: self.show_all_devices,
            layout: self.layout(),
        };
        std::thread::spawn(move || {
            let result = session::prepare_write(&*platform, &request).map(Box::new);
            let _ = tx.send(WorkerMsg::Prepared(result));
            ctx.request_repaint();
        });
    }

    /// Elevates and streams a plan already built and confirmed (a write, once
    /// the user retyped the device path) or one that never needed confirming
    /// (a verify, which never writes). One worker thread, one code path for
    /// both, since from here on they are identical: `session::spawn` and
    /// `Running::stream` do not know or care which they were handed.
    ///
    /// Always `ElevationUi::Graphical`: a windowed app has no controlling
    /// terminal for `sudo` to ask a password on, which is the whole reason
    /// that route exists (#90/#100).
    fn spawn_run(tx: Sender<WorkerMsg>, ctx: egui::Context, plan: Plan) {
        std::thread::spawn(move || {
            match session::spawn(&plan, ElevationUi::Graphical) {
                Ok(running) => {
                    let canceller = running.canceller();
                    let _ = tx.send(WorkerMsg::Started(canceller));
                    ctx.request_repaint();
                    let mut sink = ThrottledSink::new(tx.clone(), ctx.clone());
                    let outcome = running.stream(&mut sink);
                    let _ = tx.send(WorkerMsg::Finished(outcome));
                }
                Err(err) => {
                    let _ = tx.send(WorkerMsg::Finished(Err(err)));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Verify's counterpart to `begin_preparation`+the confirm click: no
    /// confirmation step exists for it (it never writes), so preparing and
    /// starting happen in one worker-thread lifetime rather than a
    /// round-trip through the UI. The moment the thread starts, the screen
    /// already shows the running view -- its "working out the total" spinner
    /// (`fraction() == None`, since `bytes_total` starts at 0) *is* what
    /// preparing a verify looks like; a separate spinner state would only
    /// duplicate it.
    fn begin_verification(&mut self, ctx: &egui::Context) {
        let (Some(iso), Some(selection)) = (self.iso.clone(), self.selection.clone()) else {
            return;
        };
        let device_id = selection.platform_id.clone();
        self.state = AppState::Running(RunState::for_verify(device_id));
        let platform = Arc::clone(&self.platform);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let layout = self.layout();
        std::thread::spawn(move || {
            let request = session::VerifyRequest {
                device_id: selection.platform_id,
                iso,
                layout,
            };
            match session::prepare_verify(&*platform, &request) {
                Ok(prepared) => Self::spawn_run(tx, ctx, prepared.plan().clone()),
                Err(err) => {
                    let _ = tx.send(WorkerMsg::Finished(Err(err)));
                    ctx.request_repaint();
                }
            }
        });
    }

    fn take_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Devices(result) => {
                    self.enumerating = false;
                    match result {
                        Ok(devices) => {
                            self.device_error = None;
                            self.apply_device_list(devices);
                        }
                        Err(err) => self.device_error = Some(err.to_string()),
                    }
                }
                WorkerMsg::Classified(result) => {
                    self.classifying = false;
                    match result {
                        Ok(kind) => {
                            self.iso_kind = kind;
                            self.iso_error = kind
                                .is_none()
                                .then(|| "Not an image Argos recognizes".to_string());
                        }
                        Err(err) => {
                            self.iso_kind = None;
                            self.iso_error = Some(err.to_string());
                        }
                    }
                }
                other => self.state = crate::state::reduce(std::mem::take(&mut self.state), other),
            }
        }
    }

    /// Keeps the selection pointing at the same physical drive, or says so
    /// when it cannot. Never silently re-points at whatever is now first in
    /// the list.
    fn apply_device_list(&mut self, devices: Vec<Device>) {
        if let Some(selection) = &self.selection {
            match reconcile(selection, &devices) {
                SelectionOutcome::Unchanged => self.selection_warning = None,
                SelectionOutcome::Gone => {
                    self.selection_warning =
                        Some(format!("{} is no longer present", selection.platform_id));
                }
                SelectionOutcome::Replaced => {
                    // A different drive answering to the same path is exactly
                    // what the helper refuses on, so drop the selection
                    // rather than let it be confirmed.
                    let path = selection.platform_id.clone();
                    self.selection = None;
                    self.selection_warning = Some(format!(
                        "A different drive is now at {path}; selection cleared"
                    ));
                }
            }
        }
        self.devices = devices;
    }

    fn selected_device(&self) -> Option<&Device> {
        let selection = self.selection.as_ref()?;
        self.devices
            .iter()
            .find(|d| d.platform_id == selection.platform_id)
    }
}

impl eframe::App for ArgosApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_theme(ctx);
        // Drained in full, not one per frame: a write emits thousands of
        // progress events and falling behind would show stale numbers.
        self.take_messages();
        self.maybe_enumerate(ctx, false);
        self.accept_dropped_iso(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            self.draw_header(ui);
            ui.add_space(metric::GAP_GROUPS);
            match &self.state {
                AppState::Idle | AppState::Preparing | AppState::AwaitingConfirmation { .. } => {
                    self.draw_chooser(ui, ctx)
                }
                AppState::Running(_) => self.draw_progress(ui),
                AppState::Done { .. } | AppState::Failed { .. } | AppState::Cancelled => {
                    self.draw_result(ui)
                }
            }
        });

        self.draw_confirmation(ctx);

        // Only the device poll changes the screen on its own; ask for a
        // repaint when it is due rather than spinning every frame.
        if matches!(
            self.state,
            AppState::Idle | AppState::AwaitingConfirmation { .. }
        ) {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
    }
}

impl ArgosApp {
    /// Follows the desktop's light/dark setting, rebuilding the palette only
    /// when it actually changes.
    fn sync_theme(&mut self, ctx: &egui::Context) {
        let dark = ctx.theme() == egui::Theme::Dark;
        if self.applied_dark != Some(dark) {
            theme::apply(ctx, dark);
            self.applied_dark = Some(dark);
        }
    }

    /// The top band: the name, and the space the dog will live in.
    ///
    /// That space is deliberate, not leftover. A sprite animation belongs
    /// here eventually (168×42, hence this band's height); it is not built
    /// yet because the frames need an illustrator, it would be a vendored
    /// binary asset in a project that refused one for a boot record, and it
    /// would force a continuous repaint where the window currently redraws
    /// every two seconds. Please do not reclaim the gap as unused layout.
    ///
    /// The language selector belongs at the right of this band and arrives
    /// with i18n in G6 (#93); a selector that switched nothing would be a
    /// lie on screen.
    fn draw_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(metric::HEADER_HEIGHT);
            ui.heading("Argos");
            ui.add_space(metric::GAP_ROW);
            ui.allocate_space(egui::vec2(metric::SPRITE[0], metric::SPRITE[1]));
        });
    }

    /// eframe reports dropped files itself; accepting only the first means a
    /// multi-file drop behaves like picking one, rather than silently using
    /// an arbitrary member.
    fn accept_dropped_iso(&mut self, ctx: &egui::Context) {
        if !matches!(self.state, AppState::Idle) {
            return;
        }
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .and_then(|file| file.path.clone())
        });
        if let Some(path) = dropped {
            self.classify(ctx, path);
        }
    }

    /// A bordered panel, the unit the design groups fields into.
    fn group<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(metric::GROUP_MARGIN as i8))
            .fill(theme::palette_for(ui.visuals().dark_mode).panel)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui);
            });
    }

    fn draw_chooser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        Self::group(ui, |ui| self.draw_image_group(ui, ctx));
        ui.add_space(metric::GAP_GROUPS);
        Self::group(ui, |ui| self.draw_target_group(ui, ctx));
        ui.add_space(metric::GAP_GROUPS);
        Self::group(ui, |ui| self.draw_layout_group(ui));
        ui.add_space(metric::GAP_GROUPS);
        self.draw_notices(ui);

        // Actions sit at the bottom of the window, right-aligned, with Write
        // last: the destructive one is the only filled button, and the only
        // one the eye lands on.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ready = self.can_act();
                    let accent = theme::palette_for(ui.visuals().dark_mode).accent;
                    let on_accent = theme::palette_for(ui.visuals().dark_mode).on_accent;
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(egui::RichText::new("Write…").color(on_accent))
                                .fill(accent),
                        )
                        .on_disabled_hover_text(
                            "Choose an image Argos recognises and a target device",
                        )
                        .clicked()
                    {
                        self.begin_preparation(ctx);
                    }
                    // Verify reads the device back and never writes, so it
                    // gets no confirmation and no emphasis -- present, and
                    // clearly subordinate.
                    if ui
                        .add_enabled(ready, egui::Button::new("Verify…"))
                        .clicked()
                    {
                        self.begin_verification(ctx);
                    }
                    if matches!(self.state, AppState::Preparing) {
                        ui.spinner();
                    }
                });
            });
        });
    }

    fn can_act(&self) -> bool {
        self.iso_kind.is_some()
            && self.selection.is_some()
            && !self.classifying
            && matches!(self.state, AppState::Idle)
    }

    fn draw_image_group(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label("Image");
        ui.horizontal(|ui| {
            // Called on the UI thread deliberately: on macOS NSOpenPanel must
            // be driven from the main thread. One frozen frame while a modal
            // file chooser is open is what every desktop app does.
            let button = ui.button("Choose…");
            // The file name, not the path: a deep path would otherwise set
            // the width of the whole window. The full path is a hover away,
            // and appears in full in the confirmation dialog, which is where
            // it matters.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_max_width(ui.available_width());
                match &self.iso {
                    Some(path) => {
                        ui.add(
                            egui::Label::new(egui::RichText::new(file_name_of(path)).monospace())
                                .truncate(),
                        )
                        .on_hover_text(path.display().to_string());
                    }
                    None => {
                        ui.label(egui::RichText::new("Drop an ISO here").weak());
                    }
                }
            });
            if button.clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Choose a disk image")
                    .add_filter("Disk images", &["iso", "img"])
                    .pick_file()
                {
                    self.classify(ctx, path);
                }
            }
        });

        if self.classifying {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Reading the image…").small());
            });
        } else if let Some(err) = &self.iso_error {
            ui.label(
                egui::RichText::new(format!("⚠ {err}"))
                    .small()
                    .color(ui.visuals().error_fg_color),
            );
        } else if let Some(kind) = self.iso_kind {
            let accent = theme::palette_for(ui.visuals().dark_mode).accent;
            ui.label(
                egui::RichText::new(match kind {
                    ImageKind::LinuxDd => "✔ Linux ISO (written byte for byte)",
                    ImageKind::WindowsInstaller => "✔ Windows installer",
                })
                .small()
                .color(accent),
            );
        } else {
            ui.label(egui::RichText::new("No image selected.").small().weak());
        }
    }

    fn draw_target_group(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let offered: Vec<Device> = offerable(&self.devices, self.show_all_devices)
            .into_iter()
            .cloned()
            .collect();

        ui.label("Target");
        ui.horizontal(|ui| {
            let refresh = ui.button("⟳").on_hover_text("Refresh the list");
            let width = ui.available_width();
            let label = self
                .selected_device()
                .map(describe_device)
                .unwrap_or_else(|| "No device selected".into());
            egui::ComboBox::from_id_salt("target-device")
                .selected_text(label)
                .width(width)
                .show_ui(ui, |ui| {
                    if offered.is_empty() {
                        ui.label("No removable devices found");
                    }
                    for device in &offered {
                        let selected = self
                            .selection
                            .as_ref()
                            .is_some_and(|s| s.platform_id == device.platform_id);
                        if ui
                            .selectable_label(selected, describe_device_long(device))
                            .clicked()
                        {
                            self.selection = Some(Selection::of(device));
                            self.selection_warning = None;
                        }
                    }
                });
            if refresh.clicked() {
                self.maybe_enumerate(ctx, true);
            }
        });

        // Mirrors --i-know-what-im-doing. System disks stay absent in both
        // modes: `offerable` never returns one.
        ui.checkbox(
            &mut self.show_all_devices,
            egui::RichText::new(
                "Show every disk, including those the system does not consider removable",
            )
            .small(),
        );
    }

    /// The layout control. Always visible, never behind an advanced menu:
    /// which firmware the result boots is the most consequential choice in
    /// this window, and old BIOS machines are the use case the project exists
    /// for.
    fn draw_layout_group(&mut self, ui: &mut egui::Ui) {
        let applies = self.iso_kind == Some(ImageKind::WindowsInstaller);
        ui.add_enabled_ui(applies, |ui| {
            ui.checkbox(&mut self.bios_layout, "Old machine — legacy BIOS (MBR)");
        });
        // Said out loud, not merely greyed: a disabled control with no reason
        // reads as a defect.
        let explanation = if !applies {
            match self.iso_kind {
                Some(ImageKind::LinuxDd) => {
                    "A Linux ISO carries its own partition table, so there is nothing to choose."
                }
                _ => "Choose a Windows installer image to enable this.",
            }
        } else if self.bios_layout {
            "MBR with Argos's own boot records: boots on legacy BIOS, and also on UEFI \
             firmware that accepts MBR-partitioned removable media. Windows 10 only."
        } else {
            "GPT: boots only on UEFI firmware."
        };
        let colour = if applies {
            ui.visuals().weak_text_color()
        } else {
            theme::palette_for(ui.visuals().dark_mode).text_disabled
        };
        ui.label(egui::RichText::new(explanation).small().color(colour));
    }

    fn draw_notices(&mut self, ui: &mut egui::Ui) {
        let notice = |ui: &mut egui::Ui, text: String, colour: egui::Color32| {
            ui.label(egui::RichText::new(text).small().color(colour));
            ui.add_space(metric::GAP_NOTICES);
        };
        if let Some(warning) = self.selection_warning.clone() {
            notice(ui, format!("⚠ {warning}"), ui.visuals().warn_fg_color);
        }
        if self.device_error.is_some() {
            notice(
                ui,
                "⚠ Could not list devices.".into(),
                ui.visuals().error_fg_color,
            );
        }
        if let AppState::AwaitingConfirmation { prepared, .. } = &self.state {
            for note in prepared.preview.split_notes() {
                let text = format!(
                    "· {} exceeds the FAT32 4 GiB limit and will be split into {} parts",
                    note.source_path,
                    note.part_paths.len()
                );
                ui.label(egui::RichText::new(text).small().weak());
                ui.add_space(metric::GAP_NOTICES);
            }
        }
    }

    fn draw_progress(&mut self, ui: &mut egui::Ui) {
        let AppState::Running(run) = &self.state else {
            return;
        };
        Self::group(ui, |ui| {
            ui.label(phase_label(run.phase, &run.phase_label));
            match run.fraction() {
                Some(fraction) => {
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_height(metric::PROGRESS_HEIGHT)
                            .show_percentage(),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {}",
                            human_size(run.bytes_done),
                            human_size(run.bytes_total)
                        ))
                        .small()
                        .weak(),
                    );
                }
                // egui's ProgressBar has no indeterminate mode -- animate()
                // only animates the filled part -- so an unknown total gets a
                // spinner and says so, rather than a bar frozen at zero that
                // reads as stalled.
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("Working out the total…").small().weak());
                    });
                }
            }
            ui.label(
                egui::RichText::new("Do not unplug the device.")
                    .small()
                    .weak(),
            );
        });

        ui.add_space(metric::GAP_GROUPS);

        // Verify shows no Cancel button at all -- execute_verify never
        // consults a CancelToken, so a disabled button here would carry the
        // same false promise a live one during a write's uncancellable phase
        // would (#104). Nothing to click is more honest than something that
        // looks permanently stuck.
        if run.is_verify {
            ui.label(
                egui::RichText::new("Verification cannot be interrupted.")
                    .small()
                    .weak(),
            );
            return;
        }

        let cancellable = run.is_cancellable();
        let requested = run.cancel_requested;
        ui.add_enabled_ui(cancellable, |ui| {
            if ui.button("Cancel").clicked() {
                if let AppState::Running(run) = &mut self.state {
                    run.cancel_requested = true;
                    if let Some(canceller) = &run.canceller {
                        canceller.cancel();
                    }
                }
            }
        });
        // The honest part, and the reason #104 exists: past the copy loop a
        // cancel is not delayed, it is discarded. Saying which side of that
        // line the run is on beats a button that looks live and does nothing.
        let (text, colour) = if requested {
            (
                "Interrupting. The device will be unusable and will need to be written again.",
                ui.visuals().warn_fg_color,
            )
        } else if cancellable {
            (
                "Interrupting is still possible.",
                ui.visuals().weak_text_color(),
            )
        } else {
            (
                "Past the point where interrupting is possible.",
                ui.visuals().weak_text_color(),
            )
        };
        ui.label(egui::RichText::new(text).small().color(colour));
    }

    fn draw_result(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        match &self.state {
            AppState::Done { outcome, device_id } => {
                let device_id = device_id.clone();
                let summary = describe_outcome(outcome);
                Self::group(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!("✔ Done. {summary}"))
                            .color(theme::palette_for(dark).accent),
                    );
                    ui.label(
                        egui::RichText::new(format!("Ejected {device_id}. Safe to unplug."))
                            .small()
                            .weak(),
                    );
                });
            }
            AppState::Cancelled => {
                Self::group(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Cancelled. The device is unusable and needs to be written again.",
                        )
                        .color(ui.visuals().warn_fg_color),
                    );
                });
            }
            AppState::Failed { error } => {
                let detail = error.to_string();
                egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::same(metric::GROUP_MARGIN as i8))
                    .fill(theme::palette_for(dark).error_panel)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(&detail).color(theme::palette_for(dark).error),
                        );
                        // The raw text is what a bug report needs, so it is
                        // always reachable even once these messages are
                        // translated (G6).
                        ui.collapsing("Details", |ui| {
                            ui.label(egui::RichText::new(&detail).monospace().small());
                        });
                    });
            }
            _ => {}
        }
        ui.add_space(metric::GAP_GROUPS);
        if ui.button("Start over").clicked() {
            self.state = AppState::Idle;
        }
    }

    /// The same block the CLI prints, ending in the same guard: the button
    /// stays disabled until the device path is retyped exactly. Rufus only
    /// asks for OK; Argos stays stricter.
    fn draw_confirmation(&mut self, ctx: &egui::Context) {
        let AppState::AwaitingConfirmation { prepared, typed } = &mut self.state else {
            return;
        };
        let device = prepared.device.clone();
        let iso = prepared.iso.clone();
        let preview_line = describe_preview(&prepared.preview);
        let notes: Vec<String> = prepared
            .preview
            .split_notes()
            .into_iter()
            .map(|n| {
                format!(
                    "{} will be split into {} parts ({})",
                    n.source_path,
                    n.part_paths.len(),
                    n.part_paths.join(", ")
                )
            })
            .collect();

        let mut confirmed = false;
        let mut cancelled = false;
        let mut typed_now = typed.clone();
        // Cloned now, while `prepared` is still in scope: the guard below
        // ends its borrow of `self.state` at `typed`'s last use, and
        // `self.state` is what gets overwritten once confirmed.
        let plan = prepared.plan().clone();

        egui::Window::new("Confirm")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("About to overwrite:");
                // Monospace here is load-bearing, not decoration: the guard
                // below is an exact match, and /dev/disk4 has to be
                // distinguishable from /dev/diskl.
                ui.monospace(format!("{} ({})", device.platform_id, device.display_name));
                ui.label(format!("Size: {}", human_size(device.size_bytes)));
                ui.label(format!(
                    "Serial number: {}",
                    device.serial.as_deref().unwrap_or("unknown")
                ));
                // The one place the whole path is shown, rather than elided.
                ui.label("Image:");
                ui.monospace(iso.display().to_string());
                ui.label(preview_line);
                for note in &notes {
                    ui.label(egui::RichText::new(note).small().weak());
                }
                ui.separator();
                ui.label(
                    egui::RichText::new(format!(
                        "THIS WILL PERMANENTLY ERASE all data on {}.",
                        device.platform_id
                    ))
                    .color(ui.visuals().error_fg_color),
                );
                ui.label(format!(
                    "Type the device path ({}) to confirm:",
                    device.platform_id
                ));
                ui.add(egui::TextEdit::singleline(&mut typed_now).font(egui::TextStyle::Monospace));
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                    let matches = confirmation_matches(&typed_now, &device.platform_id);
                    if ui
                        .add_enabled(matches, egui::Button::new("Write"))
                        .on_disabled_hover_text("Retype the device path exactly")
                        .clicked()
                    {
                        confirmed = true;
                    }
                });
            });

        *typed = typed_now;
        if cancelled {
            self.state = AppState::Idle;
        } else if confirmed {
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            self.state = AppState::Running(RunState::new(device.platform_id, 0));
            Self::spawn_run(tx, ctx, plan);
        }
    }
}

/// Just the file name, for a row that must not set the window's width.
/// Forwards each helper event to the UI thread, throttling `request_repaint`
/// rather than calling it per event.
///
/// A single write emits an `Event::Progress` roughly once per 1--4MiB block,
/// so a multi-gigabyte write is thousands of them; a `.swm` split or a
/// per-file Windows copy produces far more. Repainting on every one would
/// make the window unusable. The message itself is still sent immediately --
/// only the repaint request is throttled -- so the next frame, whenever it
/// comes, always has the freshest numbers.
struct ThrottledSink {
    tx: Sender<WorkerMsg>,
    ctx: egui::Context,
    last_repaint: Instant,
}

const REPAINT_INTERVAL: Duration = Duration::from_millis(33);

impl ThrottledSink {
    fn new(tx: Sender<WorkerMsg>, ctx: egui::Context) -> Self {
        ThrottledSink {
            tx,
            ctx,
            last_repaint: Instant::now(),
        }
    }
}

impl EventSink for ThrottledSink {
    fn on_event(&mut self, event: SessionEvent) {
        let _ = self.tx.send(WorkerMsg::Event(event));
        if self.last_repaint.elapsed() >= REPAINT_INTERVAL {
            self.ctx.request_repaint();
            self.last_repaint = Instant::now();
        }
    }

    fn on_finished(&mut self, _outcome: &Outcome) {
        // The terminal event always gets an unconditional repaint: the run
        // just ended and the result screen must not wait out the throttle.
        self.ctx.request_repaint();
    }

    fn on_failed(&mut self) {
        self.ctx.request_repaint();
    }
}

fn file_name_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The phase, worded for a person rather than as the `Debug` name the CLI
/// prints. Being able to do this is what typing `Phase` on the wire bought
/// (#89): the label is chosen here, not baked into the protocol.
fn phase_label(phase: Option<argos_core::progress::Phase>, fallback: &str) -> String {
    use argos_core::progress::Phase;
    match phase {
        Some(Phase::Unmounting) => "Unmounting".into(),
        Some(Phase::Checksumming) => "Checksumming".into(),
        Some(Phase::Partitioning) => "Partitioning".into(),
        Some(Phase::FormattingFat32) => "Formatting".into(),
        Some(Phase::CopyingFiles) => "Copying files".into(),
        Some(Phase::Writing) => "Writing".into(),
        Some(Phase::Flushing) => "Flushing".into(),
        Some(Phase::Verifying) => "Verifying".into(),
        // A helper new enough to send a phase this build does not know: show
        // what it sent rather than nothing.
        None if fallback.is_empty() => "Starting…".into(),
        None => fallback.to_string(),
    }
}

fn describe_device(device: &Device) -> String {
    format!("{} — {}", device.platform_id, human_size(device.size_bytes))
}

/// The fuller form, for the open dropdown, where there is room and where the
/// model name is what distinguishes two similar-looking paths.
fn describe_device_long(device: &Device) -> String {
    format!(
        "{} — {} ({})",
        device.platform_id,
        device.display_name,
        human_size(device.size_bytes)
    )
}

fn describe_preview(preview: &WritePreview) -> String {
    match preview {
        WritePreview::Dd { image_size_bytes } => {
            format!("Image size: {}", human_size(*image_size_bytes))
        }
        WritePreview::Windows { layout, .. } => format!(
            "One {} FAT32 partition at offset {}",
            human_size(layout.windows_partition.size_bytes),
            human_size(layout.windows_partition.start_offset_bytes)
        ),
    }
}

fn describe_outcome(outcome: &argos_session::Outcome) -> String {
    use argos_session::Outcome;
    match outcome {
        Outcome::DdWrite { hash } => format!("SHA-256: {hash}"),
        Outcome::Verify { hash } => format!("Verified. SHA-256: {hash}"),
        Outcome::WindowsWrite {
            files_copied,
            bytes_copied,
        } => format!(
            "{files_copied} files copied ({})",
            human_size(*bytes_copied)
        ),
        Outcome::WindowsVerify { files_verified } => format!("{files_verified} files checked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argos_core::progress::Phase;

    /// Every phase gets a human label, and none of them is empty -- an empty
    /// one would show a blank line where the user expects to be told what is
    /// happening. Exhaustive so a new variant forces a decision.
    #[test]
    fn every_phase_has_a_label() {
        for phase in [
            Phase::Unmounting,
            Phase::Checksumming,
            Phase::Writing,
            Phase::Flushing,
            Phase::Verifying,
            Phase::Partitioning,
            Phase::FormattingFat32,
            Phase::CopyingFiles,
        ] {
            assert!(!phase_label(Some(phase), "").is_empty(), "{phase:?}");
        }
    }

    /// A phase name this build does not recognise is shown as sent, rather
    /// than swallowed.
    #[test]
    fn an_unknown_phase_falls_back_to_what_the_helper_sent() {
        assert_eq!(phase_label(None, "Polishing"), "Polishing");
        assert_eq!(phase_label(None, ""), "Starting…");
    }

    #[test]
    fn a_deep_path_is_shown_as_its_file_name() {
        assert_eq!(
            file_name_of(std::path::Path::new("/a/very/deep/path/Win10.iso")),
            "Win10.iso"
        );
    }
}
