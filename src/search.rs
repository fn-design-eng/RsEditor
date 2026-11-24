//! 検索・置換の純粋なロジック (UI に依存しない)
//!
//! 検索の内部ではバイト位置を使い (`find_bytes`)、egui のカーソル用に文字位置 (char index)
//! へ変換して使う (`find_all` / `bytes_to_chars`)。
//! どちらの検索も文書の長さに比例する時間で終わる (パターンが長くても O(n*m) にならない)。

use std::collections::VecDeque;

/// 置換後の文字列の大きさの上限 (バイト)。これを超える置換は実行しない
pub const MAX_REPLACE_RESULT: usize = 512 * 1024 * 1024;

/// 大文字・小文字を無視して比較するときの正規化 (畳み込み) の仕様。
///
/// - 1 文字ずつ `char::to_lowercase` で小文字にする。結果が複数文字になるもの
///   (例: `İ` → `i̇` の 2 文字) は、その複数文字をそのまま並べる。
/// - 語末のシグマ `ς` は `σ` に揃える (`Σ` は小文字化で `σ` になるため、`Σ` / `σ` / `ς`
///   がすべて同じ文字として一致する)。
/// - 一致は元の文字の境界に揃っているものだけを採用する。1 文字が複数文字に畳まれる場合に、
///   その途中から始まる・途中で終わる一致 (例: `İ` の中の `i`) は一致とみなさない。
/// - 日本語 (かな・漢字) には大文字小文字の区別がないので、そのまま比較される。
fn fold_char(c: char, mut emit: impl FnMut(char, bool)) {
    let mut it = c.to_lowercase().peekable();
    while let Some(l) = it.next() {
        let l = if l == 'ς' { 'σ' } else { l };
        emit(l, it.peek().is_none());
    }
}

/// KMP の失敗関数
fn kmp_table(q: &[char]) -> Vec<usize> {
    let mut fail = vec![0; q.len()];
    let mut k = 0;
    for i in 1..q.len() {
        while k > 0 && q[i] != q[k] {
            k = fail[k - 1];
        }
        if q[i] == q[k] {
            k += 1;
        }
        fail[i] = k;
    }
    fail
}

/// 畳み込んだ 1 文字と、その元の文字の位置
struct Folded {
    src_start: usize,
    src_end: usize,
    /// 元の文字の最初の畳み込み文字か
    first: bool,
    /// 元の文字の最後の畳み込み文字か
    last: bool,
}

/// 大文字・小文字を区別しない検索 (KMP, 線形時間)。
///
/// 文書全体の畳み込み済み配列は作らず、検索語の長さ分だけの履歴を持ちながら
/// 先頭から 1 文字ずつ流し込む (大きな文書でもメモリは増えない)。
fn find_bytes_ci(text: &str, query: &str) -> Vec<(usize, usize)> {
    let mut q: Vec<char> = Vec::new();
    for c in query.chars() {
        fold_char(c, |l, _| q.push(l));
    }
    if q.is_empty() {
        return Vec::new();
    }
    let fail = kmp_table(&q);
    let mut window: VecDeque<Folded> = VecDeque::with_capacity(q.len().min(1 << 16) + 1);
    let mut out = Vec::new();
    let mut last_end = 0;
    let mut k = 0;
    for (start, c) in text.char_indices() {
        let end = start + c.len_utf8();
        let mut first = true;
        fold_char(c, |l, last| {
            if window.len() == q.len() {
                window.pop_front();
            }
            window.push_back(Folded {
                src_start: start,
                src_end: end,
                first,
                last,
            });
            first = false;
            while k > 0 && q[k] != l {
                k = fail[k - 1];
            }
            if q[k] == l {
                k += 1;
            }
            if k == q.len() {
                // 出現した (重なる出現も KMP ならすべて見つかる)。左から順に重ならないものを採用する
                let head = &window[0];
                let tail = &window[q.len() - 1];
                if head.first && tail.last && head.src_start >= last_end {
                    out.push((head.src_start, tail.src_end));
                    last_end = tail.src_end;
                }
                k = fail[k - 1];
            }
        });
    }
    out
}

