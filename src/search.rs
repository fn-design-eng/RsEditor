//! 検索・置換の純粋なロジック (UI に依存しない)
//!
//! 位置はすべて「文字単位」(char index) で扱う。egui のカーソルも文字単位なので、
//! バイト位置が必要なときは `char_to_byte` / `to_byte_ranges` で変換する。

/// 大文字・小文字を無視して比較するときの 1 文字の正規化。
///
/// 方針: 小文字化した結果が必ず 1 文字になる場合だけ変換し、それ以外はそのまま残す。
/// 文字数が 1:1 のままなので、文字位置がずれない。
/// 日本語 (かな・漢字) には大文字小文字の区別がないので、そのまま比較される。
fn fold(c: char, case_sensitive: bool) -> char {
    if case_sensitive {
        return c;
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// `text` の中の `query` の出現位置を (開始, 終了) の文字位置で返す。
///
/// 出現は左から順に重ならないように拾う (例: "aaa" の "aa" は 1 件)。
/// 隣接する出現 (例: "aaaa" の "aa" は 2 件) はそれぞれ数える。
/// 空の検索語は何も返さない。
pub fn find_all(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let t: Vec<char> = text.chars().map(|c| fold(c, case_sensitive)).collect();
    let q: Vec<char> = query.chars().map(|c| fold(c, case_sensitive)).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + q.len() <= t.len() {
        if t[i..i + q.len()] == q[..] {
            out.push((i, i + q.len()));
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

/// 文字位置をバイト位置に変換する (文字数を超える場合は text.len())
pub fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

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

/// すべての一致を `replacement` に置き換える。戻り値は (置換後の文字列, 置換した件数)
pub fn replace_all(
    text: &str,
    query: &str,
    replacement: &str,
    case_sensitive: bool,
) -> (String, usize) {
    let matches = find_all(text, query, case_sensitive);
    if matches.is_empty() {
        return (text.to_owned(), 0);
    }
    let byte_ranges = to_byte_ranges(text, &matches);
    let mut out = String::with_capacity(text.len() + matches.len() * replacement.len());
    let mut last = 0;
    for &(bs, be) in &byte_ranges {
        out.push_str(&text[last..bs]);
        out.push_str(replacement);
        last = be;
    }
    out.push_str(&text[last..]);
    (out, matches.len())
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

    #[test]
    fn empty_query_matches_nothing() {
        assert!(find_all("abc", "", true).is_empty());
        assert_eq!(replace_all("abc", "", "x", true), ("abc".to_string(), 0));
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
        assert_eq!(find_all("aaaa", "aa", true), vec![(0, 2), (2, 4)]);
        assert_eq!(find_all("aaa", "aa", true), vec![(0, 2)]);
        assert_eq!(find_all("ああああ", "ああ", true), vec![(0, 2), (2, 4)]);
        assert!(find_all("ab", "abc", true).is_empty());
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
    fn replace_all_works() {
        assert_eq!(replace_all("abab", "ab", "x", true), ("xx".to_string(), 2));
        assert_eq!(
            replace_all("東京都東京", "東京", "大阪", true),
            ("大阪都大阪".to_string(), 2)
        );
        assert_eq!(
            replace_all("ABC abc", "abc", "X", false),
            ("X X".to_string(), 2)
        );
        // 置換後の文字列に検索語が含まれても繰り返さない
        assert_eq!(replace_all("a", "a", "aa", true), ("aa".to_string(), 1));
        assert_eq!(replace_all("abc", "x", "y", true), ("abc".to_string(), 0));
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
    }
}
