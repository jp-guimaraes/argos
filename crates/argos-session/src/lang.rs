//! `Lang` and `Strings`: the whole translated surface of the GUI, in one
//! place.
//!
//! Hand-written and checked at compile time -- not a file-based scheme.
//! `Strings` is a plain struct: a fixed label missing from either catalogue
//! is a compile error, and a parameterized message is a function pointer
//! rather than a format template, so its placeholder *arity* is
//! type-checked too. That is a stronger guarantee than gettext or any
//! `.po`-at-runtime scheme gives for two languages and roughly eighty
//! strings -- and it is exactly why this project didn't reach for gettext
//! despite `docs-site` already using PO: there is no well-maintained
//! pure-Rust `.mo` runtime, and `gettext-rs` links C `libintl`, a system
//! dependency this project's "easy to install on macOS" constraint rules
//! out. The escape hatch, if translator ergonomics ever matter, is a
//! `build.rs` generating this file from a `.po` at compile time -- so this
//! is not a dead end, just the simplest thing that is honest about what two
//! languages actually need.
//!
//! Lives in `argos-session`, not `argos-gui`, so error localization sits
//! next to `ArgosError` -- the type doing the matching in
//! [`localize_error`] -- and so a future front end gets it for free.
//!
//! The CLI stays English on purpose: its help text, man page and five
//! completion scripts all come from the same clap definitions, and
//! `packaging/build-deb.sh` runs `argos man` on the build machine -- a
//! locale-dependent `argos man` would ship whatever language the CI runner
//! happened to have. `argos-core`'s own `#[error(...)]` strings stay
//! English for the same reason: that crate's whole purpose is carrying no
//! dependency, i18n included, and [`localize_error`] below matches on the
//! typed variant rather than needing the crate to translate itself.

use argos_core::error::ArgosError;

/// Which language the GUI is showing. `Auto` is a menu choice, not a value
/// `Strings` are ever fetched for -- resolving it to `En`/`PtBr` is
/// [`detect`]'s job, so the rest of the app only ever handles the two real
/// languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    PtBr,
}

impl Lang {
    /// The identifier this config file and `ARGOS_LANG` use -- stable, and
    /// deliberately not the same thing as a display name (`strings_for`'s
    /// `lang_name`), which is allowed to change wording without breaking a
    /// saved config.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::PtBr => "pt_BR",
        }
    }

    /// The inverse of [`Lang::code`]. Anything else -- an empty config
    /// value, a typo, a future language this build doesn't know -- is
    /// `None`, which every caller here treats as "fall through to the next
    /// thing in the precedence chain" rather than an error. A config file
    /// naming a language a downgraded Argos doesn't understand should not
    /// refuse to start.
    pub fn from_code(code: &str) -> Option<Lang> {
        match code {
            "en" => Some(Lang::En),
            "pt_BR" => Some(Lang::PtBr),
            _ => None,
        }
    }
}

pub fn strings_for(lang: Lang) -> &'static Strings {
    match lang {
        Lang::En => &EN,
        Lang::PtBr => &PT_BR,
    }
}

/// One field per fixed label; a function pointer per message that takes a
/// parameter, so a call site passing the wrong number of arguments -- or the
/// wrong type -- is a compile error in *both* catalogues, not a blank
/// placeholder discovered by a translator or a user.
pub struct Strings {
    /// This language's own name, for the language menu itself -- "English"
    /// and "Português (Brasil)" are shown in their own language, not
    /// translated into whichever one is currently active, the way any
    /// language picker menu does it.
    pub lang_name: &'static str,
    pub lang_menu_auto: &'static str,
    /// Label drawn immediately before the language combo box. Without it,
    /// the box showed only its current value ("Automatic", "English"...)
    /// with no indication of what it was choosing -- a human tester read it
    /// as some kind of write/recording setting, not a language picker.
    pub lang_menu_label: &'static str,

    // ---- Image group ----------------------------------------------------
    pub image_group_title: &'static str,
    pub choose_button: &'static str,
    pub drop_iso_hint: &'static str,
    pub file_picker_title: &'static str,
    pub file_picker_filter_name: &'static str,
    pub reading_image: &'static str,
    pub no_image_selected: &'static str,
    pub linux_iso_detected: &'static str,
    pub windows_iso_detected: &'static str,
    pub not_recognized_image: &'static str,

    // ---- Target group -----------------------------------------------------
    pub target_group_title: &'static str,
    pub refresh_tooltip: &'static str,
    pub no_device_selected: &'static str,
    pub no_removable_devices: &'static str,
    pub show_all_devices_checkbox: &'static str,
    pub could_not_list_devices: &'static str,
    pub serial_unknown: &'static str,

    // ---- Layout group -----------------------------------------------------
    pub bios_layout_checkbox: &'static str,
    pub layout_na_linux: &'static str,
    pub layout_na_no_image: &'static str,
    pub layout_bios_explanation: &'static str,
    pub layout_gpt_explanation: &'static str,

    // ---- Write / Verify ----------------------------------------------------
    pub write_button: &'static str,
    pub verify_button: &'static str,
    pub write_disabled_hint: &'static str,

    // ---- Progress -----------------------------------------------------------
    pub working_out_total: &'static str,
    pub do_not_unplug: &'static str,
    pub cancel_button: &'static str,
    pub verification_not_interruptible: &'static str,
    pub interrupting_still_possible: &'static str,
    pub interrupting_in_progress: &'static str,
    pub past_interrupt_point: &'static str,

