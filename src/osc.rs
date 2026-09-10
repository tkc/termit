//! PTY の生バイト列から、端末が利用する OSC シーケンスだけを取り出す。
//!
//! `alacritty_terminal` は OSC 7 と OSC 133、OSC 1337 を扱わないため、
//! VT パーサへ流すのと同じバイト列をここでも走査する。
//! シーケンスは `read` の境界で分断されるので、状態は呼び出しをまたいで保持する。

use base64::Engine;

/// 取り出した通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 7: 作業ディレクトリの変更。
    Cwd(String),
    /// OSC 133;A: プロンプトの開始。
    PromptStart,
    /// OSC 133;B: コマンド入力の開始。
    CommandStart,
    /// OSC 133;C: コマンド実行の開始。
    CommandExecuted,
    /// OSC 133;D: コマンドの終了。終了コードは省略されることがある。
    CommandFinished(Option<i32>),
    /// OSC 1337 SetUserVar=termit_agent_id: エージェント ID の通知。
    AgentId(String),
}

/// ペイロードの上限。これを超えたシーケンスは捨てる。
const MAX_PAYLOAD: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Payload,
    PayloadEscape,
}

/// OSC の抽出器。ペインごとに 1 個持つ。
#[derive(Debug)]
pub struct OscScanner {
    state: State,
    payload: Vec<u8>,
    overflowed: bool,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            payload: Vec::new(),
            overflowed: false,
        }
    }

    /// バイト列を走査し、完結した OSC シーケンスから通知を作る。
    ///
    /// 返す添字は、そのシーケンスの終端の直後を指す。呼び出し側は
    /// そこまでを VT パーサへ流してから通知を処理することで、
    /// OSC が届いた時点の画面状態を読める。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(usize, OscEvent)> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            // 普通の文字のあいだは、次の ESC まで一気に飛ばす。
            // 出力のほとんどは普通の文字なので、1 バイトずつ見ると無駄が多い。
            if self.state == State::Ground {
                match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(off) => {
                        self.state = State::Escape;
                        i += off + 1;
                        continue;
                    }
                    None => break,
                }
            }
            let b = bytes[i];
            match self.state {
                // 上で飛ばしているので、ここへは来ない。
                // それでも元の判定を残す。取りこぼすより安全である。
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => match b {
                    b']' => {
                        self.state = State::Payload;
                        self.payload.clear();
                        self.overflowed = false;
                    }
                    0x1b => {}
                    _ => self.state = State::Ground,
                },
                State::Payload => match b {
                    0x07 => {
                        self.finish(i + 1, &mut out);
                    }
                    0x1b => self.state = State::PayloadEscape,
                    _ => {
                        if self.payload.len() < MAX_PAYLOAD {
                            self.payload.push(b);
                        } else {
                            self.overflowed = true;
                        }
                    }
                },
                State::PayloadEscape => {
                    // ST は ESC \ である。それ以外が続いた場合も打ち切る。
                    self.finish(i + 1, &mut out);
                    if b == 0x1b {
                        self.state = State::Escape;
                    }
                }
            }
            i += 1;
        }
        out
    }

    fn finish(&mut self, end: usize, out: &mut Vec<(usize, OscEvent)>) {
        self.state = State::Ground;
        if !self.overflowed {
            if let Some(ev) = parse_payload(&self.payload) {
                out.push((end, ev));
            }
        }
        self.payload.clear();
        self.overflowed = false;
    }
}

fn parse_payload(payload: &[u8]) -> Option<OscEvent> {
    let text = std::str::from_utf8(payload).ok()?;
    let (num, rest) = match text.split_once(';') {
        Some((n, r)) => (n, r),
        None => (text, ""),
    };
    match num {
        "7" => parse_osc7(rest),
        "133" => parse_osc133(rest),
        "1337" => parse_osc1337(rest),
        _ => None,
    }
}

/// `file://host/path` 形式から絶対パスを取り出す。
fn parse_osc7(rest: &str) -> Option<OscEvent> {
    let after_scheme = rest.strip_prefix("file://")?;
    let slash = after_scheme.find('/')?;
    let path = percent_decode(&after_scheme[slash..]);
    Some(OscEvent::Cwd(path))
}

