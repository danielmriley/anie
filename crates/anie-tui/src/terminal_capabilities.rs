//! Static terminal-capability detection.
//!
//! Shape mirrors pi's `TerminalCapabilities`
//! (`packages/tui/src/terminal-image.ts`): three fields, each
//! baking in the multiplexer / env conditions. Over-engineering
//! an eight-field breakdown proved tempting but pi's three-field
//! shape is what consumers actually need (yes/no answers).
//!
//! Detection is a pure function of the environment — no dynamic
//! DA/DA2 queries. Those would interleave awkwardly with
//! crossterm's event stream for little payoff.
//!
//! **anie-specific (not in pi):** none yet. User-override env
//! vars (`ANIE_FORCE_OSC8=1` etc.) are deferred until a concrete
//! need surfaces.

use std::collections::HashMap;

/// What the current terminal supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalCapabilities {
    /// Which inline-image protocol is supported, if any. `None`
    /// also covers "images advertised but we're inside
    /// tmux/screen without passthrough" — consumers can treat
    /// this as a simple boolean check.
    pub images: Option<ImageProtocol>,
    /// 24-bit colour is advertised by the terminal.
    pub truecolor: bool,
    /// OSC 8 hyperlinks are safe to emit. False inside
    /// tmux/screen by default, true in most native terminals we
    /// recognise.
    pub hyperlinks: bool,
}

/// Supported inline-image protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

impl TerminalCapabilities {
    /// Detect capabilities from the current process environment.
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_from(&env_snapshot())
    }

    /// Detect capabilities from an explicit env snapshot.
    /// Factored out so tests don't touch global process state.
    #[must_use]
    pub fn detect_from(env: &HashMap<String, String>) -> Self {
        let get = |key: &str| env.get(key).map(String::as_str);
        let any = |keys: &[&str]| keys.iter().any(|k| get(k).is_some());

        let inside_multiplexer = get("TMUX").is_some()
            || get("TERM").is_some_and(|v| v.starts_with("tmux") || v.starts_with("screen"));

        let term_program = get("TERM_PROGRAM").unwrap_or_default();

        let is_kitty = any(&["KITTY_WINDOW_ID"])
            || term_program.eq_ignore_ascii_case("ghostty")
            || get("GHOSTTY_RESOURCES_DIR").is_some();
        let is_iterm =
            term_program.eq_ignore_ascii_case("iTerm.app") || get("ITERM_SESSION_ID").is_some();
        let is_wezterm =
            term_program.eq_ignore_ascii_case("WezTerm") || get("WEZTERM_PANE").is_some();
        let is_vscode = term_program.eq_ignore_ascii_case("vscode");
        let is_alacritty =
            term_program.eq_ignore_ascii_case("alacritty") || get("ALACRITTY_WINDOW_ID").is_some();

        // Image protocol: only honored outside multiplexers.
        // Kitty and Ghostty both speak the Kitty protocol. iTerm
        // and WezTerm both speak iTerm2's inline-image escape.
        let images = if inside_multiplexer {
            None
        } else if is_kitty {
            Some(ImageProtocol::Kitty)
        } else if is_iterm || is_wezterm {
            Some(ImageProtocol::Iterm2)
        } else {
            None
        };

        // Truecolor: advertised via COLORTERM or implied by
        // terminals we know emit it. VS Code's terminal also
        // supports truecolor.
        let truecolor = get("COLORTERM").is_some_and(|v| {
            v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit")
        }) || is_kitty
            || is_iterm
            || is_wezterm
            || is_vscode
            || is_alacritty;

        // OSC 8 hyperlinks: disabled inside multiplexers unless
        // future work surfaces a passthrough override.
        // Native terminals we recognise all support OSC 8.
        let hyperlinks = !inside_multiplexer
            && (is_kitty || is_iterm || is_wezterm || is_vscode || is_alacritty);

        Self {
            images,
            truecolor,
            hyperlinks,
        }
    }
}

fn env_snapshot() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<const N: usize>(pairs: [(&str, &str); N]) -> HashMap<String, String> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn empty_env_produces_conservative_defaults() {
        let caps = TerminalCapabilities::detect_from(&HashMap::new());
        assert_eq!(caps.images, None);
        assert!(!caps.truecolor);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn kitty_detected_via_kitty_window_id() {
        let caps = TerminalCapabilities::detect_from(&env([("KITTY_WINDOW_ID", "1")]));
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
        assert!(caps.truecolor);
    }

    #[test]
    fn ghostty_detected_via_term_program_and_maps_to_kitty_images() {
        let caps = TerminalCapabilities::detect_from(&env([("TERM_PROGRAM", "ghostty")]));
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    #[test]
    fn iterm_detected_via_term_program() {
        let caps = TerminalCapabilities::detect_from(&env([("TERM_PROGRAM", "iTerm.app")]));
        assert_eq!(caps.images, Some(ImageProtocol::Iterm2));
        assert!(caps.hyperlinks);
    }

    #[test]
    fn iterm_detected_via_iterm_session_id() {
        let caps = TerminalCapabilities::detect_from(&env([("ITERM_SESSION_ID", "w0t0p0:123")]));
        assert_eq!(caps.images, Some(ImageProtocol::Iterm2));
    }

    #[test]
    fn wezterm_detected_via_wezterm_pane() {
        let caps = TerminalCapabilities::detect_from(&env([("WEZTERM_PANE", "42")]));
        assert_eq!(caps.images, Some(ImageProtocol::Iterm2));
        assert!(caps.hyperlinks);
    }

    #[test]
    fn vscode_terminal_supports_hyperlinks_and_truecolor_but_no_images() {
        let caps = TerminalCapabilities::detect_from(&env([("TERM_PROGRAM", "vscode")]));
        assert_eq!(caps.images, None);
        assert!(caps.truecolor);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn alacritty_detected_via_alacritty_window_id() {
        let caps = TerminalCapabilities::detect_from(&env([("ALACRITTY_WINDOW_ID", "abc")]));
        assert!(caps.truecolor);
        assert!(caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn colorterm_truecolor_enables_truecolor() {
        let caps = TerminalCapabilities::detect_from(&env([("COLORTERM", "truecolor")]));
        assert!(caps.truecolor);
    }

    #[test]
    fn colorterm_24bit_enables_truecolor() {
        let caps = TerminalCapabilities::detect_from(&env([("COLORTERM", "24bit")]));
        assert!(caps.truecolor);
    }

    #[test]
    fn tmux_env_disables_hyperlinks_and_images_even_in_kitty() {
        let caps = TerminalCapabilities::detect_from(&env([
            ("KITTY_WINDOW_ID", "1"),
            ("TMUX", "/tmp/tmux-1000/default,1234,0"),
        ]));
        assert_eq!(caps.images, None);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn term_starting_with_tmux_disables_hyperlinks() {
        let caps = TerminalCapabilities::detect_from(&env([
            ("TERM", "tmux-256color"),
            ("TERM_PROGRAM", "iTerm.app"),
        ]));
        assert_eq!(caps.images, None);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn term_starting_with_screen_disables_hyperlinks() {
        let caps = TerminalCapabilities::detect_from(&env([("TERM", "screen.xterm-256color")]));
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn unknown_terminal_is_conservative() {
        // Not a multiplexer but also not one we recognise — no
        // hyperlinks, no images, no truecolor claim.
        let caps = TerminalCapabilities::detect_from(&env([("TERM", "xterm-256color")]));
        assert_eq!(caps.images, None);
        assert!(!caps.truecolor);
        assert!(!caps.hyperlinks);
    }
}
