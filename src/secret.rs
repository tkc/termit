//! 貼り付ける文字列から、認証情報らしき値を伏せる。
//!
//! 何を伏せるかは設定（`[paste] redact`）にある。ここにあるのは
//! 「式に当てはめて、`secret` と名付けた部分を置き換える」という手続きだけで、
//! AWS や Google の鍵の形は 1 つも書かれていない。相手の形が変われば設定を直す。
//!
//! iTerm2 も同じ考え方で、貼り付けに正規表現の置換を持たせている
//! （`iTermPasteHelper.m` の `sanitizePasteEvent:`）。違いは、あちらが
//! 道具だけを配るのに対し、termit は既定の式を持つことである。
//! 道具だけ配っても、書く人がいなければ誰も守られない。

use regex::{Captures, Regex};

/// 伏せた跡に置く文字列。
///
/// 読む側（エージェント）に「消されている」と分かる形にする。
/// 伏せ字だけだと、その文字が値そのものだと解釈されることがある。
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
            "paste.redact[{}] is not a valid regex: {}\n  {}",
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

    /// 伏せた文字列と、伏せた件数を返す。
    ///
    /// 式に `secret` という名前の組があれば、**その部分だけ**を置き換える。
    /// 名前や引用符は残るので、貼り付けた先には形が伝わる。
    /// 組が無ければ、当たった全体を置き換える。
    pub fn redact(&self, text: &str) -> (String, usize) {
        let mut out = text.to_string();
        let mut hits = 0usize;
        for re in &self.rules {
            out = re
                .replace_all(&out, |caps: &Captures| {
                    let whole = caps.get(0).expect("当たり全体は必ずある");
                    let (from, to) = match caps.name("secret") {
                        Some(m) => (m.start() - whole.start(), m.end() - whole.start()),
                        None => (0, whole.len()),
                    };
                    let s = whole.as_str();
                    // 既に伏せてあるものを数え直さない。式は重なることがあり
                    // （`AWS_SECRET…=AKIA…` は語頭の式と名前の式の両方に当たる）、
                    // そのたびに数えると「2 件伏せた」と嘘の件数が出る。
                    if &s[from..to] == REDACTED {
                        return s.to_string();
                    }
                    hits += 1;
                    format!("{}{REDACTED}{}", &s[..from], &s[to..])
                })
                .into_owned();
        }
        (out, hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PasteConfig;

    /// 既定の式で試す。設定の既定値そのものが正しいことを確かめたい。
    fn default_redactor() -> Redactor {
        Redactor::new(&PasteConfig::default().redact).expect("既定の式は組み立てられる")
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

    /// 二度通しても結果が変わらず、二度目は 0 件であること。
    #[test]
    fn 二度伏せても変わらない() {
        let r = default_redactor();
        let (once, n1) =
            r.redact("aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        let (twice, n2) = r.redact(&once);
        assert_eq!(once, twice);
        assert_eq!(n1, 1);
        assert_eq!(n2, 0);
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
        assert!(e.to_string().contains("paste.redact[1]"), "{e}");
    }
}
