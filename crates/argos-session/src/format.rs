//! Formatting shared by every front end.

use crate::lang::Lang;

/// Moved verbatim from `argos-cli`'s `commands::helper`, output unchanged.
///
/// Deliberately *not* localized: this feeds `argos list`'s fixed-width
/// `{:>10}` column and every confirmation prompt, so changing the decimal
/// separator or the spacing would silently change CLI output. A localized
/// variant belongs next to a front end's own string catalogue, as a separate
/// function.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

/// `human_size`'s localized sibling, for a front end's own display -- never
/// for `argos list`'s column or a CLI prompt, which is why this is a
/// separate function rather than a parameter on `human_size` itself (see
/// its own doc comment). Only the decimal separator changes: the unit
/// abbreviations (`KiB`, `GiB`, ...) are the international binary-prefix
/// symbols, unchanged by Brazilian Portuguese technical writing, and giving
/// them their own translated forms would only make a hex dump or a size in
/// pt-BR less recognisable to anyone who has seen one in English.
pub fn human_size_localized(bytes: u64, lang: Lang) -> String {
    let english = human_size(bytes);
    match lang {
        Lang::En => english,
        Lang::PtBr => english.replace('.', ","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_bytes_and_larger_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5KiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0GiB");
    }

    #[test]
    fn localized_english_is_unchanged() {
        assert_eq!(human_size_localized(1536, Lang::En), "1.5KiB");
    }

    #[test]
    fn localized_portuguese_uses_a_comma() {
        assert_eq!(human_size_localized(1536, Lang::PtBr), "1,5KiB");
    }

    /// A whole-byte count has no decimal point at all, in either language --
    /// nothing to localize, and nothing this function should invent.
    #[test]
    fn localized_portuguese_leaves_whole_byte_counts_alone() {
        assert_eq!(human_size_localized(512, Lang::PtBr), "512B");
    }
}