    // ---- Phases, in the order argos_core::progress::Phase declares them ---
    pub phase_unmounting: &'static str,
    pub phase_checksumming: &'static str,
    pub phase_partitioning: &'static str,
    pub phase_formatting: &'static str,
    pub phase_copying_files: &'static str,
    pub phase_writing: &'static str,
    pub phase_flushing: &'static str,
    pub phase_verifying: &'static str,
    pub phase_starting: &'static str,

    // ---- Result screen ------------------------------------------------------
    pub cancelled_message: &'static str,
    pub details_label: &'static str,
    pub start_over_button: &'static str,

    // ---- Confirmation modal -------------------------------------------------
    pub confirm_title: &'static str,
    pub about_to_overwrite: &'static str,
    pub image_label: &'static str,
    pub retype_hint: &'static str,
    pub write_confirm_button: &'static str,

    // ---- Parameterized messages: arity checked by the function's type ------
    pub selection_gone: fn(&str) -> String,
    pub selection_replaced: fn(&str) -> String,
    pub ejected: fn(&str) -> String,
    pub size_label: fn(&str) -> String,
    pub serial_label: fn(&str) -> String,
    pub erase_warning: fn(&str) -> String,
    pub type_to_confirm: fn(&str) -> String,
    pub split_note: fn(&str, usize) -> String,
    pub split_note_confirmation: fn(&str, usize, &str) -> String,
    pub done_prefix: fn(&str) -> String,
    pub image_size_label: fn(&str) -> String,
    pub one_fat32_partition: fn(&str, &str) -> String,
    pub sha256_label: fn(&str) -> String,
    pub verified_sha256: fn(&str) -> String,
    pub files_copied: fn(u64, &str) -> String,
    pub files_checked: fn(u64) -> String,
    pub device_description: fn(&str, &str) -> String,
    pub device_description_long: fn(&str, &str, &str) -> String,

    // ---- Error localization (G6.5) ------------------------------------------
    /// Shown ahead of the raw English text for anything below with no
    /// per-category translation: a `Helper` error whose `exit_code` this
    /// build doesn't recognise (version skew, in either direction), or one
    /// of the handful of `ArgosError` variants rare enough that a category
    /// label -- not the exact parameters, which don't survive the crossing
    /// from `argos-helper`'s own error message -- isn't worth a dedicated
    /// field here. Never a bare number, per the milestone's own rule.
    pub error_generic_prefix: &'static str,
    pub error_device_not_found: fn(&str) -> String,
    pub error_device_not_removable: fn(&str) -> String,
    pub error_device_is_system_disk: fn(&str) -> String,
    pub error_insufficient_permissions: fn(&str) -> String,
    pub error_source_target_collision: fn(&str) -> String,
    pub error_unsupported_iso: fn(&str) -> String,
    pub error_not_windows_installer_iso: fn(&str) -> String,
    pub error_checksum_mismatch: fn(&str, &str) -> String,
    pub error_windows_partition_layout_mismatch: fn(&str) -> String,
    pub error_cancelled: &'static str,
    pub error_not_confirmed: &'static str,
    pub error_elevation_declined: &'static str,
    /// One category label per `ArgosError::Helper` exit code this build
    /// recognises (10-27, stable since G2/#89's exit-code fix); used only
    /// when the specific parameters the CLI-side variants above carry
    /// didn't survive the privilege boundary. Order matches
    /// `ArgosError::exit_code`'s own match arms.
    pub helper_category_device_not_found: &'static str,
    pub helper_category_device_not_removable: &'static str,
    pub helper_category_device_is_system_disk: &'static str,
    pub helper_category_insufficient_permissions: &'static str,
    pub helper_category_device_too_small: &'static str,
    pub helper_category_source_target_collision: &'static str,
    pub helper_category_unsupported_iso: &'static str,
    pub helper_category_checksum_mismatch: &'static str,
    pub helper_category_cancelled: &'static str,
    pub helper_category_io: &'static str,
    pub helper_category_not_implemented: &'static str,
    pub helper_category_not_windows_installer_iso: &'static str,
    pub helper_category_windows_partition_layout_mismatch: &'static str,
    pub helper_category_windows_file_mismatch: &'static str,
    pub helper_category_windows_file_too_large_for_fat32: &'static str,
    pub helper_category_not_confirmed: &'static str,
}