/// `text` の中の `query` の出現位置を (開始, 終了) のバイト位置で返す。
///
/// 出現は左から順に重ならないように拾う (例: "aaa" の "aa" は 1 件)。
/// 隣接する出現 (例: "aaaa" の "aa" は 2 件) はそれぞれ数える。
/// 空の検索語は何も返さない。
pub fn find_bytes(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    if case_sensitive {
        // std の str 検索は Two-Way アルゴリズム (線形時間)。重ならない出現を左から返す
        text.match_indices(query)
            .map(|(i, m)| (i, i + m.len()))
            .collect()
    } else {
        find_bytes_ci(text, query)
    }
}

/// バイト範囲の列 (昇順・重ならない) を文字位置の範囲に変換する。1 回の走査で済む
pub fn bytes_to_chars(text: &str, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(ranges.len());
    let (mut chars, mut byte) = (0usize, 0usize);
    for &(s, e) in ranges {
        chars += text[byte..s].chars().count();
        let from = chars;
        chars += text[s..e].chars().count();
        out.push((from, chars));
        byte = e;
    }
    out
}

/// `text` の中の `query` の出現位置を (開始, 終了) の文字位置で返す (規則は `find_bytes` と同じ)
pub fn find_all(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    bytes_to_chars(text, &find_bytes(text, query, case_sensitive))
}

/// 文字位置をバイト位置に変換する (文字数を超える場合は text.len())
pub fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

#[cfg(test)]
/// 文字単位の範囲列をバイト単位に変換する。範囲外の要素は取り除く
pub fn to_byte_ranges(text: &str, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let mut offs: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    offs.push(text.len());
    ranges
        .iter()
        .filter_map(|&(s, e)| Some((*offs.get(s)?, *offs.get(e)?)))
        .collect()
}

/// 置換できなかった理由
#[derive(Debug, PartialEq, Eq)]
pub enum ReplaceError {
    /// 置換後の大きさが上限を超える
    TooLarge,
    /// メモリを確保できない
    OutOfMemory,
}

/// `ranges` (バイト範囲, 昇順) を `replacement` に置き換えたときの文字列の大きさ (バイト)。
/// 桁あふれする場合は `None`。何も確保しない
pub fn replaced_len(text_len: usize, ranges: &[(usize, usize)], replacement: &str) -> Option<usize> {
    let removed: usize = ranges.iter().map(|&(s, e)| e - s).sum();
    let added = ranges.len().checked_mul(replacement.len())?;
    (text_len - removed).checked_add(added)
}

/// `ranges` (バイト範囲, 昇順) を `replacement` に置き換えた文字列を作る。
///
/// 先に結果の大きさを桁あふれに注意して計算し、`limit` を超えるときや、確保に失敗するときは
/// 何も作らずにエラーを返す (確保の失敗でプロセスごと落ちて、他のタブの編集を失わないため)。
pub fn replace_ranges(
    text: &str,
    ranges: &[(usize, usize)],
    replacement: &str,
    limit: usize,
) -> Result<String, ReplaceError> {
    let new_len = replaced_len(text.len(), ranges, replacement).ok_or(ReplaceError::TooLarge)?;
    if new_len > limit {
        return Err(ReplaceError::TooLarge);
    }
    let mut out = String::new();
    out.try_reserve_exact(new_len)
        .map_err(|_| ReplaceError::OutOfMemory)?;
    let mut last = 0;
    for &(bs, be) in ranges {
        out.push_str(&text[last..bs]);
        out.push_str(replacement);
        last = be;
    }
    out.push_str(&text[last..]);
    Ok(out)
}

/// すべての一致を `replacement` に置き換える。戻り値は (置換後の文字列, 置換した件数)
#[cfg(test)]
pub fn replace_all(
    text: &str,
    query: &str,
    replacement: &str,
    case_sensitive: bool,
) -> Result<(String, usize), ReplaceError> {
    let ranges = find_bytes(text, query, case_sensitive);
    let out = replace_ranges(text, &ranges, replacement, MAX_REPLACE_RESULT)?;
    Ok((out, ranges.len()))
}

