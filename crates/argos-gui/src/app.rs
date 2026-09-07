//! The window.
//!
//! Everything here is presentation and thread plumbing. What may be written,
//! and what happens when it is, lives in `argos-session` -- the same code the
//! CLI runs, so there is one implementation of "may I write to this disk?"
//! rather than one per interface.

use crate::devices::{offerable, reconcile, Selection, SelectionOutcome};
use crate::state::{confirmation_matches, AppState, WorkerMsg};
use argos_core::device::Device;
use argos_core::error::ArgosError;
use argos_platform::PlatformOps;
use argos_privileged::protocol::WindowsLayout;
use argos_session::{self as session, human_size, ImageKind, WritePreview};
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
        // Drained in full, not one per frame: a write emits thousands of
        // progress events and falling behind would show stale numbers.
        self.take_messages();
        self.maybe_enumerate(ctx, false);
        self.accept_dropped_iso(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Argos");
            ui.label("Create a bootable installer USB drive");
            ui.separator();

            match &self.state {
                AppState::Idle | AppState::Preparing => self.draw_chooser(ui, ctx),
                AppState::AwaitingConfirmation { .. } => {
                    self.draw_chooser(ui, ctx);
                }
                AppState::Running(_) => self.draw_progress(ui),
                AppState::Done { .. } | AppState::Failed { .. } | AppState::Cancelled => {
                    self.draw_result(ui)
                }
            }
        });

        self.draw_confirmation(ctx);

        // While the device poll is the only thing that changes the screen,
        // ask for a repaint when it is due rather than spinning every frame.
        if matches!(
            self.state,
            AppState::Idle | AppState::AwaitingConfirmation { .. }
        ) {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
    }
}

impl ArgosApp {
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

