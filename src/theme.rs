use ratatui::style::{Color, Modifier, Style};

pub struct Theme {
    pub accent:  Color,
    pub fg:      Color,
    pub dim:     Color,
    pub bg:      Color,
    pub bg_sel:  Color,
    pub ok:      Color,
    pub warn:    Color,
    pub attn:    Color,
    pub info:    Color,
    pub err:     Color,
    pub muted:   Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            accent: Color::Rgb(0x3f, 0xc8, 0xff),
            fg:     Color::Rgb(0xe6, 0xe8, 0xee),
            dim:    Color::Rgb(0x8a, 0x90, 0xa0),
            bg:     Color::Rgb(0x11, 0x13, 0x1a),
            bg_sel: Color::Rgb(0x1d, 0x22, 0x30),
            ok:     Color::Rgb(0x7c, 0xd9, 0x92),
            warn:   Color::Rgb(0xe6, 0xb6, 0x5a),
            attn:   Color::Rgb(0x4f, 0xd1, 0xff),
            info:   Color::Rgb(0x6a, 0x9c, 0xff),
            err:    Color::Rgb(0xe5, 0x6a, 0x6a),
            muted:  Color::Rgb(0x4b, 0x50, 0x5c),
        }
    }

    pub fn border_focused(&self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn border_unfocused(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn title_focused(&self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn title_unfocused(&self) -> Style {
        Style::default().fg(self.fg)
    }
}
