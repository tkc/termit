//! 入力の往復にかかる時間を測る。
//!
//! キーボードを使わずに測れるよう、PTY へ 1 文字書いてから、
//! それが端末のグリッドに現れるまでを計る。
//! 経路は「書き込みスレッド → PTY → 行規律の echo → 読み取りスレッド →
//! VT パーサ → グリッド」で、利用者が打鍵してから文字が出るまでの
//! 端末側の取り分にあたる。

use std::path::Path;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};

use crate::pty;
use crate::term::{TermSize, UiSender};

const SAMPLES: usize = 80;
const WARMUP: usize = 20;

pub fn run() {
    let (tx, rx) = channel();
    let size = TermSize::new(80, 24);
    let spawned = match pty::spawn(
        pty::SpawnOptions {
            id: 1,
            argv: &["/bin/cat".to_string()],
            cwd: Path::new("/"),
            size,
            cell: (8, 16),
            scrollback: 1000,
            session_key: "latency-test".to_string(),
        },
        UiSender::Channel(tx),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PTY を起動できない: {e}");
            return;
        }
    };
    // 起動を待つ。
    std::thread::sleep(Duration::from_millis(300));

    let mut samples = Vec::with_capacity(SAMPLES);
    let letters: Vec<u8> = (b'a'..=b'z').collect();
    for i in 0..SAMPLES {
        let ch = letters[i % letters.len()];
        // 直前の内容を消してから測る。消去が反映されるまで少し待つ。
        spawned.handle.write(b"\x1b[2J\x1b[H".to_vec());
        std::thread::sleep(Duration::from_millis(15));
        // 溜まっている通知を捨て、次の更新で通知が飛ぶようにする。
        while rx.try_recv().is_ok() {}
        spawned
            .dirty
            .store(false, std::sync::atomic::Ordering::Release);
        let start = Instant::now();
        spawned.handle.write(vec![ch]);
        let deadline = start + Duration::from_millis(200);
        let mut hit = false;
        while !hit && Instant::now() < deadline {
            // 画面が変わった通知を待ってからグリッドを見る。
            // 総当たりで待つと、読み取りスレッドと錠を奪い合って進まなくなる。
            if rx.recv_timeout(Duration::from_millis(5)).is_err() {
                continue;
            }
            spawned
                .dirty
                .store(false, std::sync::atomic::Ordering::Release);
            let t = spawned.term.lock();
            let grid = t.grid();
            'outer: for l in 0..grid.screen_lines() {
                for c in 0..grid.columns() {
                    if grid[Line(l as i32)][Column(c)].c == ch as char {
                        hit = true;
                        break 'outer;
                    }
                }
            }
        }
        let d = start.elapsed();
        if hit && i >= WARMUP {
            samples.push(d);
        }
    }

    if samples.is_empty() {
        println!("反映を観測できなかった");
        let mut handle = spawned.handle;
        handle.kill();
        return;
    }
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: usize| ms[(ms.len() - 1) * p / 100];
    println!("入力の往復（書き込み → PTY の echo → グリッドに反映）");
    println!("  標本 {} 件", ms.len());
    println!(
        "  中央 {:.3}ms  p90 {:.3}ms  p99 {:.3}ms  最大 {:.3}ms",
        pct(50),
        pct(90),
        pct(99),
        pct(100)
    );
    println!();
    println!("打鍵から画面に出るまでは、これに次が加わる。");
    println!("  端末の描画（読み取り → present）: 別途 0.5〜1.3ms（実測）");
    println!("  present から実際の表示まで: 表示装置の 1 周期（60Hz なら最大 16.7ms）");

    let mut handle = spawned.handle;
    handle.kill();
}
