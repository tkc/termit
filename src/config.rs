//! 設定の読み込みと、起動コマンドの組み立て。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const HOST_PROFILE: &str = "host";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub profile: BTreeMap<String, Profile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            shell: ShellConfig::default(),
            agent: AgentConfig::default(),
            profile: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowConfig {
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default = "default_scrollback")]
    pub scrollback: usize,
    #[serde(default = "default_sidebar_cols")]
    pub sidebar_cols: usize,
    /// 表示装置の走査に合わせるか。
    ///
    /// 合わせると画面の裂けは起きないが、投入したフレームが出るまで
    /// 最大で 1 周期（60Hz なら 16.7ms）待つ。切ると待ちが消える代わりに
    /// 書き換えの途中が見えることがある。
    #[serde(default = "default_vsync")]
    pub vsync: bool,
}

fn default_vsync() -> bool {
    true
}

fn default_font() -> String {
    // SF Mono はアプリケーションバンドルの中にあり、フォント名で引けない。
    "Menlo".to_string()
}
fn default_font_size() -> f32 {
    13.0
}
fn default_scrollback() -> usize {
    10_000
}
fn default_sidebar_cols() -> usize {
    28
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            font: default_font(),
            font_size: default_font_size(),
            scrollback: default_scrollback(),
            sidebar_cols: default_sidebar_cols(),
            vsync: default_vsync(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellConfig {
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            program: None,
            args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// 新規セッションの起動コマンド。未設定ならシェルを起動する。
    pub new: Option<String>,
    /// 分岐セッションの起動コマンド。未設定なら親と同じコマンドを再実行する。
    pub fork: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// 使用するイメージ。省略するとホスト上で直接起動する。
    pub image: Option<String>,
    #[serde(default = "default_workdir")]
    pub workdir: String,
    #[serde(default)]
    pub mount: Vec<String>,
    #[serde(default = "default_network")]
    pub network: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_workdir() -> String {
    "/work".to_string()
}
fn default_network() -> String {
    // モデル API への接続が切れるとエージェントが動かないため bridge を既定とする。
    "bridge".to_string()
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            image: None,
            workdir: default_workdir(),
            mount: Vec::new(),
            network: default_network(),
            env: Vec::new(),
            args: Vec::new(),
        }
    }
}

impl Profile {
    pub fn is_host(&self) -> bool {
        self.image.is_none()
    }
}

// ---------------------------------------------------------------- 読み込み

#[derive(Debug)]
pub enum ConfigError {
    Read(PathBuf, std::io::Error),
    Parse(PathBuf, toml::de::Error),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read(p, e) => write!(f, "{} を読めない: {e}", p.display()),
            ConfigError::Parse(p, e) => write!(f, "{} の書式が不正:\n{e}", p.display()),
            ConfigError::Invalid(m) => write!(f, "設定が不正: {m}"),
        }
    }
}

/// 設定の置き場所。
///
/// macOS の `dirs::config_dir()` は `~/Library/Application Support` を返すが、
/// 端末の利用者が設定を探すのは `~/.config` である。XDG の作法に合わせる。
pub fn config_path() -> Option<PathBuf> {
    Some(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("tex").join("config.toml"))
}

/// `$XDG_*_HOME` があればそれを、なければ home 直下の既定を返す。
pub fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    if let Ok(v) = std::env::var(var) {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    Some(dirs::home_dir()?.join(fallback))
}

