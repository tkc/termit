//! 子プロセスの作業ディレクトリを OS に尋ねる。
//!
//! `cd` を追うのに OSC 7 だけに頼ると、シェル統合を入れていない利用者では
//! いつまでも起動時の位置のままになる。macOS の `/etc/zshrc` は
//! `/etc/zshrc_$TERM_PROGRAM` を読む作りで、`Apple_Terminal` のときにだけ
//! OSC 7 を出す仕組みが入る。他の端末では誰も出さない。
//!
//! そこで、OS に直接尋ねる経路を持つ。設定がなくても `cd` に追従できる。

use std::path::PathBuf;

/// その pid の作業ディレクトリ。読めなければ `None`。
#[cfg(target_os = "macos")]
pub fn of_pid(pid: i32) -> Option<PathBuf> {
    use std::ffi::CStr;

    if pid <= 0 {
        return None;
    }
    // SAFETY: 出力用の構造体をゼロで用意し、その大きさを渡す。
    // proc_pidinfo は書き込んだ量を返し、足りなければ 0 以下を返す。
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if written < size {
        return None;
    }
    // vip_path は [[c_char; 32]; 32] として宣言されている。実体は
    // MAXPATHLEN の 1 本の緩衝であり、終端の 0 まで読む。
    let raw = info.pvi_cdir.vip_path;
    let bytes: &[libc::c_char] =
        unsafe { std::slice::from_raw_parts(raw.as_ptr().cast(), std::mem::size_of_val(&raw)) };
    if bytes.first().copied().unwrap_or(0) == 0 {
        return None;
    }
    // SAFETY: 上で先頭が 0 でないことを見ており、緩衝は 0 終端である。
    let cstr = unsafe { CStr::from_ptr(bytes.as_ptr()) };
    let path = PathBuf::from(cstr.to_str().ok()?);
    path.is_absolute().then_some(path)
}

#[cfg(not(target_os = "macos"))]
pub fn of_pid(_pid: i32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 自分自身の作業ディレクトリを読める() {
        let want = std::env::current_dir().unwrap();
        let got = of_pid(std::process::id() as i32).expect("自分の cwd は読める");
        // /tmp と /private/tmp のように、経路が正規化されることがある。
        assert_eq!(
            std::fs::canonicalize(&got).unwrap(),
            std::fs::canonicalize(&want).unwrap()
        );
    }

    #[test]
    fn 居ない_pid_は_none_になる() {
        assert_eq!(of_pid(0), None);
        assert_eq!(of_pid(-1), None);
        // 使われていないはずの大きな pid。
        assert_eq!(of_pid(999_999), None);
    }

    #[test]
    fn 子プロセスの移動を追える() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("/bin/sh")
            .args(["-c", "cd /usr/lib && sleep 5"])
            .stdout(Stdio::null())
            .spawn()
            .expect("子を起動できる");
        // cd が済むまで少し待つ。
        let pid = child.id() as i32;
        let mut got = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Some(p) = of_pid(pid) {
                if p != std::env::current_dir().unwrap() {
                    got = Some(p);
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(got, Some(PathBuf::from("/usr/lib")));
    }
}
