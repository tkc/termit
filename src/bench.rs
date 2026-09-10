//! 描画の内訳を測る。ウィンドウを開かずに、実際の描画関数を通す。
//!
//! 遅さの原因を「描画の指示を組み立てる費用（CPU）」と
//! 「GPU へ投げて待つ費用」に切り分けるために使う。

use std::time::Duration;

use crate::render::Renderer;
use crate::theme::Theme;

const FRAMES: usize = 20;
const WARMUP: usize = 4;

pub fn run() {
    let theme = Theme::default();
    let sizes = [(1280u32, 800u32), (1920, 1200)];
    for (w, h) in sizes {
        let mut r = pollster::block_on(Renderer::offscreen(w, h, "Menlo", 13.0));
        let (cols, rows) = r.grid_size();
        println!(
            "\n=== {w}x{h}  {cols} 桁 x {rows} 行 = {} セル ===",
            cols * rows
        );
        for (label, fill) in [("全面", 1.0f32), ("半分", 0.5), ("1 割", 0.1)] {
            let mut build = Vec::new();
            let mut prep = Vec::new();
            let mut submit = Vec::new();
            let mut cells = 0usize;
            for frame in 0..FRAMES {
                r.begin();
                cells = 0;
                let t0 = std::time::Instant::now();
                let limit = (cols as f32 * fill) as usize;
                for row in 0..rows {
                    for col in 0..limit {
                        // 毎フレーム内容を変え、字形の使い回しだけで速く見えないようにする。
                        let c = char::from_u32(0x21 + ((col + row + frame) % 90) as u32).unwrap();
                        let fg = theme.ansi[(col + row) % 16];
                        if (col + row) % 7 == 0 {
                            r.fill_cells(col, row, 1, 1, theme.surface);
                        }
                        r.put_char(col, row, c, fg, (col + row) % 5 == 0, false);
                        cells += 1;
                    }
                }
                let b = t0.elapsed();
                let p = r.bench_prepare();
                let s = r.bench_submit(theme.bg);
                if frame >= WARMUP {
                    build.push(b);
                    prep.push(p);
                    submit.push(s);
                }
            }
            let (bm, b90) = stats(&build);
            let (pm, p90) = stats(&prep);
            let (sm, s90) = stats(&submit);
            println!(
                "  {label:<6} セル {cells:>6}  組み立て 中央 {bm:6.2}ms p90 {b90:6.2}ms | prepare 中央 {pm:6.2}ms p90 {p90:6.2}ms | submit 中央 {sm:6.2}ms p90 {s90:6.2}ms | 合計 中央 {:6.2}ms",
                bm + pm + sm
            );
        }
    }
    println!("\n60fps の予算は 16.7ms。入力から表示までの体感は、これに vsync 待ちが加わる。");
}

fn stats(v: &[Duration]) -> (f64, f64) {
    let mut v: Vec<f64> = v.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[v.len() * 9 / 10])
}