pub static EN: Strings = Strings {
    lang_name: "English",
    lang_menu_auto: "Automatic",
    lang_menu_label: "Language",

    image_group_title: "Image",
    choose_button: "Choose…",
    drop_iso_hint: "Drop an ISO here",
    file_picker_title: "Choose a disk image",
    file_picker_filter_name: "Disk images",
    reading_image: "Reading the image…",
    no_image_selected: "No image selected.",
    linux_iso_detected: "✔ Linux ISO (written byte for byte)",
    windows_iso_detected: "✔ Windows installer",
    not_recognized_image: "Not an image Argos recognizes",

    target_group_title: "Target",
    refresh_tooltip: "Refresh the list",
    no_device_selected: "No device selected",
    no_removable_devices: "No removable devices found",
    show_all_devices_checkbox: "Show every disk, including those the system does not consider removable",
    could_not_list_devices: "Could not list devices.",
    serial_unknown: "unknown",

    bios_layout_checkbox: "Old machine — legacy BIOS (MBR)",
    layout_na_linux: "A Linux ISO carries its own partition table, so there is nothing to choose.",
    layout_na_no_image: "Choose a Windows installer image to enable this.",
    layout_bios_explanation: "MBR with Argos's own boot records: boots on legacy BIOS, and also on UEFI \
        firmware that accepts MBR-partitioned removable media. Windows 10 only.",
    layout_gpt_explanation: "GPT: boots only on UEFI firmware.",

    write_button: "Write…",
    verify_button: "Verify…",
    write_disabled_hint: "Choose an image Argos recognises and a target device",

    working_out_total: "Working out the total…",
    do_not_unplug: "Do not unplug the device.",
    cancel_button: "Cancel",
    verification_not_interruptible: "Verification cannot be interrupted.",
    interrupting_still_possible: "Interrupting is still possible.",
    interrupting_in_progress: "Interrupting. The device will be unusable and will need to be written again.",
    past_interrupt_point: "Past the point where interrupting is possible.",

    phase_unmounting: "Unmounting",
    phase_checksumming: "Checksumming",
    phase_partitioning: "Partitioning",
    phase_formatting: "Formatting",
    phase_copying_files: "Copying files",
    phase_writing: "Writing",
    phase_flushing: "Flushing",
    phase_verifying: "Verifying",
    phase_starting: "Starting…",

    cancelled_message: "Cancelled. The device is unusable and needs to be written again.",
    details_label: "Details",
    start_over_button: "Start over",

    confirm_title: "Confirm",
    about_to_overwrite: "About to overwrite:",
    image_label: "Image:",
    retype_hint: "Retype the device path exactly",
    write_confirm_button: "Write",

    selection_gone: |path| format!("{path} is no longer present"),
    selection_replaced: |path| format!("A different drive is now at {path}; selection cleared"),
    ejected: |device| format!("Ejected {device}. Safe to unplug."),
    size_label: |size| format!("Size: {size}"),
    serial_label: |serial| format!("Serial number: {serial}"),
    erase_warning: |path| format!("THIS WILL PERMANENTLY ERASE all data on {path}."),
    type_to_confirm: |path| format!("Type the device path ({path}) to confirm:"),
    split_note: |source, parts| {
        format!("· {source} exceeds the FAT32 4 GiB limit and will be split into {parts} parts")
    },
    split_note_confirmation: |source, parts, names| {
        format!("{source} will be split into {parts} parts ({names})")
    },
    done_prefix: |summary| format!("✔ Done. {summary}"),
    image_size_label: |size| format!("Image size: {size}"),
    one_fat32_partition: |size, offset| format!("One {size} FAT32 partition at offset {offset}"),
    sha256_label: |hash| format!("SHA-256: {hash}"),
    verified_sha256: |hash| format!("Verified. SHA-256: {hash}"),
    files_copied: |count, size| format!("{count} files copied ({size})"),
    files_checked: |count| format!("{count} files checked"),
    device_description: |id, size| format!("{id} — {size}"),
    device_description_long: |id, name, size| format!("{id} — {name} ({size})"),

    error_generic_prefix: "the privileged process reported an error",
    error_device_not_found: |id| format!("no device matches '{id}'"),
    error_device_not_removable: |id| {
        format!("device '{id}' is not a removable disk; refusing to write without --i-know-what-im-doing")
    },
    error_device_is_system_disk: |id| {
        format!("device '{id}' looks like a system disk; refusing to write")
    },
    error_insufficient_permissions: |id| {
        format!("insufficient permissions to access '{id}'; try running with elevated privileges")
    },
    error_source_target_collision: |path| {
        format!("the image file '{path}' is stored on the very device it would be written to")
    },
    error_unsupported_iso: |path| format!("'{path}' is not a Linux ISO9660 image Argos recognizes"),
    error_not_windows_installer_iso: |path| {
        format!("'{path}' is not a Windows installer ISO Argos recognizes")
    },
    error_checksum_mismatch: |expected, actual| {
        format!("checksum mismatch after writing: expected {expected}, got {actual}")
    },
    error_windows_partition_layout_mismatch: |detail| {
        format!("partition table does not match the expected Windows write layout: {detail}")
    },
    error_cancelled: "operation cancelled by user; the device is left in an inconsistent state and must be rewritten before use",
    error_not_confirmed: "confirmation did not match; nothing was written and the device is untouched",
    error_elevation_declined: "authorization was declined; nothing was written and the device is untouched",

    helper_category_device_not_found: "no device matches that path",
    helper_category_device_not_removable: "the device is not removable",
    helper_category_device_is_system_disk: "the device looks like a system disk",
    helper_category_insufficient_permissions: "insufficient permissions to access the device",
    helper_category_device_too_small: "the device is smaller than the image",
    helper_category_source_target_collision: "the image is stored on the device it would be written to",
    helper_category_unsupported_iso: "not a Linux ISO Argos recognizes",
    helper_category_checksum_mismatch: "checksum mismatch after writing",
    helper_category_cancelled: "operation cancelled",
    helper_category_io: "an I/O error occurred",
    helper_category_not_implemented: "not implemented",
    helper_category_not_windows_installer_iso: "not a Windows installer ISO Argos recognizes",
    helper_category_windows_partition_layout_mismatch: "the partition table does not match the expected layout",
    helper_category_windows_file_mismatch: "a file does not match after writing",
    helper_category_windows_file_too_large_for_fat32: "a file is too large to fit on FAT32",
    helper_category_not_confirmed: "confirmation was not given",
};