    fn draw_chooser(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.draw_image_row(ui, ctx);
        ui.add_space(8.0);
        self.draw_device_row(ui, ctx);
        ui.add_space(8.0);
        self.draw_layout_row(ui);
        ui.add_space(8.0);
        self.draw_warnings(ui);
        ui.add_space(8.0);

        let ready = self.iso_kind.is_some()
            && self.selection.is_some()
            && !self.classifying
            && matches!(self.state, AppState::Idle);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ready, egui::Button::new("Write…"))
                .on_disabled_hover_text("Choose an image Argos recognizes and a target drive")
                .clicked()
            {
                self.begin_preparation(ctx);
            }
            if matches!(self.state, AppState::Preparing) {
                ui.spinner();
                ui.label("Checking the image and the drive…");
            }
        });
    }

    fn draw_image_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label("Image:");
            let shown = self
                .iso
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(drop an ISO here, or choose one)".into());
            ui.monospace(shown);
            // Called on the UI thread deliberately: on macOS NSOpenPanel must
            // be driven from the main thread. One frozen frame while a modal
            // file chooser is open is what every desktop app does.
            if ui.button("Choose…").clicked() {
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
                ui.label("Reading the image…");
            });
        } else if let Some(err) = &self.iso_error {
            ui.colored_label(ui.visuals().error_fg_color, err);
        } else if let Some(kind) = self.iso_kind {
            ui.label(match kind {
                ImageKind::LinuxDd => "Detected: Linux ISO (written byte for byte)",
                ImageKind::WindowsInstaller => "Detected: Windows installer",
            });
        }
    }

    fn draw_device_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let offered: Vec<Device> = offerable(&self.devices, self.show_all_devices)
            .into_iter()
            .cloned()
            .collect();

        ui.horizontal(|ui| {
            ui.label("Target:");
            let label = self
                .selected_device()
                .map(describe_device)
                .unwrap_or_else(|| "(no drive selected)".into());
            egui::ComboBox::from_id_salt("target-device")
                .selected_text(label)
                .width(360.0)
                .show_ui(ui, |ui| {
                    for device in &offered {
                        let selected = self
                            .selection
                            .as_ref()
                            .is_some_and(|s| s.platform_id == device.platform_id);
                        if ui
                            .selectable_label(selected, describe_device(device))
                            .clicked()
                        {
                            self.selection = Some(Selection::of(device));
                            self.selection_warning = None;
                        }
                    }
                    if offered.is_empty() {
                        ui.label("No removable drives found");
                    }
                });
            if ui.button("⟳").on_hover_text("Refresh the list").clicked() {
                self.maybe_enumerate(ctx, true);
            }
        });

        // Mirrors --i-know-what-im-doing. System disks stay absent in both
        // modes: `offerable` never returns one.
        ui.checkbox(
            &mut self.show_all_devices,
            "Show all drives, including ones the system does not call removable",
        );
    }

    /// The layout control. Always visible, never behind an advanced menu:
    /// which firmware the result boots is the most consequential choice in
    /// this window, and old BIOS machines are the use case the project exists
    /// for.
    fn draw_layout_row(&mut self, ui: &mut egui::Ui) {
        let applies = self.iso_kind == Some(ImageKind::WindowsInstaller);
        ui.add_enabled_ui(applies, |ui| {
            ui.checkbox(&mut self.bios_layout, "Old machine — legacy BIOS (MBR)");
        });
        let explanation = if !applies {
            match self.iso_kind {
                // Said, not merely greyed: a disabled control with no reason
                // reads as a bug.
                Some(ImageKind::LinuxDd) => {
                    "A Linux ISO carries its own partition table, so there is nothing to choose."
                }
                _ => "Choose a Windows installer image to enable this.",
            }
        } else if self.bios_layout {
            "MBR with Argos's own boot records: boots legacy BIOS, and UEFI firmware \
             that accepts MBR-partitioned removable media. Windows 10 only."
        } else {
            "GPT: boots UEFI firmware only."
        };
        ui.label(egui::RichText::new(explanation).small());
    }

    fn draw_warnings(&mut self, ui: &mut egui::Ui) {
        if let Some(warning) = &self.selection_warning {
            ui.colored_label(ui.visuals().warn_fg_color, warning);
        }
        if let Some(err) = &self.device_error {
            ui.colored_label(ui.visuals().error_fg_color, format!("Drives: {err}"));
        }
        if let AppState::AwaitingConfirmation { prepared, .. } = &self.state {
            for note in prepared.preview.split_notes() {
                ui.label(format!(
                    "{} is over FAT32's 4GiB file limit and will be split into {} parts",
                    note.source_path,
                    note.part_paths.len()
                ));
            }
        }
    }

    fn draw_progress(&mut self, ui: &mut egui::Ui) {
        let AppState::Running(run) = &self.state else {
            return;
        };
        ui.label(if run.phase_label.is_empty() {
            "Starting…".to_string()
        } else {
            run.phase_label.clone()
        });
        let bar = match run.fraction() {
            Some(fraction) => egui::ProgressBar::new(fraction).show_percentage(),
            None => egui::ProgressBar::new(0.0).animate(true),
        };
        ui.add(bar);
        if run.bytes_total > 0 {
            ui.label(format!(
                "{} / {}",
                human_size(run.bytes_done),
                human_size(run.bytes_total)
            ));
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
        // cancel is not delayed, it is discarded. Saying so beats a button
        // that looks live and does nothing.
        if requested {
            ui.label("Stopping. The drive will be left unusable and must be rewritten.");
        } else if !cancellable {
            ui.label(egui::RichText::new("Past the point where this can be interrupted.").small());
        }
    }

    fn draw_result(&mut self, ui: &mut egui::Ui) {
        match &self.state {
            AppState::Done { outcome, device_id } => {
                ui.label(format!("Done. {}", describe_outcome(outcome)));
                ui.label(format!("Drive: {device_id}"));
            }
            AppState::Cancelled => {
                ui.label("Cancelled. The drive was left unusable and must be rewritten.");
            }
            AppState::Failed { error } => {
                ui.colored_label(ui.visuals().error_fg_color, error.to_string());
            }
            _ => {}
        }
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

        egui::Window::new("Confirm")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("About to overwrite:");
                ui.monospace(format!("{} ({})", device.platform_id, device.display_name));
                ui.label(format!("Size: {}", human_size(device.size_bytes)));
                ui.label(format!(
                    "Serial: {}",
                    device.serial.as_deref().unwrap_or("unknown")
                ));
                ui.label(format!("Image: {}", iso.display()));
                ui.label(preview_line);
                for note in &notes {
                    ui.label(note);
                }
                ui.separator();
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!(
                        "This will PERMANENTLY ERASE all data on {}.",
                        device.platform_id
                    ),
                );
                ui.label(format!(
                    "Type the device path ({}) to confirm:",
                    device.platform_id
                ));
                ui.text_edit_singleline(&mut typed_now);
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
            // Wiring this to `session::spawn` is G5 (#92). Reported through
            // the real error type rather than a placeholder string, so the
            // result panel is exercised as it will actually be used.
            self.state = AppState::Failed {
                error: ArgosError::NotImplemented("writing from the GUI"),
            };
        }
    }
}

fn describe_device(device: &Device) -> String {
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
            "One FAT32 partition of {} at offset {}",
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
        Outcome::WindowsVerify { files_verified } => format!("{files_verified} files verified"),
    }
}
