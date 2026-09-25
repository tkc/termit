//! 画面に出す文字列のうち、認証情報らしき値の位置を返す。
//!
//! 伏せるのは**見た目だけ**である。グリッドの中身も、PTY へ流れる値も
//! 本物のままなので、`export AWS_SECRET_ACCESS_KEY=…` を貼れば普通に効く。
//! 肩越しの視線・画面共有・スクリーンショット・遡った画面から消えるだけである。
//!
//! 何を伏せるかは設定（`[screen] redact`）にある。ここにあるのは
//! 「式に当てはめて、`secret` と名付けた部分を置き換える」という手続きだけで、
//! AWS や Google の鍵の形は 1 つも書かれていない。相手の形が変われば設定を直す。
//!
//! Claude Code も同じことをしている。実行ファイルの中の `redactForDisplay` は、
//! 確信度 `high` の規則だけを使う。人が読む画面では、誤検知のほうが害だからである
//! （出ていくデータには広い規則も併せて使う）。termit の既定の式もその `high` 相当に絞る。

use std::ops::Range;

use regex::Regex;

/// 試験で伏せた跡に置く文字列。画面では `MASK_CHAR` を 1 コマずつ置く。
#[cfg(test)]
const REDACTED: &str = "[redacted]";

/// 設定の式をまとめて持つ。起動時に 1 度だけ組み立てる。
#[derive(Debug, Default)]
pub struct Redactor {
    rules: Vec<Regex>,
}

/// 組み立てに失敗した式と、その理由。
#[derive(Debug)]
pub struct BadPattern {
    /// 設定の `redact` の中で何番目か（0 から数える）。
    pub index: usize,
    pub pattern: String,
    pub message: String,
}

impl std::fmt::Display for BadPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "screen.redact[{}] is not a valid regex: {}\n  {}",
            self.index, self.pattern, self.message
        )
    }
}

impl Redactor {
    /// 式を組み立てる。1 つでも壊れていれば、その場所を言って失敗する。
    pub fn new(patterns: &[String]) -> Result<Self, BadPattern> {
        let mut rules = Vec::with_capacity(patterns.len());
        for (index, pattern) in patterns.iter().enumerate() {
            match Regex::new(pattern) {
                Ok(re) => rules.push(re),
                Err(e) => {
                    return Err(BadPattern {
                        index,
                        pattern: pattern.clone(),
                        message: e.to_string(),
                    })
                }
            }
        }
        Ok(Self { rules })
    }

    /// 伏せる式を 1 つも持たないか。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 伏せる範囲を、バイトの位置で返す。重なりは畳んで、前から順に並べる。
    ///
    /// 式に `secret` という名前の組があれば**その部分だけ**を指す。
    /// 名前や引用符は画面に残るので、何が伏せてあるのかは読み取れる。
    /// 組が無ければ、当たった全体を指す。
    pub fn spans(&self, text: &str) -> Vec<Range<usize>> {
        if self.rules.is_empty() || text.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Range<usize>> = Vec::new();
        for re in &self.rules {
            for caps in re.captures_iter(text) {
                let m = match caps.name("secret") {
                    Some(m) => m,
                    None => caps.get(0).expect("当たり全体は必ずある"),
                };
                if m.start() < m.end() {
                    out.push(m.start()..m.end());
                }
            }
        }
        if out.len() > 1 {
            // 式どうしは重なる（`AWS_SECRET…=AKIA…` は語頭の式にも変数名の式にも当たる）。
            // 畳んでおかないと、描く側が同じコマを二度見ることになる。
            out.sort_by_key(|r| (r.start, r.end));
            let mut merged: Vec<Range<usize>> = Vec::with_capacity(out.len());
            for r in out {
                match merged.last_mut() {
                    Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                    _ => merged.push(r),
                }
            }
            return merged;
        }
        out
    }

