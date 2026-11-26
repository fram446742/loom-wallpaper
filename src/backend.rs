//! Backend selection: the native layer-shell renderer, or the external command
//! template (`swaybg`/`swww`), with graceful degradation.

use std::fmt;

/// Which renderer to use for wallpapers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// Prefer the native renderer, fall back to commands when Wayland is
    /// unavailable. This is the default.
    #[default]
    Auto,
    /// The native `wlr-layer-shell` background renderer.
    Native,
    /// Spawn an external command per output (the original behaviour).
    Command,
}

impl Backend {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "native" => Ok(Self::Native),
            "command" => Ok(Self::Command),
            other => Err(format!(
                "unknown backend `{other}` (expected auto, native or command)"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Native => "native",
            Self::Command => "command",
        }
    }

    /// Reads `$LOOM_WALLPAPER_BACKEND`, defaulting to `auto`.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("LOOM_WALLPAPER_BACKEND") {
            Ok(value) if !value.trim().is_empty() => Self::parse(value.trim()),
            _ => Ok(Self::Auto),
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The backend that will actually run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Native,
    Command,
}

impl Selection {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Command => "command",
        }
    }
}

impl fmt::Display for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resolves the requested backend against the native probe result.
///
/// `native` is the result of probing the compositor (`Ok` when a native
/// background surface can be created there, `Err` with a human-readable reason
/// otherwise). `auto` degrades to the command backend with a warning; an
/// explicit `native` request fails loudly so a misconfigured session is not
/// silently swapped for a different renderer.
pub fn select(requested: Backend, native: Result<(), String>) -> Result<Selection, String> {
    match (requested, native) {
        (Backend::Command, _) => Ok(Selection::Command),
        (Backend::Native, Ok(())) => Ok(Selection::Native),
        (Backend::Native, Err(reason)) => Err(format!(
            "the native backend is unavailable: {reason}\n\
             use --backend=command, or omit --backend to degrade automatically"
        )),
        (Backend::Auto, Ok(())) => Ok(Selection::Native),
        (Backend::Auto, Err(reason)) => {
            eprintln!(
                "loom-wallpaper: native backend unavailable ({reason}); using the command backend"
            );
            Ok(Selection::Command)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_names() {
        assert_eq!(Backend::parse("auto").unwrap(), Backend::Auto);
        assert_eq!(Backend::parse("native").unwrap(), Backend::Native);
        assert_eq!(Backend::parse("command").unwrap(), Backend::Command);
        assert!(Backend::parse("x11").is_err());
        assert_eq!(Backend::default(), Backend::Auto);
        assert_eq!(Backend::Native.to_string(), "native");
    }

    #[test]
    fn auto_prefers_native_when_available() {
        assert_eq!(select(Backend::Auto, Ok(())).unwrap(), Selection::Native);
    }

    #[test]
    fn auto_degrades_to_command_without_wayland() {
        assert_eq!(
            select(Backend::Auto, Err("no wayland display".to_owned())).unwrap(),
            Selection::Command
        );
    }

    #[test]
    fn explicit_native_reports_the_reason() {
        let err = select(
            Backend::Native,
            Err("zwlr_layer_shell_v1 missing".to_owned()),
        )
        .unwrap_err();
        assert!(err.contains("zwlr_layer_shell_v1 missing"), "{err}");
        assert!(err.contains("--backend=command"), "{err}");
    }

    #[test]
    fn explicit_command_never_probes() {
        assert_eq!(
            select(Backend::Command, Err("no wayland display".to_owned())).unwrap(),
            Selection::Command
        );
        assert_eq!(
            select(Backend::Command, Ok(())).unwrap(),
            Selection::Command
        );
    }
}
