//! Reads the desktop's light/dark preference straight from the XDG desktop
//! portal, because `winit`'s X11 backend does not report one at all.
//!
//! `winit-0.30.13/src/platform_impl/linux/x11/window.rs` defines
//! `theme(&self) -> Option<Theme> { None }` -- hardcoded, only implemented on
//! the Wayland backend. `egui` then falls back to
//! `Options::fallback_theme`, whose default is `Theme::Dark`
//! (`egui-0.33.3/src/memory/mod.rs`). The result, confirmed on real hardware
//! (Ubuntu 24.04, GNOME 46, X11): the window renders dark unconditionally,
//! independent of the desktop's actual setting -- forcing `prefer-light` in
//! GNOME and reading the palette back by pixel still showed
//! [`crate::theme::DARK`]'s colours.
//!
//! `org.freedesktop.appearance`'s `color-scheme` is what GNOME, KDE and every
//! other portal-implementing desktop actually publish this on, and the
//! portal answers identically under X11 and Wayland -- so this replaces
//! `ctx.system_theme()` as the source of truth on Linux entirely, rather than
//! patching only the X11 case and leaving two code paths to keep in sync.
//!
//! Costs no new dependency: `zbus` is already in the tree via
//! `accesskit_unix`'s AT-SPI support (`cargo tree` before and after this
//! module: same 187 crates).

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

/// `color-scheme`'s three documented values: 0 = no preference, 1 = prefer
/// dark, 2 = prefer light. Anything else is a portal version this wasn't
/// written against; treated the same as "no preference" -- there is nothing
/// in it to override the caller's own fallback with.
fn interpret_color_scheme(value: u32) -> Option<bool> {
    match value {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Blocks on one D-Bus round trip (session bus, local socket -- a few
/// milliseconds in practice), so call this from a worker thread and never
/// from `update`. Returns `None` on anything short of a clean answer: no
/// portal running, an unsupported desktop, no preference set. The caller's
/// own fallback (`ctx.theme()`, which already works correctly on macOS and
/// under Wayland) takes over silently in every `None` case, so a desktop
/// with no portal is no worse off than before this existed.
pub fn read_system_dark_preference() -> Option<bool> {
    let connection = Connection::session().ok()?;
    let proxy = Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .ok()?;
    let reply: OwnedValue = proxy
        .call("Read", &("org.freedesktop.appearance", "color-scheme"))
        .ok()?;
    // Settings.Read's reply is a variant *containing* a variant -- confirmed
    // against the real portal (busctl and a throwaway zbus probe agreed):
    // unwrapping only the outer layer and reading a u32 directly fails.
    let Value::Value(inner) = Value::from(reply) else {
        return None;
    };
    let raw = u32::try_from(*inner).ok()?;
    interpret_color_scheme(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_three_documented_values() {
        assert_eq!(interpret_color_scheme(0), None); // no preference
        assert_eq!(interpret_color_scheme(1), Some(true));
        assert_eq!(interpret_color_scheme(2), Some(false));
    }

    /// A future portal version adding a fourth value must not be read as an
    /// opinion this code was never told how to interpret.
    #[test]
    fn an_unrecognized_value_is_treated_as_no_opinion() {
        assert_eq!(interpret_color_scheme(99), None);
    }
}