pub static PT_BR: Strings = Strings {
    lang_name: "Português (Brasil)",
    lang_menu_auto: "Automático",
    lang_menu_label: "Idioma",

    image_group_title: "Imagem",
    choose_button: "Escolher…",
    drop_iso_hint: "Arraste uma ISO aqui",
    file_picker_title: "Escolha uma imagem de disco",
    file_picker_filter_name: "Imagens de disco",
    reading_image: "Lendo a imagem…",
    no_image_selected: "Nenhuma imagem selecionada.",
    linux_iso_detected: "✔ ISO Linux (gravada byte a byte)",
    windows_iso_detected: "✔ Instalador do Windows",
    not_recognized_image: "Não é uma imagem que o Argos reconhece",

    target_group_title: "Destino",
    refresh_tooltip: "Atualizar a lista",
    no_device_selected: "Nenhum dispositivo selecionado",
    no_removable_devices: "Nenhum dispositivo removível encontrado",
    show_all_devices_checkbox: "Mostrar todos os discos, incluindo os que o sistema não considera removíveis",
    could_not_list_devices: "Não foi possível listar os dispositivos.",
    serial_unknown: "desconhecido",

    bios_layout_checkbox: "Máquina antiga — BIOS legado (MBR)",
    layout_na_linux: "Uma ISO Linux já carrega sua própria tabela de partição, então não há nada para escolher aqui.",
    layout_na_no_image: "Escolha uma imagem de instalador do Windows para habilitar isto.",
    layout_bios_explanation: "MBR com os registros de boot do próprio Argos: inicializa em BIOS legado, e \
        também em firmware UEFI que aceita mídia removível particionada em MBR. Apenas Windows 10.",
    layout_gpt_explanation: "GPT: inicializa apenas em firmware UEFI.",

    write_button: "Gravar…",
    verify_button: "Verificar…",
    write_disabled_hint: "Escolha uma imagem que o Argos reconheça e um dispositivo de destino",

    working_out_total: "Calculando o total…",
    do_not_unplug: "Não desconecte o dispositivo.",
    cancel_button: "Cancelar",
    verification_not_interruptible: "A verificação não pode ser interrompida.",
    interrupting_still_possible: "Ainda é possível interromper.",
    interrupting_in_progress: "Interrompendo. O dispositivo ficará inutilizável e precisará ser gravado novamente.",
    past_interrupt_point: "Passou do ponto em que é possível interromper.",

    phase_unmounting: "Desmontando",
    phase_checksumming: "Calculando checksum",
    phase_partitioning: "Particionando",
    phase_formatting: "Formatando",
    phase_copying_files: "Copiando arquivos",
    phase_writing: "Gravando",
    phase_flushing: "Descarregando",
    phase_verifying: "Verificando",
    phase_starting: "Iniciando…",

    cancelled_message: "Cancelado. O dispositivo ficou inutilizável e precisa ser gravado novamente.",
    details_label: "Detalhes",
    start_over_button: "Recomeçar",

    confirm_title: "Confirmar",
    about_to_overwrite: "Prestes a sobrescrever:",
    image_label: "Imagem:",
    retype_hint: "Digite o caminho do dispositivo exatamente",
    write_confirm_button: "Gravar",

    selection_gone: |path| format!("{path} não está mais presente"),
    selection_replaced: |path| format!("Um dispositivo diferente agora está em {path}; seleção limpa"),
    ejected: |device| format!("{device} ejetado. Seguro para desconectar."),
    size_label: |size| format!("Tamanho: {size}"),
    serial_label: |serial| format!("Número de série: {serial}"),
    erase_warning: |path| format!("ISTO VAI APAGAR PERMANENTEMENTE todos os dados em {path}."),
    type_to_confirm: |path| format!("Digite o caminho do dispositivo ({path}) para confirmar:"),
    split_note: |source, parts| {
        format!("· {source} excede o limite de 4 GiB do FAT32 e será dividido em {parts} partes")
    },
    split_note_confirmation: |source, parts, names| {
        format!("{source} será dividido em {parts} partes ({names})")
    },
    done_prefix: |summary| format!("✔ Concluído. {summary}"),
    image_size_label: |size| format!("Tamanho da imagem: {size}"),
    one_fat32_partition: |size, offset| format!("Uma partição FAT32 de {size} no deslocamento {offset}"),
    sha256_label: |hash| format!("SHA-256: {hash}"),
    verified_sha256: |hash| format!("Verificado. SHA-256: {hash}"),
    files_copied: |count, size| format!("{count} arquivos copiados ({size})"),
    files_checked: |count| format!("{count} arquivos verificados"),
    device_description: |id, size| format!("{id} — {size}"),
    device_description_long: |id, name, size| format!("{id} — {name} ({size})"),

    error_generic_prefix: "o processo privilegiado relatou um erro",
    error_device_not_found: |id| format!("nenhum dispositivo corresponde a '{id}'"),
    error_device_not_removable: |id| {
        format!("o dispositivo '{id}' não é um disco removível; recusando gravar sem --i-know-what-im-doing")
    },
    error_device_is_system_disk: |id| {
        format!("o dispositivo '{id}' parece ser um disco de sistema; recusando gravar")
    },
    error_insufficient_permissions: |id| {
        format!("permissões insuficientes para acessar '{id}'; tente executar com privilégios elevados")
    },
    error_source_target_collision: |path| {
        format!("o arquivo de imagem '{path}' está armazenado no próprio dispositivo em que seria gravado")
    },
    error_unsupported_iso: |path| format!("'{path}' não é uma imagem ISO9660 Linux que o Argos reconhece"),
    error_not_windows_installer_iso: |path| {
        format!("'{path}' não é uma ISO de instalador do Windows que o Argos reconhece")
    },
    error_checksum_mismatch: |expected, actual| {
        format!("checksum não confere após a gravação: esperado {expected}, obtido {actual}")
    },
    error_windows_partition_layout_mismatch: |detail| {
        format!("a tabela de partição não corresponde ao layout de gravação do Windows esperado: {detail}")
    },
    error_cancelled: "operação cancelada pelo usuário; o dispositivo ficou em estado inconsistente e precisa ser regravado antes de usar",
    error_not_confirmed: "a confirmação não correspondeu; nada foi gravado e o dispositivo está intacto",
    error_elevation_declined: "a autorização foi recusada; nada foi gravado e o dispositivo está intacto",

    helper_category_device_not_found: "nenhum dispositivo corresponde a esse caminho",
    helper_category_device_not_removable: "o dispositivo não é removível",
    helper_category_device_is_system_disk: "o dispositivo parece ser um disco de sistema",
    helper_category_insufficient_permissions: "permissões insuficientes para acessar o dispositivo",
    helper_category_device_too_small: "o dispositivo é menor que a imagem",
    helper_category_source_target_collision: "a imagem está armazenada no dispositivo em que seria gravada",
    helper_category_unsupported_iso: "não é uma ISO Linux que o Argos reconhece",
    helper_category_checksum_mismatch: "checksum não confere após a gravação",
    helper_category_cancelled: "operação cancelada",
    helper_category_io: "ocorreu um erro de entrada/saída",
    helper_category_not_implemented: "não implementado",
    helper_category_not_windows_installer_iso: "não é uma ISO de instalador do Windows que o Argos reconhece",
    helper_category_windows_partition_layout_mismatch: "a tabela de partição não corresponde ao layout esperado",
    helper_category_windows_file_mismatch: "um arquivo não confere após a gravação",
    helper_category_windows_file_too_large_for_fat32: "um arquivo é grande demais para caber em FAT32",
    helper_category_not_confirmed: "a confirmação não foi dada",
};

