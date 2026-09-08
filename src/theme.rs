//! 色の解決。端末が持つ 256 色パレットと、画面各部の色を定める。
//!
//! 配色は Dracula を基にする。面は暗い順に端末、クローム、浮いた面の 3 段、
//! 文字は明るい順に主、副、三次の 3 段で階層を作る。

use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    // --- 端末の色 ---
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    /// 選択中のセルの背景。前景色は変えない。
    pub selection: Rgb,
    /// ANSI 16 色（通常 8 色に続けて明色 8 色）。
    pub ansi: [Rgb; 16],

    // --- 面。端末より明るくして手前にあることを示す ---
    /// 左ペインなど、端末の外側。
    pub chrome_bg: Rgb,
    /// 選択行や重ねた一覧など、さらに手前の面。
    pub surface: Rgb,

    // --- 文字の 3 段階 ---
    pub fg_primary: Rgb,
    pub fg_secondary: Rgb,
    pub fg_tertiary: Rgb,

    // --- 強調 ---
    pub accent: Rgb,
    pub warn: Rgb,
    /// 検索で見つかった箇所。
    pub search_hit: Rgb,
    /// そのうち、いま選んでいる箇所。
    pub search_current: Rgb,
    /// 強調した箇所の上に載せる文字色。
    pub search_fg: Rgb,
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: rgb(0xF8, 0xF8, 0xF2),
            bg: rgb(0x28, 0x2A, 0x36),
            cursor: rgb(0xF8, 0xF8, 0xF2),
            selection: rgb(0x44, 0x47, 0x5A),
            ansi: [
                rgb(0x21, 0x22, 0x2C), // black
                rgb(0xFF, 0x55, 0x55), // red
                rgb(0x50, 0xFA, 0x7B), // green
                rgb(0xF1, 0xFA, 0x8C), // yellow
                rgb(0xBD, 0x93, 0xF9), // blue
                rgb(0xFF, 0x79, 0xC6), // magenta
                rgb(0x8B, 0xE9, 0xFD), // cyan
                rgb(0xF8, 0xF8, 0xF2), // white
                rgb(0x62, 0x72, 0xA4), // bright black
                rgb(0xFF, 0x6E, 0x6E), // bright red
                rgb(0x69, 0xFF, 0x94), // bright green
                rgb(0xFF, 0xFF, 0xA5), // bright yellow
                rgb(0xD6, 0xAC, 0xFF), // bright blue
                rgb(0xFF, 0x92, 0xDF), // bright magenta
                rgb(0xA4, 0xFF, 0xFF), // bright cyan
                rgb(0xFF, 0xFF, 0xFF), // bright white
            ],
            chrome_bg: rgb(0x32, 0x34, 0x3F),
            surface: rgb(0x3C, 0x3E, 0x48),
            fg_primary: rgb(0xF8, 0xF8, 0xF2),
            fg_secondary: rgb(0xB0, 0xB1, 0xB0),
            fg_tertiary: rgb(0x8A, 0x8B, 0x90),
            accent: rgb(0x50, 0xFA, 0x7B),
            warn: rgb(0xFF, 0x55, 0x55),
            search_hit: rgb(0xF1, 0xFA, 0x8C),
            search_current: rgb(0xFF, 0xB8, 0x6C),
            search_fg: rgb(0x28, 0x2A, 0x36),
        }
    }
}

impl Theme {
    /// 256 色パレットの i 番を返す。
    pub fn indexed(&self, i: usize) -> Rgb {
        match i {
            0..=15 => self.ansi[i],
            16..=231 => {
                let i = i - 16;
                let steps = [0u8, 95, 135, 175, 215, 255];
                rgb(steps[(i / 36) % 6], steps[(i / 6) % 6], steps[i % 6])
            }
            232..=255 => {
                let v = 8 + 10 * (i as u8 - 232);
                rgb(v, v, v)
            }
            257 => self.fg,
            256 => self.bg,
            _ => self.fg,
        }
    }

    pub fn named(&self, n: NamedColor) -> Rgb {
        use NamedColor::*;
        match n {
            Black => self.ansi[0],
            Red => self.ansi[1],
            Green => self.ansi[2],
            Yellow => self.ansi[3],
            Blue => self.ansi[4],
            Magenta => self.ansi[5],
            Cyan => self.ansi[6],
            White => self.ansi[7],
            BrightBlack => self.ansi[8],
            BrightRed => self.ansi[9],
            BrightGreen => self.ansi[10],
            BrightYellow => self.ansi[11],
            BrightBlue => self.ansi[12],
            BrightMagenta => self.ansi[13],
            BrightCyan => self.ansi[14],
            BrightWhite => self.ansi[15],
            Foreground | BrightForeground => self.fg,
            Background => self.bg,
            Cursor => self.cursor,
            DimBlack => dim(self.ansi[0]),
            DimRed => dim(self.ansi[1]),
            DimGreen => dim(self.ansi[2]),
            DimYellow => dim(self.ansi[3]),
            DimBlue => dim(self.ansi[4]),
            DimMagenta => dim(self.ansi[5]),
            DimCyan => dim(self.ansi[6]),
            DimWhite => dim(self.ansi[7]),
            DimForeground => dim(self.fg),
        }
    }

    /// セルの色指定を RGB へ解決する。`colors` は端末が受け取った上書き指定。
    pub fn resolve(&self, c: AnsiColor, colors: &alacritty_terminal::term::color::Colors) -> Rgb {
        match c {
            AnsiColor::Spec(rgb) => rgb,
            AnsiColor::Named(n) => colors[n].unwrap_or_else(|| self.named(n)),
            AnsiColor::Indexed(i) => colors[i as usize].unwrap_or_else(|| self.indexed(i as usize)),
        }
    }
}

fn dim(c: Rgb) -> Rgb {
    rgb(
        (c.r as u16 * 2 / 3) as u8,
        (c.g as u16 * 2 / 3) as u8,
        (c.b as u16 * 2 / 3) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 立方体の色を計算する() {
        let t = Theme::default();
        assert_eq!(t.indexed(16), rgb(0, 0, 0));
        assert_eq!(t.indexed(231), rgb(255, 255, 255));
        assert_eq!(t.indexed(21), rgb(0, 0, 255));
    }

    #[test]
    fn 灰色階調を計算する() {
        let t = Theme::default();
        assert_eq!(t.indexed(232), rgb(8, 8, 8));
        assert_eq!(t.indexed(255), rgb(238, 238, 238));
    }

    #[test]
    fn 面は端末より明るい順に重なる() {
        // 端末 < クローム < 浮いた面。手前ほど明るくして層を示す。
        let t = Theme::default();
        let lum = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        assert!(lum(t.bg) < lum(t.chrome_bg));
        assert!(lum(t.chrome_bg) < lum(t.surface));
    }

    #[test]
    fn 文字は三段階で暗くなる() {
        let t = Theme::default();
        let lum = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        assert!(lum(t.fg_primary) > lum(t.fg_secondary));
        assert!(lum(t.fg_secondary) > lum(t.fg_tertiary));
    }

    #[test]
    fn 端末の基本色は_dracula_に一致する() {
        let t = Theme::default();
        assert_eq!(t.bg, rgb(0x28, 0x2A, 0x36));
        assert_eq!(t.fg, rgb(0xF8, 0xF8, 0xF2));
        assert_eq!(t.ansi[1], rgb(0xFF, 0x55, 0x55));
        assert_eq!(t.ansi[2], rgb(0x50, 0xFA, 0x7B));
        assert_eq!(t.ansi[3], rgb(0xF1, 0xFA, 0x8C));
    }
}
