//! クリップボード。macOS の pbcopy と pbpaste を呼ぶ。
//!
//! GUI ツールキットの依存を増やさないため、外部コマンドで済ませる。

use std::io::Write;
use std::process::{Command, Stdio};

pub fn copy(text: &str) {
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else {
        log::warn!("pbcopy を起動できない");
        return;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}

pub fn paste() -> Option<String> {
    let out = Command::new("pbpaste").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}