/// Translates an [`ArgosError`] for display, in the spirit G6.5 asks for:
/// the CLI-side variants -- the large majority, thrown before anything ever
/// crosses the privilege boundary -- get a real translation with their own
/// parameters. `Helper` errors arrive already flattened into a string by
/// `argos-helper`, so they're matched on the one thing that does survive the
/// crossing, `exit_code` (stable since #89), and get a *category* label
/// rather than the exact wording -- the specific device path or file name
/// the helper's own message named is not reconstructable from an exit code
/// alone. An `exit_code` this build doesn't recognise (built against an
/// older or newer helper) falls through to the generic prefix, same as
/// every other case with no per-category translation, rather than showing a
/// bare number.
pub fn localize_error(err: &ArgosError, lang: Lang) -> String {
    let s = strings_for(lang);
    match err {
        ArgosError::DeviceNotFound(id) => (s.error_device_not_found)(id),
        ArgosError::DeviceNotRemovable(id) => (s.error_device_not_removable)(id),
        ArgosError::DeviceIsSystemDisk(id) => (s.error_device_is_system_disk)(id),
        ArgosError::InsufficientPermissions(id) => (s.error_insufficient_permissions)(id),
        ArgosError::SourceTargetCollision(path) => {
            (s.error_source_target_collision)(&path.display().to_string())
        }
        ArgosError::UnsupportedIso(path) => (s.error_unsupported_iso)(&path.display().to_string()),
        ArgosError::NotWindowsInstallerIso(path) => {
            (s.error_not_windows_installer_iso)(&path.display().to_string())
        }
        ArgosError::ChecksumMismatch { expected, actual } => {
            (s.error_checksum_mismatch)(expected, actual)
        }
        ArgosError::WindowsPartitionLayoutMismatch(detail) => {
            (s.error_windows_partition_layout_mismatch)(detail)
        }
        ArgosError::Cancelled => s.error_cancelled.to_string(),
        ArgosError::NotConfirmed => s.error_not_confirmed.to_string(),
        ArgosError::ElevationDeclined => s.error_elevation_declined.to_string(),
        // A known exit code gets *only* its category: `helper_category`'s
        // labels are already complete, translated sentences (matching the
        // CLI-side variants above, none of which append their own raw
        // English either), and the whole point of translating this screen
        // is that its main line reads in one language. `message` is not
        // silently dropped -- it is what `draw_result`'s "Details" already
        // shows underneath, from `error.to_string()`, same as every other
        // variant here.
        //
        // Found on real hardware, not by inspection: yanking a USB stick
        // mid-write produced "operação cancelada: operation cancelled by
        // user; the device is left in an inconsistent state..." on the main
        // line -- a Portuguese phrase glued to an entire English sentence,
        // both visible at once. An unrecognised exit code still needs the
        // raw message concatenated (there is no category to stand alone
        // with), which is the one place this string legitimately mixes
        // languages.
        ArgosError::Helper { message, exit_code } => match helper_category(*exit_code, s) {
            Some(category) => category.to_string(),
            None => format!("{}: {message}", s.error_generic_prefix),
        },
        // DeviceTooSmall, WindowsFileMismatch, WindowsFileTooLargeForFat32,
        // Io and NotImplemented: rare enough, and carry enough independent
        // parameters, that a dedicated field would mostly restate the
        // English -- the raw message behind the translated prefix is what a
        // bug report needs anyway, and stays intact either way.
        other => format!("{}: {other}", s.error_generic_prefix),
    }
}

