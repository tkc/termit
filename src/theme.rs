//! 色の解決。端末が持つ 256 色パレットと既定の前景・背景色を定める。

use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    /// ANSI 16 色（通常 8 色に続けて明色 8 色）。
    pub ansi: [Rgb; 16],
    /// 左ペインの背景と枠線。
    pub sidebar_bg: Rgb,
    pub sidebar_fg: Rgb,
    pub sidebar_dim: Rgb,
    pub sidebar_sel: Rgb,
    pub accent: Rgb,
    pub warn: Rgb,
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: rgb(0xd8, 0xd8, 0xd2),
            bg: rgb(0x14, 0x15, 0x18),
            cursor: rgb(0xd8, 0xd8, 0xd2),
            ansi: [
                rgb(0x1c, 0x1d, 0x21), // black
                rgb(0xe0, 0x6c, 0x75), // red
                rgb(0x98, 0xc3, 0x79), // green
                rgb(0xe5, 0xc0, 0x7b), // yellow
                rgb(0x61, 0xaf, 0xef), // blue
                rgb(0xc6, 0x78, 0xdd), // magenta
                rgb(0x56, 0xb6, 0xc2), // cyan
                rgb(0xab, 0xb2, 0xbf), // white
                rgb(0x5c, 0x63, 0x70), // bright black
                rgb(0xef, 0x86, 0x8f), // bright red
                rgb(0xb0, 0xd6, 0x92), // bright green
                rgb(0xf2, 0xd3, 0x94), // bright yellow
                rgb(0x82, 0xc0, 0xff), // bright blue
                rgb(0xd8, 0x96, 0xec), // bright magenta
                rgb(0x74, 0xcd, 0xd8), // bright cyan
                rgb(0xe6, 0xe6, 0xe6), // bright white
            ],
            sidebar_bg: rgb(0x0e, 0x0f, 0x12),
            sidebar_fg: rgb(0xc8, 0xcc, 0xd4),
            sidebar_dim: rgb(0x69, 0x70, 0x7e),
            sidebar_sel: rgb(0x23, 0x2a, 0x36),
            accent: rgb(0x98, 0xc3, 0x79),
            warn: rgb(0xe0, 0x6c, 0x75),
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
                rgb(
                    steps[(i / 36) % 6],
                    steps[(i / 6) % 6],
                    steps[i % 6],
                )
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
        // 16 番は立方体の原点、231 番は白。
        assert_eq!(t.indexed(16), rgb(0, 0, 0));
        assert_eq!(t.indexed(231), rgb(255, 255, 255));
        // 各成分が 6 段階のいずれかになる。
        assert_eq!(t.indexed(21), rgb(0, 0, 255));
    }

    #[test]
    fn 灰色階調を計算する() {
        let t = Theme::default();
        assert_eq!(t.indexed(232), rgb(8, 8, 8));
        assert_eq!(t.indexed(255), rgb(238, 238, 238));
    }
}