fn parse_osc133(rest: &str) -> Option<OscEvent> {
    let mut parts = rest.split(';');
    let kind = parts.next()?;
    match kind {
        "A" => Some(OscEvent::PromptStart),
        "B" => Some(OscEvent::CommandStart),
        "C" => Some(OscEvent::CommandExecuted),
        "D" => {
            // `D` 単独と `D;<code>` の両方がある。
            let code = parts.next().and_then(|c| c.parse::<i32>().ok());
            Some(OscEvent::CommandFinished(code))
        }
        _ => None,
    }
}

fn parse_osc1337(rest: &str) -> Option<OscEvent> {
    let var = rest.strip_prefix("SetUserVar=")?;
    let (name, value) = var.split_once('=')?;
    if name != "termit_agent_id" {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .ok()?;
    let id = String::from_utf8(decoded).ok()?;
    if id.is_empty() {
        return None;
    }
    Some(OscEvent::AgentId(id))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// zsh 用のシェル統合。これを読み込むと、端末がコマンド履歴を記録できる。
///
/// `precmd` で直前の終了コードと作業ディレクトリを知らせ、続けてプロンプトの
/// 開始を知らせる。入力の開始位置はプロンプト文字列の末尾に置く。
/// `%{ %}` は幅を持たない区間を表すので、桁の計算は狂わない。
pub const ZSH_INTEGRATION: &str = r#"# termit shell integration (zsh)
if [[ -n "$TERMIT_SHELL_INTEGRATION" && -z "$__TERMIT_LOADED" ]]; then
  __TERMIT_LOADED=1
  __termit_precmd() {
    local st=$?
    printf '\033]133;D;%s\007' "$st"
    printf '\033]7;file://%s%s\007' "${HOST:-localhost}" "$PWD"
    printf '\033]133;A\007'
  }
  __termit_preexec() { printf '\033]133;C\007' }
  typeset -ga precmd_functions preexec_functions
  precmd_functions+=(__termit_precmd)
  preexec_functions+=(__termit_preexec)
  PS1="$PS1"$'%{\e]133;B\a%}'
fi
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(input: &[u8]) -> Vec<OscEvent> {
        OscScanner::new()
            .feed(input)
            .into_iter()
            .map(|(_, e)| e)
            .collect()
    }

    fn scan_with_offsets(input: &[u8]) -> Vec<(usize, OscEvent)> {
        OscScanner::new().feed(input)
    }

    #[test]
    fn bel_で終わる_osc133_を読む() {
        assert_eq!(scan(b"\x1b]133;A\x07"), vec![OscEvent::PromptStart]);
        assert_eq!(scan(b"\x1b]133;B\x07"), vec![OscEvent::CommandStart]);
        assert_eq!(scan(b"\x1b]133;C\x07"), vec![OscEvent::CommandExecuted]);
        assert_eq!(
            scan(b"\x1b]133;D;0\x07"),
            vec![OscEvent::CommandFinished(Some(0))]
        );
        assert_eq!(
            scan(b"\x1b]133;D;130\x07"),
            vec![OscEvent::CommandFinished(Some(130))]
        );
    }

    #[test]
    fn st_で終わる_osc_を読む() {
        assert_eq!(scan(b"\x1b]133;A\x1b\\"), vec![OscEvent::PromptStart]);
    }

    #[test]
    fn 終了コードのない_d_を読む() {
        assert_eq!(
            scan(b"\x1b]133;D\x07"),
            vec![OscEvent::CommandFinished(None)]
        );
    }

    #[test]
    fn 読み取り境界をまたいだシーケンスを組み立てる() {
        let mut s = OscScanner::new();
        assert!(s.feed(b"\x1b]13").is_empty());
        assert!(s.feed(b"3;D;").is_empty());
        assert_eq!(
            s.feed(b"42\x07"),
            vec![(3, OscEvent::CommandFinished(Some(42)))]
        );
    }

    #[test]
    fn 通常の文字列は何も生まない() {
        assert!(scan(b"hello world\n$ ls -la\n").is_empty());
    }

    #[test]
    fn 他のエスケープを誤って解釈しない() {
        // SGR、カーソル移動、DECSET はいずれも OSC ではない。
        assert!(scan(b"\x1b[31mred\x1b[0m\x1b[2J\x1b[?25l").is_empty());
    }

    #[test]
    fn osc7_からパスを取り出す() {
        assert_eq!(
            scan(b"\x1b]7;file://host/Users/tkc/repo\x07"),
            vec![OscEvent::Cwd("/Users/tkc/repo".into())]
        );
    }

    #[test]
    fn osc7_のパーセント符号を戻す() {
        assert_eq!(
            scan(b"\x1b]7;file://host/Users/tkc/my%20repo\x07"),
            vec![OscEvent::Cwd("/Users/tkc/my repo".into())]
        );
    }

    #[test]
    fn setuservar_からエージェント_id_を取り出す() {
        // base64("0198f5a2-1111-7000-8000-abcdefabcdef")
        let id = "0198f5a2-1111-7000-8000-abcdefabcdef";
        let b64 = base64::engine::general_purpose::STANDARD.encode(id);
        let seq = format!("\x1b]1337;SetUserVar=termit_agent_id={b64}\x07");
        assert_eq!(scan(seq.as_bytes()), vec![OscEvent::AgentId(id.into())]);
    }

    #[test]
    fn 他の_setuservar_は無視する() {
        let b64 = base64::engine::general_purpose::STANDARD.encode("x");
        let seq = format!("\x1b]1337;SetUserVar=other={b64}\x07");
        assert!(scan(seq.as_bytes()).is_empty());
    }

    #[test]
    fn 過大なペイロードを捨てる() {
        let mut s = OscScanner::new();
        let mut seq = b"\x1b]133;D;".to_vec();
        seq.extend(std::iter::repeat_n(b'9', MAX_PAYLOAD + 10));
        seq.push(0x07);
        assert!(s.feed(&seq).is_empty());
        // 打ち切った後も次のシーケンスを読める。
        assert_eq!(s.feed(b"\x1b]133;A\x07"), vec![(8, OscEvent::PromptStart)]);
    }

    #[test]
    fn 終端の直後の位置を返す() {
        // ESC ] 1 3 3 ; C BEL で 8 バイト。その後ろに続く出力は次の区間になる。
        let got = scan_with_offsets(b"\x1b]133;C\x07ls\r\n\x1b]133;D;0\x07");
        assert_eq!(got[0].0, 8);
        assert_eq!(got[0].1, OscEvent::CommandExecuted);
        // 8 + "ls\r\n" の 4 + "\x1b]133;D;0\x07" の 10。
        assert_eq!(got[1].0, 22);
        assert_eq!(got[1].1, OscEvent::CommandFinished(Some(0)));
    }

    #[test]
    fn 複数のシーケンスを順に取り出す() {
        assert_eq!(
            scan(b"\x1b]133;C\x07ls -la\r\n\x1b]133;D;0\x07\x1b]133;A\x07"),
            vec![
                OscEvent::CommandExecuted,
                OscEvent::CommandFinished(Some(0)),
                OscEvent::PromptStart
            ]
        );
    }
    /// ESC まで飛ばす近道が、位置を狂わせないことを確かめる。
    #[test]
    fn 長い平文のあとのシーケンスも位置が合う() {
        let mut input = vec![b'a'; 10_000];
        input.extend_from_slice(b"\x1b]133;C\x07");
        let got = scan_with_offsets(&input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 10_008);
        assert_eq!(got[0].1, OscEvent::CommandExecuted);
    }

    /// OSC でないエスケープの直後に OSC が来ても取りこぼさない。
    #[test]
    fn 別のエスケープに続く_osc_を拾う() {
        // ESC [ 3 1 m のあとに OSC が続く。
        assert_eq!(
            scan(b"\x1b[31mred\x1b]133;A\x07"),
            vec![OscEvent::PromptStart]
        );
    }
}
