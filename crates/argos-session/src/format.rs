//! Formatting shared by every front end.

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
}