/// Every fixed label plus a representative call of every parameterized
/// message, for one language. Exists to support two different tests in two
/// different crates: this module's own placeholder-arity check below, and
/// `argos-gui`'s font-coverage test, which needs to know every character
/// the UI can actually draw in *either* language -- a hand-picked glyph
/// list (the old approach, still used for the handful of pictographic
/// symbols like `✔`/`⚠` that don't come from this catalogue) would silently
/// miss an accented Portuguese letter the day a translation added one.
pub fn sample_all_messages(lang: Lang) -> Vec<String> {
    let s = strings_for(lang);
    vec![
        s.lang_name.to_string(),
        s.lang_menu_auto.to_string(),
        s.lang_menu_label.to_string(),
        s.image_group_title.to_string(),
        s.choose_button.to_string(),
        s.drop_iso_hint.to_string(),
        s.file_picker_title.to_string(),
        s.file_picker_filter_name.to_string(),
        s.reading_image.to_string(),
        s.no_image_selected.to_string(),
        s.linux_iso_detected.to_string(),
        s.windows_iso_detected.to_string(),
        s.not_recognized_image.to_string(),
        s.target_group_title.to_string(),
        s.refresh_tooltip.to_string(),
        s.no_device_selected.to_string(),
        s.no_removable_devices.to_string(),
        s.show_all_devices_checkbox.to_string(),
        s.could_not_list_devices.to_string(),
        s.serial_unknown.to_string(),
        s.bios_layout_checkbox.to_string(),
        s.layout_na_linux.to_string(),
        s.layout_na_no_image.to_string(),
        s.layout_bios_explanation.to_string(),
        s.layout_gpt_explanation.to_string(),
        s.write_button.to_string(),
        s.verify_button.to_string(),
        s.write_disabled_hint.to_string(),
        s.working_out_total.to_string(),
        s.do_not_unplug.to_string(),
        s.cancel_button.to_string(),
        s.verification_not_interruptible.to_string(),
        s.interrupting_still_possible.to_string(),
        s.interrupting_in_progress.to_string(),
        s.past_interrupt_point.to_string(),
        s.phase_unmounting.to_string(),
        s.phase_checksumming.to_string(),
        s.phase_partitioning.to_string(),
        s.phase_formatting.to_string(),
        s.phase_copying_files.to_string(),
        s.phase_writing.to_string(),
        s.phase_flushing.to_string(),
        s.phase_verifying.to_string(),
        s.phase_starting.to_string(),
        s.cancelled_message.to_string(),
        s.details_label.to_string(),
        s.start_over_button.to_string(),
        s.confirm_title.to_string(),
        s.about_to_overwrite.to_string(),
        s.image_label.to_string(),
        s.retype_hint.to_string(),
        s.write_confirm_button.to_string(),
        s.error_generic_prefix.to_string(),
        s.error_cancelled.to_string(),
        s.error_not_confirmed.to_string(),
        s.error_elevation_declined.to_string(),
        s.helper_category_device_not_found.to_string(),
        s.helper_category_device_not_removable.to_string(),
        s.helper_category_device_is_system_disk.to_string(),
        s.helper_category_insufficient_permissions.to_string(),
        s.helper_category_device_too_small.to_string(),
        s.helper_category_source_target_collision.to_string(),
        s.helper_category_unsupported_iso.to_string(),
        s.helper_category_checksum_mismatch.to_string(),
        s.helper_category_cancelled.to_string(),
        s.helper_category_io.to_string(),
        s.helper_category_not_implemented.to_string(),
        s.helper_category_not_windows_installer_iso.to_string(),
        s.helper_category_windows_partition_layout_mismatch
            .to_string(),
        s.helper_category_windows_file_mismatch.to_string(),
        s.helper_category_windows_file_too_large_for_fat32
            .to_string(),
        s.helper_category_not_confirmed.to_string(),
        (s.selection_gone)("/dev/sdb"),
        (s.selection_replaced)("/dev/sdb"),
        (s.ejected)("/dev/sdb"),
        (s.size_label)("1.0GiB"),
        (s.serial_label)("ABC123"),
        (s.erase_warning)("/dev/sdb"),
        (s.type_to_confirm)("/dev/sdb"),
        (s.split_note)("install.wim", 2),
        (s.split_note_confirmation)("install.wim", 2, "install.swm, install2.swm"),
        (s.done_prefix)("SHA-256: abc"),
        (s.image_size_label)("4.0GiB"),
        (s.one_fat32_partition)("4.0GiB", "1.0MiB"),
        (s.sha256_label)("abc123"),
        (s.verified_sha256)("abc123"),
        (s.files_copied)(42, "1.0GiB"),
        (s.files_checked)(42),
        (s.device_description)("/dev/sdb", "1.0GiB"),
        (s.device_description_long)("/dev/sdb", "USB Disk", "1.0GiB"),
        (s.error_device_not_found)("/dev/sdb"),
        (s.error_device_not_removable)("/dev/sdb"),
        (s.error_device_is_system_disk)("/dev/sdb"),
        (s.error_insufficient_permissions)("/dev/sdb"),
        (s.error_source_target_collision)("/tmp/x.iso"),
        (s.error_unsupported_iso)("/tmp/x.iso"),
        (s.error_not_windows_installer_iso)("/tmp/x.iso"),
        (s.error_checksum_mismatch)("aaa", "bbb"),
        (s.error_windows_partition_layout_mismatch)("detail"),
    ]
}

