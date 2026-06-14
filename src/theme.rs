//! Centralized semantic color palette. Rich 24-bit RGB by default; when the
//! `NO_COLOR` env var is set (https://no-color.org), falls back to the 16 ANSI
//! named colors so the user's terminal theme governs the palette.
//!
//! Every field is named by ROLE, not by call site, and holds one DISTINCT rgb
//! value. The `rich()` palette reproduces, byte-for-byte, every `Color::Rgb`
//! triple the views used before this module existed; that byte-identity is the
//! invariant that makes routing all of those sites through `theme()` a no-op in
//! the default (non-`NO_COLOR`) case.
use ratatui::style::Color;
use std::sync::OnceLock;

/// Semantic palette. One field per distinct color role.
pub struct Theme {
    /// Inactive panel/popup border.
    pub border: Color,
    /// Active/focused border (e.g. an edited field, an active pane).
    pub border_active: Color,
    /// Conversation-list (left pane) border.
    pub list_border: Color,
    /// Composer border while idle (not editing).
    pub input_border_idle: Color,
    /// Selected-row background in lists.
    pub list_highlight: Color,
    /// Selected-row foreground in lists.
    pub list_highlight_fg: Color,
    /// Panel/popup title text.
    pub title: Color,
    /// Conversation-list pane title text.
    pub list_title: Color,
    /// Hint-bar key glyphs.
    pub hint_key: Color,
    /// Hint-bar separators / table borders.
    pub hint_sep: Color,
    /// Dim secondary text (hint descriptions, status dim, "global" labels).
    pub text_dim: Color,
    /// Even dimmer count text (e.g. message counts).
    pub count_dim: Color,
    /// "You:" user-prefix accent (also the rename popup accent / mode chip bg).
    pub user_prefix: Color,
    /// Assistant name prefix.
    pub assistant_prefix: Color,
    /// Rename popup title text.
    pub rename_title: Color,
    /// Success / ok / streaming-assistant text.
    pub ok: Color,
    /// Warning text (distinct from the amber context-fill shade).
    pub warn: Color,
    /// Error text / error & delete borders.
    pub error: Color,
    /// Bright error message body text.
    pub error_text: Color,
    /// Context-fill indicator: comfortably under budget.
    pub ctx_green: Color,
    /// Context-fill indicator: approaching the compaction line.
    pub ctx_amber: Color,
    /// Context-fill indicator: at/over budget.
    pub ctx_red: Color,
    /// Debug tool-call accent / pending-task marker.
    pub debug_tool: Color,
    /// Debug system text / inherited-value marker / timestamps.
    pub debug_system: Color,
    /// Assistant indicator / informational blue accent.
    pub assistant_indicator: Color,
    /// Running-task accent / normal-mode chip bg / running badge.
    pub run: Color,
    /// Edit-mode chip background.
    pub mode_edit_bg: Color,
    /// Pinned / current-pick highlight.
    pub pinned: Color,
    /// Markdown code-span foreground.
    pub code_fg: Color,
    /// Markdown code-span background.
    pub code_bg: Color,
    /// Markdown link text.
    pub link: Color,
    /// Markdown blockquote text.
    pub blockquote: Color,
}

impl Theme {
    /// The 24-bit palette. Reproduces every current RGB value byte-for-byte.
    fn rich() -> Self {
        Self {
            border: Color::Rgb(82, 104, 173),
            border_active: Color::Rgb(120, 183, 109),
            list_border: Color::Rgb(62, 125, 146),
            input_border_idle: Color::Rgb(109, 122, 143),
            list_highlight: Color::Rgb(72, 102, 180),
            list_highlight_fg: Color::Rgb(245, 248, 255),
            title: Color::Rgb(166, 182, 255),
            list_title: Color::Rgb(136, 214, 240),
            hint_key: Color::Rgb(216, 223, 236),
            hint_sep: Color::Rgb(82, 90, 110),
            text_dim: Color::Rgb(143, 153, 174),
            count_dim: Color::Rgb(124, 132, 148),
            user_prefix: Color::Rgb(255, 189, 89),
            assistant_prefix: Color::Rgb(92, 206, 154),
            rename_title: Color::Rgb(255, 220, 160),
            ok: Color::Rgb(132, 218, 193),
            warn: Color::Rgb(232, 200, 130),
            error: Color::Rgb(232, 130, 130),
            error_text: Color::Rgb(255, 200, 200),
            ctx_green: Color::Rgb(122, 200, 132),
            ctx_amber: Color::Rgb(232, 184, 96),
            ctx_red: Color::Rgb(232, 106, 106),
            debug_tool: Color::Rgb(178, 138, 220),
            debug_system: Color::Rgb(140, 156, 196),
            assistant_indicator: Color::Rgb(178, 220, 245),
            run: Color::Rgb(122, 163, 255),
            mode_edit_bg: Color::Rgb(120, 214, 118),
            pinned: Color::Rgb(255, 207, 119),
            code_fg: Color::Rgb(244, 228, 188),
            code_bg: Color::Rgb(40, 44, 56),
            link: Color::Rgb(132, 204, 232),
            blockquote: Color::Rgb(170, 178, 196),
        }
    }