    /// 伏せた文字列を組み立てる。読みやすさのため、試験でだけ使う。
    #[cfg(test)]
    pub fn redact(&self, text: &str) -> (String, usize) {
        let spans = self.spans(text);
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        for r in &spans {
            out.push_str(&text[at..r.start]);
            out.push_str(REDACTED);
            at = r.end;
        }
        out.push_str(&text[at..]);
        (out, spans.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScreenConfig;

    /// 既定の式で試す。設定の既定値そのものが正しいことを確かめたい。
    fn default_redactor() -> Redactor {
        Redactor::new(&ScreenConfig::default().redact).expect("既定の式は組み立てられる")
    }

    #[test]
    fn 既定の式はすべて組み立てられる() {
        let r = default_redactor();
        assert!(!r.is_empty());
    }

    /// ~/.aws/credentials をそのまま貼った形。
    #[test]
    fn aws_の認証情報ファイルの値を伏せる() {
        let text = "[default]\n\
                    aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n\
                    aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n";
        let (out, n) = default_redactor().redact(text);
        // 名前は残り、値だけが消える。貼った先に形は伝わる。
        assert!(out.contains("aws_secret_access_key = [redacted]"), "{out}");
        assert!(out.contains("aws_access_key_id = [redacted]"), "{out}");
        assert!(!out.contains("wJalrXUtnFEMI"), "{out}");
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "{out}");
        assert_eq!(n, 2);
    }

    /// aws sts assume-role の出力を貼った形。
    #[test]
    fn sts_の_json_の値を伏せる() {
        let text = r#"{"Credentials": {"AccessKeyId": "ASIAIOSFODNN7EXAMPLE",
            "SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "SessionToken": "FwoGZXIvYXdzEHwaDEXAMPLETOKEN123456"}}"#;
        let (out, _) = default_redactor().redact(text);
        assert!(out.contains(r#""SecretAccessKey": "[redacted]""#), "{out}");
        assert!(out.contains(r#""SessionToken": "[redacted]""#), "{out}");
        // キー ID は語頭で分かるので、名前が付いていなくても消える。
        assert!(!out.contains("ASIAIOSFODNN7EXAMPLE"), "{out}");
        assert!(!out.contains("wJalrXUtnFEMI"), "{out}");
        assert!(!out.contains("FwoGZXIvYXdz"), "{out}");
    }

    /// GCP のサービスアカウントの鍵。値の中に改行（\n）が入るので、
    /// 「形が秘密らしいか」で絞ると通り抜ける。閉じ引用符まで取る。
    #[test]
    fn gcp_のサービスアカウントの鍵を伏せる() {
        let text = r#"{"type": "service_account", "project_id": "my-proj",
  "private_key": "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BA\n-----END PRIVATE KEY-----\n",
  "client_email": "svc@my-proj.iam.gserviceaccount.com"}"#;
        let (out, n) = default_redactor().redact(text);
        assert!(out.contains(r#""private_key": "[redacted]""#), "{out}");
        assert!(!out.contains("MIIEvQIBADAN"), "{out}");
        assert!(!out.contains("BEGIN PRIVATE KEY"), "{out}");
        // 秘密でないものは残す。相手に文脈が伝わらないと質問にならない。
        assert!(out.contains(r#""project_id": "my-proj""#), "{out}");
        assert!(out.contains("svc@my-proj.iam.gserviceaccount.com"), "{out}");
        assert_eq!(n, 1);
    }

    #[test]
    fn google_の_api_キーと_oauth_を伏せる() {
        // 実物と同じ長さ（AIza に続けて 35 文字）。
        let text = "curl 'https://x/v1?key=AIzaSyD_abcdefghijklmnopqrstuvwxyz01234' \
                    -H 'Authorization: Bearer ya29.a0AfH6SMBexampletoken_-123'";
        let (out, n) = default_redactor().redact(text);
        assert!(!out.contains("AIzaSyD_abcdefg"), "{out}");
        assert!(!out.contains("ya29.a0AfH6SMB"), "{out}");
        assert_eq!(n, 2);
    }

    /// エージェントに貼るコードを壊さないこと。これが誤爆の本命である。
    #[test]
    fn 秘密でないコードはそのまま貼る() {
        let text = "let api_key = config.get(\"API_KEY\")?;\n\
                    token = os.environ['TOKEN']\n\
                    password = prompt()\n\
                    // AKIA is the prefix of an AWS key id\n\
                    aws_secret_access_key = None\n";
        let (out, n) = default_redactor().redact(text);
        assert_eq!(out, text, "貼った内容が変わってはいけない");
        assert_eq!(n, 0);
    }

    /// `export AWS_SECRET_ACCESS_KEY=…` の形。クラウドの変数をまとめて拾う。
    #[test]
    fn クラウドの環境変数の値を伏せる() {
        let text = "export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
                    export AWS_SESSION_TOKEN=FwoGZXIvYXdzEHwaDEXAMPLETOKEN123456\n\
                    export AZURE_CLIENT_SECRET=Abc123~defGHIjklMNOpqrSTUvwxYZ01234\n";
        let (out, n) = default_redactor().redact(text);
        assert!(out.contains("AWS_SECRET_ACCESS_KEY=[redacted]"), "{out}");
        assert!(out.contains("AWS_SESSION_TOKEN=[redacted]"), "{out}");
        assert!(out.contains("AZURE_CLIENT_SECRET=[redacted]"), "{out}");
        assert_eq!(n, 3);
    }

    /// 秘密でないクラウドの設定は残す。ここが Claude Code の式と変えたところで、
    /// あちらは telemetry 用なので `AWS_REGION` まで伏せる。
    /// エージェントに読ませる経路では、これを消すと質問が成り立たない。
    #[test]
    fn クラウドの設定値は残す() {
        let text = "AWS_REGION=us-east-1\n\
                    AWS_PROFILE=default\n\
                    AWS_DEFAULT_OUTPUT=json\n\
                    GOOGLE_CLOUD_PROJECT=my-project-123456\n";
        let (out, n) = default_redactor().redact(text);
        assert_eq!(out, text, "設定はそのまま届く");
        assert_eq!(n, 0);
    }

    /// 鍵のファイルをそのまま貼った形。塊ごと消す。
    #[test]
    fn pem_の秘密鍵を塊ごと伏せる() {
        let text = "ここに鍵があります\n\
                    -----BEGIN RSA PRIVATE KEY-----\n\
                    MIIEowIBAAKCAQEAxGZ1kQ0pS7vN8mKcZ3rTjWq\n\
                    aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789abc\n\
                    -----END RSA PRIVATE KEY-----\n\
                    これで全部です";
        let (out, n) = default_redactor().redact(text);
        assert!(!out.contains("MIIEowIBAAK"), "{out}");
        assert!(!out.contains("BEGIN RSA PRIVATE KEY"), "{out}");
        // 前後の文は残る。何を聞きたかったのかが伝わらないと意味がない。
        assert!(out.contains("ここに鍵があります"), "{out}");
        assert!(out.contains("これで全部です"), "{out}");
        assert_eq!(n, 1);
    }

    /// 終わりの印が無ければ伏せない。
    /// 「-----BEGIN PRIVATE KEY----- と書くと…」という文章を巻き込まないため。
    #[test]
    fn 終わりの無い_pem_は伏せない() {
        let text = "PEM は -----BEGIN PRIVATE KEY----- という行で始まります";
        let (out, n) = default_redactor().redact(text);
        assert_eq!(out, text);
        assert_eq!(n, 0);
    }

    #[test]
    fn google_の_client_secret_を伏せる() {
        let text = "GOCSPX-abcdefghijklmnopqrstuvwxyz01";
        let (out, n) = default_redactor().redact(text);
        assert_eq!(out, "[redacted]");
        assert_eq!(n, 1);
    }

    /// 式が重なっても、件数は実際に伏せた数のままであること。
    /// `AWS_ACCESS_KEY_ID=AKIA…` は語頭の式と変数名の式の両方に当たる。
    #[test]
    fn 重なった式で件数が増えない() {
        let text = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let (out, n) = default_redactor().redact(text);
        assert_eq!(out, "AWS_ACCESS_KEY_ID=[redacted]");
        assert_eq!(n, 1, "1 つの秘密は 1 件と数える");
    }

    #[test]
    fn 式が無ければ素通りする() {
        let r = Redactor::new(&[]).unwrap();
        assert!(r.is_empty());
        let text = "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let (out, n) = r.redact(text);
        assert_eq!(out, text);
        assert_eq!(n, 0);
    }

    /// `secret` の組が無い式は、当たり全体を伏せる。
    /// 重なった式は畳んでから返すこと。描く側が同じコマを二度見ない。
    #[test]
    fn 重なった範囲を畳む() {
        let r = default_redactor();
        // 語頭の式にも、変数名の式にも当たる行。
        let spans = r.spans("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE");
        assert_eq!(spans.len(), 1, "{spans:?}");
        let s = &spans[0];
        assert_eq!(
            &"AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"[s.clone()],
            "AKIAIOSFODNN7EXAMPLE"
        );
    }

    /// 範囲は前から順に並ぶこと。描く側が走査しながら使える。
    #[test]
    fn 範囲は前から順に並ぶ() {
        let r = default_redactor();
        let text = "a AKIAIOSFODNN7EXAMPLE b AIzaSyD_abcdefghijklmnopqrstuvwxyz01234 c";
        let spans = r.spans(text);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!(spans[0].end <= spans[1].start);
        assert_eq!(&text[spans[0].clone()], "AKIAIOSFODNN7EXAMPLE");
    }

    #[test]
    fn 組の無い式は当たり全体を伏せる() {
        let r = Redactor::new(&[r"hunter2".to_string()]).unwrap();
        let (out, n) = r.redact("pw is hunter2 ok");
        assert_eq!(out, "pw is [redacted] ok");
        assert_eq!(n, 1);
    }

    /// 壊れた式は、何番目かを言って断る。起動時に気づけるようにする。
    #[test]
    fn 壊れた式は場所を言って断る() {
        let e = Redactor::new(&["ok".to_string(), "[unclosed".to_string()]).unwrap_err();
        assert_eq!(e.index, 1);
        assert!(e.to_string().contains("screen.redact[1]"), "{e}");
    }
}