impl Config {
    /// 既定の位置から読む。ファイルがなければ既定値を返す。
    pub fn load() -> Result<Config, ConfigError> {
        let Some(path) = config_path() else {
            return Ok(Config::default());
        };
        if !path.exists() {
            return Ok(Config::default());
        }
        Config::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(path.to_path_buf(), e))?;
        let config: Config =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))?;
        config.validate()?;
        Ok(config)
    }

    /// 不正な値は既定値で補わず、その場で失敗させる。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.window.font_size < 4.0 || self.window.font_size > 200.0 {
            return Err(ConfigError::Invalid(format!(
                "window.font_size は 4.0 から 200.0 のあいだ（現在 {}）",
                self.window.font_size
            )));
        }
        if self.window.sidebar_cols < 10 || self.window.sidebar_cols > 120 {
            return Err(ConfigError::Invalid(format!(
                "window.sidebar_cols は 10 から 120 のあいだ（現在 {}）",
                self.window.sidebar_cols
            )));
        }
        if self.profile.contains_key(HOST_PROFILE) {
            let p = &self.profile[HOST_PROFILE];
            if p.image.is_some() {
                return Err(ConfigError::Invalid(
                    "profile.host に image は書けない。ホスト実行を表す予約名である".into(),
                ));
            }
        }
        for (name, p) in &self.profile {
            if p.image.is_some() && p.mount.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "profile.{name} に mount がない。コンテナから作業ディレクトリが見えない"
                )));
            }
            for m in &p.mount {
                if !m.contains(':') {
                    return Err(ConfigError::Invalid(format!(
                        "profile.{name}.mount の \"{m}\" は <ホスト>:<コンテナ> の形ではない"
                    )));
                }
            }
            for e in &p.env {
                if e.contains('=') {
                    return Err(ConfigError::Invalid(format!(
                        "profile.{name}.env の \"{e}\" には値ではなく変数名だけを書く"
                    )));
                }
            }
        }
        for (field, tmpl) in [("agent.new", &self.agent.new), ("agent.fork", &self.agent.fork)] {
            if let Some(t) = tmpl {
                if let Err(e) = split_template(t) {
                    return Err(ConfigError::Invalid(format!("{field}: {e}")));
                }
                for name in template_vars(t) {
                    if !KNOWN_VARS.contains(&name.as_str()) {
                        return Err(ConfigError::Invalid(format!(
                            "{field}: 未知の変数 {{{name}}}。使えるのは {}",
                            KNOWN_VARS.join(", ")
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Profile {
        self.profile.get(name).cloned().unwrap_or_default()
    }

    pub fn profile_names(&self) -> Vec<String> {
        let mut names = vec![HOST_PROFILE.to_string()];
        names.extend(
            self.profile
                .keys()
                .filter(|k| k.as_str() != HOST_PROFILE)
                .cloned(),
        );
        names
    }
}

// ------------------------------------------------------- コマンドの組み立て

pub const KNOWN_VARS: &[&str] = &["new_id", "parent_agent_id", "cwd", "parent_title"];

/// テンプレート展開に渡す値。`None` の変数を使うテンプレートは展開できない。
#[derive(Debug, Clone, Default)]
pub struct Vars {
    pub new_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub cwd: Option<String>,
    pub parent_title: Option<String>,
}

impl Vars {
    fn get(&self, name: &str) -> Option<&str> {
        match name {
            "new_id" => self.new_id.as_deref(),
            "parent_agent_id" => self.parent_agent_id.as_deref(),
            "cwd" => self.cwd.as_deref(),
            "parent_title" => self.parent_title.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExpandError {
    /// 変数の値がない。呼び出し側は既定の起動へ落とす。
    MissingValue(String),
    /// 変数名が未知である。
    UnknownVar(String),
    /// 引用符が閉じていない。
    UnclosedQuote,
    /// 展開結果が空になった。
    Empty,
}

impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpandError::MissingValue(v) => write!(f, "{{{v}}} の値がない"),
            ExpandError::UnknownVar(v) => write!(f, "未知の変数 {{{v}}}"),
            ExpandError::UnclosedQuote => write!(f, "引用符が閉じていない"),
            ExpandError::Empty => write!(f, "コマンドが空である"),
        }
    }
}

/// テンプレートを引数列へ展開する。
///
/// 変数の値に空白が含まれても引数は分割されないよう、
/// 先にトークンへ分けてから各トークンの中で置換する。
pub fn expand_template(template: &str, vars: &Vars) -> Result<Vec<String>, ExpandError> {
    let tokens = split_template(template)?;
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        out.push(substitute(&token, vars)?);
    }
    if out.is_empty() || out[0].is_empty() {
        return Err(ExpandError::Empty);
    }
    Ok(out)
}

/// 空白で区切りつつ、引用符の中の空白は保つ。
fn split_template(s: &str) -> Result<Vec<String>, ExpandError> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started {
                        tokens.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err(ExpandError::UnclosedQuote);
    }
    if started {
        tokens.push(cur);
    }
    Ok(tokens)
}

fn substitute(token: &str, vars: &Vars) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push('{');
            rest = after;
            continue;
        };
        let name = &after[..close];
        if !KNOWN_VARS.contains(&name) {
            return Err(ExpandError::UnknownVar(name.to_string()));
        }
        let value = vars
            .get(name)
            .ok_or_else(|| ExpandError::MissingValue(name.to_string()))?;
        out.push_str(value);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// テンプレートに現れる変数名を列挙する。
pub fn template_vars(s: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                names.push(after[..close].to_string());
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    names
}

/// プロファイルに従い、実際に起動する引数列を作る。
///
/// ホストのプロファイルでは `command` をそのまま返し、
/// イメージを持つプロファイルでは `docker run` で包む。
pub fn build_argv(profile: &Profile, cwd: &Path, command: &[String]) -> Vec<String> {
    if profile.is_host() {
        let mut argv = command.to_vec();
        argv.extend(profile.args.iter().cloned());
        return argv;
    }
    let cwd_str = cwd.to_string_lossy().to_string();
    let mut argv = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
    ];
    for m in &profile.mount {
        argv.push("-v".to_string());
        argv.push(m.replace("{cwd}", &cwd_str));
    }
    argv.push("-w".to_string());
    argv.push(profile.workdir.clone());
    argv.push("--network".to_string());
    argv.push(profile.network.clone());
    for e in &profile.env {
        argv.push("-e".to_string());
        argv.push(e.clone());
    }
    argv.push(profile.image.clone().unwrap_or_default());
    argv.extend(command.iter().cloned());
    argv.extend(profile.args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars {
            new_id: Some("11111111-2222-3333-4444-555555555555".into()),
            parent_agent_id: Some("aaaa-bbbb".into()),
            cwd: Some("/Users/tkc/my repo".into()),
            parent_title: Some("main".into()),
        }
    }

    #[test]
    fn 変数を置き換える() {
        let got = expand_template("claude --session-id {new_id}", &vars()).unwrap();
        assert_eq!(
            got,
            vec!["claude", "--session-id", "11111111-2222-3333-4444-555555555555"]
        );
    }

    #[test]
    fn 値に空白があっても引数を分割しない() {
        let got = expand_template("ls {cwd}", &vars()).unwrap();
        assert_eq!(got, vec!["ls", "/Users/tkc/my repo"]);
    }

    #[test]
    fn 引用符の中の空白を保つ() {
        let got = expand_template("sh -c 'echo hello world'", &vars()).unwrap();
        assert_eq!(got, vec!["sh", "-c", "echo hello world"]);
    }

    #[test]
    fn 値のない変数は_missingvalue_になる() {
        let v = Vars {
            parent_agent_id: None,
            ..vars()
        };
        let err = expand_template("claude --resume {parent_agent_id}", &v).unwrap_err();
        assert_eq!(err, ExpandError::MissingValue("parent_agent_id".into()));
    }

    #[test]
    fn 未知の変数を拒む() {
        let err = expand_template("claude {model}", &vars()).unwrap_err();
        assert_eq!(err, ExpandError::UnknownVar("model".into()));
    }

    #[test]
    fn 閉じていない引用符を拒む() {
        let err = expand_template("sh -c 'echo", &vars()).unwrap_err();
        assert_eq!(err, ExpandError::UnclosedQuote);
    }

    #[test]
    fn 空のテンプレートを拒む() {
        assert_eq!(expand_template("   ", &vars()).unwrap_err(), ExpandError::Empty);
    }

    #[test]
    fn ホストのプロファイルはコマンドをそのまま返す() {
        let p = Profile::default();
        let argv = build_argv(&p, Path::new("/repo"), &["claude".into()]);
        assert_eq!(argv, vec!["claude"]);
    }

    #[test]
    fn コンテナのプロファイルを_docker_run_で包む() {
        let p = Profile {
            image: Some("tex-agent:latest".into()),
            workdir: "/work".into(),
            mount: vec!["{cwd}:/work".into()],
            network: "bridge".into(),
            env: vec!["ANTHROPIC_API_KEY".into()],
            args: vec!["--dangerously-skip-permissions".into()],
        };
        let argv = build_argv(&p, Path::new("/Users/tkc/repo"), &["claude".into()]);
        assert_eq!(
            argv,
            vec![
                "docker",
                "run",
                "--rm",
                "-it",
                "-v",
                "/Users/tkc/repo:/work",
                "-w",
                "/work",
                "--network",
                "bridge",
                "-e",
                "ANTHROPIC_API_KEY",
                "tex-agent:latest",
                "claude",
                "--dangerously-skip-permissions",
            ]
        );
    }

    #[test]
    fn マウント指定に含まれない変数はそのまま残す() {
        let p = Profile {
            image: Some("img".into()),
            mount: vec!["/etc/ssl:/etc/ssl:ro".into()],
            ..Profile::default()
        };
        let argv = build_argv(&p, Path::new("/repo"), &["sh".into()]);
        assert!(argv.contains(&"/etc/ssl:/etc/ssl:ro".to_string()));
    }

    #[test]
    fn マウントのないコンテナプロファイルを拒む() {
        let toml = r#"
[profile.bad]
image = "img"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn env_に値を書いた設定を拒む() {
        let toml = r#"
[profile.p]
image = "img"
mount = ["{cwd}:/work"]
env = ["KEY=value"]
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn 未知の変数を含むテンプレートを拒む() {
        let toml = r#"
[agent]
new = "claude --model {model}"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn 既定の設定は妥当である() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn 仕様書に載せた設定例を読める() {
        let toml = r#"
[window]
font        = "Menlo"
font_size   = 13.0
scrollback  = 10000

[shell]
program = "/bin/zsh"
args    = ["-l"]

[agent]
new  = "claude --session-id {new_id}"
fork = "claude --resume {parent_agent_id} --fork-session --session-id {new_id}"

[profile.host]

[profile.sandbox]
image   = "tex-agent:latest"
workdir = "/work"
mount   = ["{cwd}:/work"]
network = "bridge"
env     = ["ANTHROPIC_API_KEY"]
args    = ["--dangerously-skip-permissions"]
"#;
        let c: Config = toml::from_str(toml).unwrap();
        c.validate().unwrap();
        assert_eq!(c.profile_names(), vec!["host", "sandbox"]);
        assert!(c.profile("host").is_host());
        assert!(!c.profile("sandbox").is_host());
    }
}