/// `from` (文字位置) より後ろで最初の一致の index。無ければ先頭に折り返す
pub fn next_index(matches: &[(usize, usize)], from: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().position(|&(s, _)| s >= from).unwrap_or(0))
}

/// `from` より前で最後の一致の index。無ければ末尾に折り返す
pub fn prev_index(matches: &[(usize, usize)], from: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(
        matches
            .iter()
            .rposition(|&(_, e)| e <= from)
            .unwrap_or(matches.len() - 1),
    )
}

/// 現在の選択 (sel_start..sel_end) に対応する一致の index。
/// 選択がちょうど一致と同じならその一致、そうでなければ選択の後ろの次の一致
/// (「次を検索」で移動する先) を返す
pub fn current_index(
    matches: &[(usize, usize)],
    sel_start: usize,
    sel_end: usize,
) -> Option<usize> {
    matches
        .iter()
        .position(|&m| m == (sel_start, sel_end))
        .or_else(|| next_index(matches, sel_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaced(text: &str, q: &str, r: &str, cs: bool) -> (String, usize) {
        replace_all(text, q, r, cs).unwrap()
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(find_all("abc", "", true).is_empty());
        assert!(find_all("abc", "", false).is_empty());
        assert_eq!(replaced("abc", "", "x", true), ("abc".to_string(), 0));
    }

    #[test]
    fn char_positions_for_ascii_and_japanese() {
        assert_eq!(find_all("abcabc", "bc", true), vec![(1, 3), (4, 6)]);
        // 「日本語テキスト」は 7 文字なので、2 件目は文字位置 7 から
        assert_eq!(
            find_all("日本語テキスト日本語", "日本語", true),
            vec![(0, 3), (7, 10)]
        );
        // 改行を挟んでも文字位置で数える
        assert_eq!(find_all("あいう\nいう", "いう", true), vec![(1, 3), (4, 6)]);
    }

    #[test]
    fn adjacent_and_overlapping_matches() {
        for cs in [true, false] {
            assert_eq!(find_all("aaaa", "aa", cs), vec![(0, 2), (2, 4)]);
            assert_eq!(find_all("aaa", "aa", cs), vec![(0, 2)]);
            assert_eq!(find_all("ああああ", "ああ", cs), vec![(0, 2), (2, 4)]);
            assert!(find_all("ab", "abc", cs).is_empty());
            // 自己重複するパターン
            assert_eq!(find_all("abababab", "abab", cs), vec![(0, 4), (4, 8)]);
            assert_eq!(find_all("ababa", "aba", cs), vec![(0, 3)]);
        }
    }

    #[test]
    fn case_insensitive_matching() {
        assert_eq!(
            find_all("Hello HELLO hello", "hello", false),
            vec![(0, 5), (6, 11), (12, 17)]
        );
        assert_eq!(find_all("Hello HELLO hello", "hello", true), vec![(12, 17)]);
        // 日本語と英字が混在しても文字位置がずれない
        assert_eq!(
            find_all("日本語ABC日本語abc", "abc", false),
            vec![(3, 6), (9, 12)]
        );
    }

    #[test]
    fn sigma_variants_match_each_other() {
        // Σ (大文字) / σ / ς (語末) はすべて同じ文字として一致する
        assert_eq!(find_all("ΟΔΥΣΣΕΥΣ", "οδυσσευς", false), vec![(0, 8)]);
        assert_eq!(find_all("οδυσσευς", "ΟΔΥΣΣΕΥΣ", false), vec![(0, 8)]);
        assert_eq!(find_all("ς σ Σ", "σ", false), vec![(0, 1), (2, 3), (4, 5)]);
        // 大文字小文字を区別するときは別の文字
        assert_eq!(find_all("ς σ Σ", "σ", true), vec![(2, 3)]);
    }

    #[test]
    fn multi_char_lowercase_keeps_original_positions() {
        // İ (U+0130) は小文字化で "i̇" (2 文字) になる。元の文字単位の位置を返す
        assert_eq!(find_all("aİb", "i\u{307}", false), vec![(1, 2)]);
        assert_eq!(find_all("aİb", "aİb", false), vec![(0, 3)]);
        // 元の文字の途中から始まる一致は採用しない
        assert!(find_all("aİb", "i", false).is_empty());
        assert!(find_all("aİb", "\u{307}b", false).is_empty());
        // バイト位置も元の文字の境界
        assert_eq!(find_bytes("aİb", "b", false), vec![(3, 4)]);
    }

    #[test]
    fn replace_all_works() {
        assert_eq!(replaced("abab", "ab", "x", true), ("xx".to_string(), 2));
        assert_eq!(
            replaced("東京都東京", "東京", "大阪", true),
            ("大阪都大阪".to_string(), 2)
        );
        assert_eq!(replaced("ABC abc", "abc", "X", false), ("X X".to_string(), 2));
        // 置換後の文字列に検索語が含まれても繰り返さない
        assert_eq!(replaced("a", "a", "aa", true), ("aa".to_string(), 1));
        assert_eq!(replaced("abc", "x", "y", true), ("abc".to_string(), 0));
    }

    #[test]
    fn replace_refuses_oversized_result_before_allocating() {
        let text = "a".repeat(1000);
        let r = find_bytes(&text, "a", true);
        // 1000 件 x 1000 バイト = 約 1MB。上限 100KB なら拒否する
        let big = "x".repeat(1000);
        assert_eq!(
            replace_ranges(&text, &r, &big, 100_000),
            Err(ReplaceError::TooLarge)
        );
        assert_eq!(replaced_len(text.len(), &r, &big), Some(1_000_000));
        assert!(replace_ranges(&text, &r, &big, 2_000_000).is_ok());
        let many = vec![(0usize, 0usize); 4];
        assert_eq!(replaced_len(0, &many, "xy"), Some(8));
    }

    #[test]
    fn linear_time_on_pathological_patterns() {
        // "aaa…ab" の形の検索語は、素朴な検索だと O(n*m) になる
        let text = "a".repeat(300_000);
        let query = format!("{}b", "a".repeat(3_000));
        for cs in [true, false] {
            assert!(find_bytes(&text, &query, cs).is_empty());
        }
        let r = find_bytes(&text, &"a".repeat(3_000), false);
        assert_eq!(r.len(), 100);
    }

    #[test]
    fn navigation_wraps_around() {
        let m = [(1, 3), (4, 6), (8, 10)];
        assert_eq!(next_index(&m, 0), Some(0));
        assert_eq!(next_index(&m, 3), Some(1));
        assert_eq!(next_index(&m, 10), Some(0));
        assert_eq!(prev_index(&m, 8), Some(1));
        assert_eq!(prev_index(&m, 1), Some(2));
        assert_eq!(next_index(&[], 0), None);
        assert_eq!(prev_index(&[], 0), None);
    }

    #[test]
    fn current_index_prefers_exact_selection() {
        let m = [(1, 3), (4, 6), (8, 10)];
        assert_eq!(current_index(&m, 4, 6), Some(1));
        assert_eq!(current_index(&m, 6, 6), Some(2));
    }

    #[test]
    fn char_to_byte_and_ranges() {
        assert_eq!(char_to_byte("あいう", 1), 3);
        assert_eq!(char_to_byte("あいう", 3), 9);
        assert_eq!(to_byte_ranges("あいう", &[(1, 2)]), vec![(3, 6)]);
        assert_eq!(
            to_byte_ranges("abc", &[(5, 6)]),
            Vec::<(usize, usize)>::new()
        );
        assert_eq!(bytes_to_chars("あいうえ", &[(3, 6), (9, 12)]), vec![(1, 2), (3, 4)]);
    }
}