/// The category label for a `Helper` error's `exit_code`, or `None` for a
/// code this build doesn't recognise -- an older or newer `argos-helper`.
/// Order matches `ArgosError::exit_code`'s own match arms in
/// `argos-core/src/error.rs`; 24 and 25 are retired codes with nothing to
/// map (see that file's own comment), and 27 covers both `NotConfirmed` and
/// `ElevationDeclined`, same as it does there.
fn helper_category(exit_code: i32, s: &'static Strings) -> Option<&'static str> {
    Some(match exit_code {
        10 => s.helper_category_device_not_found,
        11 => s.helper_category_device_not_removable,
        12 => s.helper_category_device_is_system_disk,
        13 => s.helper_category_insufficient_permissions,
        14 => s.helper_category_device_too_small,
        15 => s.helper_category_source_target_collision,
        16 => s.helper_category_unsupported_iso,
        17 => s.helper_category_checksum_mismatch,
        18 => s.helper_category_cancelled,
        19 => s.helper_category_io,
        20 => s.helper_category_not_implemented,
        21 => s.helper_category_not_windows_installer_iso,
        22 => s.helper_category_windows_partition_layout_mismatch,
        23 => s.helper_category_windows_file_mismatch,
        26 => s.helper_category_windows_file_too_large_for_fat32,
        27 => s.helper_category_not_confirmed,
        _ => return None,
    })
}

/// Guesses the language from the desktop, with no opinion about a saved
/// config or `ARGOS_LANG` -- [`resolve_lang`] is what applies the full
/// precedence chain the milestone specifies (config > `ARGOS_LANG` >
/// detection > English). Kept separate so each layer is testable on its
/// own: this one's result depends on the environment the process actually
/// has, and a test can only fake so much of that.
pub fn detect_lang() -> Lang {
    #[cfg(target_os = "macos")]
    {
        macos_detect_lang().unwrap_or(Lang::En)
    }
    #[cfg(not(target_os = "macos"))]
    {
        linux_detect_lang(
            std::env::var("LC_ALL").ok(),
            std::env::var("LC_MESSAGES").ok(),
            std::env::var("LANG").ok(),
        )
    }
}

/// `LC_ALL` outranks `LC_MESSAGES` outranks `LANG`, the same precedence
/// `setlocale(3)` itself uses -- so an `LC_ALL=C` set for a script's benefit
/// is honoured over a `LANG=pt_BR.UTF-8` left in the user's shell profile,
/// same as every other locale-aware program on the system would read it.
/// Takes the three values as plain `Option<String>` rather than reading the
/// environment itself, so every combination is a table a test can drive
/// without mutating global process state.
#[cfg(not(target_os = "macos"))]
fn linux_detect_lang(
    lc_all: Option<String>,
    lc_messages: Option<String>,
    lang: Option<String>,
) -> Lang {
    // The first *defined* variable decides, on its own value alone -- same
    // as setlocale(3): `LC_ALL=C` is a real, higher-precedence answer ("no
    // locale"), not an absent one, so it must not be skipped in favour of a
    // `LANG` set further down for an interactive shell's benefit.
    match [lc_all, lc_messages, lang].into_iter().flatten().next() {
        Some(value) => lang_from_posix_locale(&value).unwrap_or(Lang::En),
        None => Lang::En,
    }
}

/// Reads only the language subtag: `pt_BR.UTF-8`, `pt_BR`, and bare `pt` all
/// mean the same thing here, since Argos ships exactly one Portuguese
/// variant. `C`/`POSIX` (no real language named) and anything not starting
/// with `pt` fall through to `None`, which the caller reads as "keep
/// checking the next variable" -- not as "this means English", since `LANG`
/// being unset entirely is handled the same way as it being `C`.
fn lang_from_posix_locale(value: &str) -> Option<Lang> {
    let language = value.split(['.', '_', '@']).next().unwrap_or("");
    match language {
        "pt" => Some(Lang::PtBr),
        "en" => Some(Lang::En),
        _ => None,
    }
}

/// `LANG` is frequently empty for a `.app` launched from Finder -- the
/// reason this milestone specifically calls for `defaults read -g
/// AppleLanguages` instead of trusting the environment on macOS at all.
/// **Not verified on real hardware**: this host is Linux, and the phase-4
/// GUI's own macOS validation (#100, #102) never exercised language
/// detection. Written to match `defaults`' documented output (an ordered
/// list, most-preferred first, `"en-US"`/`"pt-BR"` with a hyphen rather than
/// the POSIX underscore) rather than asserted against a real Mac.
#[cfg(target_os = "macos")]
fn macos_detect_lang() -> Option<Lang> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // The format is a parenthesised, comma-separated plist array, one
    // language per line, e.g.:
    //   (
    //       "pt-BR",
    //       "en-US"
    //   )
    // The first entry is the user's most-preferred language; only it
    // matters here, since there is no "second choice" concept in Lang.
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('"'))?;
    let first = first.trim_matches(|c| c == '"' || c == ',');
    lang_from_posix_locale(&first.replace('-', "_"))
}

/// The full precedence chain the milestone specifies: a saved config beats
/// `ARGOS_LANG` (present so tests and CI can pin a language without
/// touching the environment `detect_lang` reads) beats the desktop's own
/// setting beats English.
pub fn resolve_lang() -> Lang {
    if let Some(lang) = crate::config::load_lang_preference() {
        return lang;
    }
    resolve_lang_without_config()
}

