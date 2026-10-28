//! 1 行の中の当たりと、当たりを中心にした詰め方。
//!
//! 左ペインは 200pt ほどの幅しかなく、結果の 1 行には 20 字前後しか載らない。
//! 末尾を落とす `fit_tail` も先頭を落とす `fit_head` も、探した語そのものを
//! 落としかねない。そこで当たりを残して、前後から落とす。
//!
//! 単位に注意する。`find_hit` が返すのは元の文字列の**バイト**位置、
//! `window_around` が受け取り返すのは**文字**の添字である。

use std::ops::Range;

/// 大文字小文字を無視した素の一致。`needle_lower` は呼ぶ側が小文字にして渡す。
///
/// 返す範囲は `text` の**バイト**位置。小文字にすると長さが変わる字（'İ' など）が
/// あるので、小文字側のバイト位置から元のバイト位置へ戻す対応表を作る。
// 呼ぶのは次の仕事（全セッションの走査）。それまで読み手がいない。
#[allow(dead_code)]
pub fn find_hit(text: &str, needle_lower: &str) -> Option<Range<usize>> {
    if needle_lower.is_empty() {
        return None;
    }
    let mut lower = String::with_capacity(text.len());
    // 小文字側の各バイトが、元の文字のどこから（start）どこまで（end）かを持つ。
    let mut start_at: Vec<usize> = Vec::with_capacity(text.len());
    let mut end_at: Vec<usize> = Vec::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();
        for lc in c.to_lowercase() {
            for _ in 0..lc.len_utf8() {
                start_at.push(i);
                end_at.push(end);
            }
            lower.push(lc);
        }
    }
    let at = lower.find(needle_lower)?;
    let from = start_at[at];
    let to = end_at[at + needle_lower.len() - 1];
    Some(from..to)
}

/// 当たりを中心に `keep` 字だけ残す範囲を返す。添字はすべて**文字**。
///
/// 当たりの前後へ均等に伸ばし、片側が尽きたら残りをもう片側へ回す。
/// 当たりが `keep` より長ければ、当たりの頭から `keep` 字を残す。
/// 戻り値の 2 つの真偽は、頭を落としたか・尾を落としたか。
#[allow(dead_code)]
pub fn window_around(chars: &[char], hit: Range<usize>, keep: usize) -> (Range<usize>, bool, bool) {
    let len = chars.len();
    if keep >= len {
        return (0..len, false, false);
    }
    let hit_len = hit.end.saturating_sub(hit.start);
    let before = keep.saturating_sub(hit_len) / 2;
    let start = hit.start.saturating_sub(before).min(len - keep);
    let end = start + keep;
    (start..end, start > 0, end < len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 大文字小文字を無視して探す() {
        assert_eq!(find_hit("この SESマッチ機能", "ses"), Some(7..10));
        assert_eq!(find_hit("ses", "SES"), None); // needle は小文字で渡す約束
        assert_eq!(find_hit("abc", "x"), None);
    }

    #[test]
    fn 小文字にして長さが変わっても位置を戻せる() {
        // 'İ'（U+0130）の小文字は 'i' と結合点の 2 文字で、バイト数が増える。
        // 元の文字列のバイト位置が返ること。
        let t = "aİb";
        assert_eq!(find_hit(t, "b"), Some(t.len() - 1..t.len()));
        // 増えた側の途中にかかる一致でも、元の文字の境で返る。
        assert_eq!(find_hit(t, "i"), Some(1..3));
    }

    #[test]
    fn 当たりを真ん中に残す() {
        let chars: Vec<char> = "0123456789ABCDEFGHIJ".chars().collect();
        // 当たりは 10..12。20 字のうち 8 字だけ残すなら、前後を均等に落とす。
        let (keep, head, tail) = window_around(&chars, 10..12, 8);
        assert!(keep.start <= 10 && keep.end >= 12);
        assert_eq!(keep.end - keep.start, 8);
        assert!(head && tail);
    }

    #[test]
    fn 当たりが端にあるときは片側だけ落とす() {
        let chars: Vec<char> = "0123456789".chars().collect();
        let (keep, head, tail) = window_around(&chars, 0..2, 5);
        assert_eq!(keep, 0..5);
        assert!(!head && tail);
    }

    #[test]
    fn 当たりが残す幅より長ければ当たりの頭から残す() {
        let chars: Vec<char> = "0123456789".chars().collect();
        let (keep, _, _) = window_around(&chars, 2..9, 3);
        assert_eq!(keep, 2..5);
    }
}