    /// `NO_COLOR` fallback: nearest ANSI-16. Borders and recessive backgrounds
    /// become `Color::Reset` (terminal default) so the user's terminal theme
    /// shows through; the list-highlight background stays a visible `Blue` so
    /// the selection remains legible.
    fn plain() -> Self {
        Self {
            border: Color::Reset,
            border_active: Color::Green,
            list_border: Color::Reset,
            input_border_idle: Color::Reset,
            list_highlight: Color::Blue,
            list_highlight_fg: Color::White,
            title: Color::Cyan,
            list_title: Color::LightBlue,
            hint_key: Color::White,
            hint_sep: Color::DarkGray,
            text_dim: Color::Gray,
            count_dim: Color::DarkGray,
            user_prefix: Color::Yellow,
            assistant_prefix: Color::Green,
            rename_title: Color::LightYellow,
            ok: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            error_text: Color::LightRed,
            ctx_green: Color::Green,
            ctx_amber: Color::Yellow,
            ctx_red: Color::Red,
            debug_tool: Color::Magenta,
            debug_system: Color::LightBlue,
            assistant_indicator: Color::LightCyan,
            run: Color::LightBlue,
            mode_edit_bg: Color::Green,
            pinned: Color::LightYellow,
            code_fg: Color::Yellow,
            code_bg: Color::Reset,
            link: Color::Cyan,
            blockquote: Color::Gray,
        }
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Global palette accessor. Initialized once from the environment: the rich
/// 24-bit palette by default, or the ANSI-16 `plain()` fallback when `NO_COLOR`
/// is set.
pub fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            Theme::plain()
        } else {
            Theme::rich()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every field into a list so a test can assert a property over the
    /// whole palette without naming each field twice.
    fn all_colors(t: &Theme) -> Vec<Color> {
        vec![
            t.border,
            t.border_active,
            t.list_border,
            t.input_border_idle,
            t.list_highlight,
            t.list_highlight_fg,
            t.title,
            t.list_title,
            t.hint_key,
            t.hint_sep,
            t.text_dim,
            t.count_dim,
            t.user_prefix,
            t.assistant_prefix,
            t.rename_title,
            t.ok,
            t.warn,
            t.error,
            t.error_text,
            t.ctx_green,
            t.ctx_amber,
            t.ctx_red,
            t.debug_tool,
            t.debug_system,
            t.assistant_indicator,
            t.run,
            t.mode_edit_bg,
            t.pinned,
            t.code_fg,
            t.code_bg,
            t.link,
            t.blockquote,
        ]
    }

    #[test]
    fn rich_constructs_and_border_is_unchanged() {
        // Constructing must not panic, and a couple of fields must equal their
        // known RGB triples — this guards the byte-identity invariant that
        // makes routing the views through `theme()` safe in the default case.
        let t = Theme::rich();
        assert_eq!(t.border, Color::Rgb(82, 104, 173));
        assert_eq!(t.list_highlight, Color::Rgb(72, 102, 180));
        assert_eq!(t.error, Color::Rgb(232, 130, 130));
        // Sanity: every field is populated (the list length tracks the count).
        assert_eq!(all_colors(&t).len(), 32);
    }

    #[test]
    fn plain_drops_all_rgb() {
        // The NO_COLOR path must actually drop 24-bit color: no field may be a
        // `Color::Rgb(..)` variant.
        for c in all_colors(&Theme::plain()) {
            assert!(
                !matches!(c, Color::Rgb(..)),
                "plain() field unexpectedly held an Rgb variant: {c:?}",
            );
        }
    }
}