/// The rest of [`resolve_lang`]'s precedence chain -- `ARGOS_LANG` then
/// detection then English -- without consulting the saved config. This is
/// what re-resolving after *clearing* a saved choice needs: right after
/// deleting the config's `lang` entry, calling `resolve_lang` again would
/// re-read the file and could still see a stale value on a filesystem that
/// doesn't order a delete before the next read as strictly as this process
/// assumes, or simply race a slow disk. A front end going back to
/// "Automatic" wants the answer without that file in the chain at all.
pub fn resolve_lang_without_config() -> Lang {
    if let Ok(value) = std::env::var("ARGOS_LANG") {
        if let Some(lang) = Lang::from_code(&value) {
            return lang;
        }
    }
    detect_lang()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_code_round_trips() {
        for lang in [Lang::En, Lang::PtBr] {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
    }

    #[test]
    fn an_unknown_code_is_not_a_language() {
        assert_eq!(Lang::from_code(""), None);
        assert_eq!(Lang::from_code("fr"), None);
        assert_eq!(Lang::from_code("PT_BR"), None); // case-sensitive on purpose
    }

    /// The compiler already enforces that both catalogues have every field;
    /// this exercises every function pointer with a representative call, so
    /// a placeholder that silently produces an empty or malformed string --
    /// which the type system cannot catch -- fails a test instead of
    /// shipping.
    #[test]
    fn every_parameterized_message_produces_non_empty_text_in_both_languages() {
        for lang in [Lang::En, Lang::PtBr] {
            let name = strings_for(lang).lang_name;
            for text in sample_all_messages(lang) {
                assert!(!text.is_empty(), "{name}: produced an empty string");
                assert!(
                    !text.contains("{}") && !text.contains("{ }"),
                    "{name}: {text:?} looks like an unfilled placeholder"
                );
            }
        }
    }

    /// Every `helper_category` code `ArgosError::exit_code` can actually
    /// produce is mapped in both languages -- an unmapped one would fall
    /// through to the generic prefix silently, which is correct for a
    /// *future* code but would be a real gap for one this build already
    /// knows about.
    #[test]
    fn every_known_helper_exit_code_has_a_category_in_both_languages() {
        for lang in [Lang::En, Lang::PtBr] {
            let s = strings_for(lang);
            for code in [
                10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 26, 27,
            ] {
                assert!(
                    helper_category(code, s).is_some(),
                    "{}: exit code {code} has no category",
                    s.lang_name
                );
            }
        }
    }

    /// The two retired codes (#89's own comment: WindowsImageRequiresLinux
    /// and WindowsFileTooLargeForAvailableMemory) and an out-of-range code
    /// both mean "this build doesn't know what this is" -- version skew
    /// against a helper from a different Argos version -- and must fall
    /// through to the generic prefix rather than a wrong category.
    #[test]
    fn an_unrecognized_exit_code_has_no_category() {
        for code in [24, 25, 999] {
            assert_eq!(helper_category(code, &EN), None, "code {code}");
        }
    }

    #[test]
    fn localize_error_translates_a_typed_variant_with_its_real_parameter() {
        let err = ArgosError::DeviceNotFound("/dev/sdz".into());
        assert_eq!(
            localize_error(&err, Lang::En),
            "no device matches '/dev/sdz'"
        );
        assert_eq!(
            localize_error(&err, Lang::PtBr),
            "nenhum dispositivo corresponde a '/dev/sdz'"
        );
    }

    #[test]
    fn localize_error_categorizes_a_known_helper_exit_code() {
        let err = ArgosError::Helper {
            message: "device '/dev/sdz' looks like a system disk".into(),
            exit_code: 12,
        };
        // Only the translated category -- not the raw English message
        // concatenated onto it. Found on real hardware: yanking a USB stick
        // mid-write produced a Portuguese category glued to an entire
        // English sentence on the same, un-expandable line. The raw text is
        // not lost; it is what "Details" shows, from `error.to_string()`.
        assert_eq!(
            localize_error(&err, Lang::En),
            "the device looks like a system disk"
        );
        assert_eq!(
            localize_error(&err, Lang::PtBr),
            "o dispositivo parece ser um disco de sistema"
        );
    }

    /// The rule G6.5 states outright: a code with no mapping shows the raw
    /// English message behind a translated prefix, never a bare number.
    #[test]
    fn localize_error_falls_back_to_the_generic_prefix_for_an_unknown_exit_code() {
        let err = ArgosError::Helper {
            message: "some future failure".into(),
            exit_code: 99,
        };
        let text = localize_error(&err, Lang::PtBr);
        assert_eq!(
            text,
            "o processo privilegiado relatou um erro: some future failure"
        );
        assert!(
            !text.contains("99"),
            "a bare exit code leaked into the message: {text}"
        );
    }

    // ---- Linux locale detection ---------------------------------------------

    #[test]
    fn a_bare_pt_br_locale_is_portuguese() {
        assert_eq!(lang_from_posix_locale("pt_BR.UTF-8"), Some(Lang::PtBr));
        assert_eq!(lang_from_posix_locale("pt_BR"), Some(Lang::PtBr));
        assert_eq!(lang_from_posix_locale("pt"), Some(Lang::PtBr));
        // pt_PT and any other Portuguese variant still maps to Argos's one
        // Portuguese catalogue rather than falling back to English --
        // there is no pt_PT-specific wording to prefer over pt_BR's.
        assert_eq!(lang_from_posix_locale("pt_PT.UTF-8"), Some(Lang::PtBr));
    }

    #[test]
    fn c_and_posix_are_not_a_language() {
        assert_eq!(lang_from_posix_locale("C"), None);
        assert_eq!(lang_from_posix_locale("POSIX"), None);
        assert_eq!(lang_from_posix_locale(""), None);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn lc_all_outranks_lc_messages_outranks_lang() {
        assert_eq!(
            linux_detect_lang(
                Some("pt_BR.UTF-8".into()),
                Some("en_US.UTF-8".into()),
                Some("en_US.UTF-8".into()),
            ),
            Lang::PtBr,
            "LC_ALL must win"
        );
        assert_eq!(
            linux_detect_lang(None, Some("pt_BR.UTF-8".into()), Some("en_US.UTF-8".into())),
            Lang::PtBr,
            "LC_MESSAGES must win when LC_ALL is unset"
        );
        assert_eq!(
            linux_detect_lang(None, None, Some("pt_BR.UTF-8".into())),
            Lang::PtBr,
            "LANG is the last resort"
        );
    }

    /// `LC_ALL=C` (a script pinning the "no locale" locale) must not be
    /// skipped in favour of a `LANG` set for the interactive shell -- `C`
    /// is a real, higher-precedence answer, just not one that names a
    /// language, so the chain falls through to English rather than reading
    /// past LC_ALL to LANG.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn an_unrecognized_higher_precedence_variable_falls_through_to_english_not_the_next_variable() {
        assert_eq!(
            linux_detect_lang(Some("C".into()), None, Some("pt_BR.UTF-8".into())),
            Lang::En
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_locale_variables_set_at_all_is_english() {
        assert_eq!(linux_detect_lang(None, None, None), Lang::En);
    }
}
