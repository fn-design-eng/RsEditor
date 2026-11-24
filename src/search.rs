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

/// 大文字・小文字を区別しない検索 (KMP, 線形時間)。一致を 1 件見つけるたびに `f` を呼ぶ
/// (`f` が false を返したらそこで止める)。
///
/// 文書全体の畳み込み済み配列は作らず、検索語の長さ分だけの履歴を持ちながら
/// 先頭から 1 文字ずつ流し込む (大きな文書でもメモリは増えない)。
fn for_each_match_ci(text: &str, query: &str, mut f: impl FnMut(usize, usize) -> bool) {
    let mut q: Vec<char> = Vec::new();
    for c in query.chars() {
        fold_char(c, |l, _| q.push(l));
    }
    if q.is_empty() {
        return;
    }
    let fail = kmp_table(&q);
    let mut window: VecDeque<Folded> = VecDeque::with_capacity(q.len().min(1 << 16) + 1);
    let mut last_end = 0;
    let mut k = 0;
    let mut stop = false;
    for (start, c) in text.char_indices() {
        let end = start + c.len_utf8();
        let mut first = true;
        fold_char(c, |l, last| {
            if stop {
                return;
            }
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
                    last_end = tail.src_end;
                    if !f(head.src_start, tail.src_end) {
                        stop = true;
                    }
                }
                k = fail[k - 1];
            }
        });
        if stop {
            return;
        }
    }
}

/// `text` の中の `query` の出現を、左から順に (開始, 終了) のバイト位置で `f` に渡す
/// (`f` が false を返したらそこで止める。何も確保しない)。
///
/// 出現は左から順に重ならないように拾う (例: "aaa" の "aa" は 1 件)。
/// 隣接する出現 (例: "aaaa" の "aa" は 2 件) はそれぞれ数える。
/// 空の検索語は何も返さない。
pub fn for_each_match(
    text: &str,
    query: &str,
    case_sensitive: bool,
    mut f: impl FnMut(usize, usize) -> bool,
) {
    if query.is_empty() {
        return;
    }
    if case_sensitive {
        // std の str 検索は Two-Way アルゴリズム (線形時間)。重ならない出現を左から返す
        for (i, m) in text.match_indices(query) {
            if !f(i, i + m.len()) {
                return;
            }
        }
    } else {
        for_each_match_ci(text, query, f);
    }
}

/// 一致を覚えておく件数の上限。短い検索語を大きな文書で探したときに、一致の一覧と
/// 強調の表示がメモリを使い尽くさないようにする (それ以上は強調・一覧に含めない)
pub const MAX_STORED_MATCHES: usize = 100_000;

/// 先頭から最大 `limit` 件の一致をバイト位置で返す。戻り値の bool は、`limit` を超える
/// 一致がまだ残っているか (残りは数えずに打ち切る)
pub fn find_bytes_limited(
    text: &str,
    query: &str,
    case_sensitive: bool,
    limit: usize,
) -> (Vec<(usize, usize)>, bool) {
    let mut out = Vec::new();
    let mut capped = false;
    for_each_match(text, query, case_sensitive, |s, e| {
        if out.len() >= limit {
            capped = true;
            return false;
        }
        out.push((s, e));
        true
    });
    (out, capped)
}

/// `text` の中の `query` の出現位置を (開始, 終了) のバイト位置ですべて返す (テスト用)
#[cfg(test)]
pub fn find_bytes(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    find_bytes_limited(text, query, case_sensitive, usize::MAX).0
}

/// 一致の件数と、一致した部分の合計バイト数。範囲を保存しないので、メモリは増えない
pub fn count_matches(text: &str, query: &str, case_sensitive: bool) -> (usize, usize) {
    let (mut n, mut bytes) = (0usize, 0usize);
    for_each_match(text, query, case_sensitive, |s, e| {
        n += 1;
        bytes += e - s;
        true
    });
    (n, bytes)
}

/// バイト位置 `from` 以降で最初の一致 (バイト範囲)。無ければ先頭に折り返す。
/// 一致の取り方は一覧 (`find_bytes_limited`) と同じで、文書の長さに比例する時間で終わる
pub fn find_next(
    text: &str,
    query: &str,
    case_sensitive: bool,
    from: usize,
) -> Option<(usize, usize)> {
    let (mut first, mut found) = (None, None);
    for_each_match(text, query, case_sensitive, |s, e| {
        first.get_or_insert((s, e));
        if s >= from {
            found = Some((s, e));
            return false;
        }
        true
    });
    found.or(first)
}

/// バイト位置 `from` より前 (終わりが `from` 以下) で最後の一致。無ければ末尾に折り返す
pub fn find_prev(
    text: &str,
    query: &str,
    case_sensitive: bool,
    from: usize,
) -> Option<(usize, usize)> {
    let (mut before, mut last) = (None, None);
    for_each_match(text, query, case_sensitive, |s, e| {
        last = Some((s, e));
        if e <= from {
            before = Some((s, e));
            true
        } else {
            // 以降は `from` より後ろ。手前に一致があれば確定。無ければ末尾まで進んで折り返す
            before.is_none()
        }
    });
    before.or(last)
}

/// バイト範囲 `bs..be` が、それ自体で 1 つの一致になっているか
/// (選択がちょうど一致箇所かの確認用。一覧に載っていない一致にも使える)
pub fn is_exact_match(text: &str, bs: usize, be: usize, query: &str, case_sensitive: bool) -> bool {
    let Some(slice) = text.get(bs..be) else {
        return false;
    };
    if slice.is_empty() {
        return false;
    }
    let mut hit = None;
    for_each_match(slice, query, case_sensitive, |s, e| {
        hit = Some((s, e));
        false
    });
    hit == Some((0, slice.len()))
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

/// `text` の中の `query` の出現位置を (開始, 終了) の文字位置で返す (規則は `find_bytes` と同じ。テスト用)
#[cfg(test)]
pub fn find_all(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    bytes_to_chars(text, &find_bytes(text, query, case_sensitive))
}

/// バイト位置を文字位置に変換する
pub fn byte_to_char(text: &str, byte: usize) -> usize {
    text[..byte].chars().count()
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

/// すべての一致を `replacement` に置き換えた文字列を、ストリーミングで作る。
/// 戻り値は (置換後の文字列, 置換した件数)。一致が 0 件のときは、何も作らず空の文字列を返す。
///
/// 一致の範囲は保存しない。1 回目の走査で件数と合計バイト数だけを数え、置換後の大きさを
/// 先に (桁あふれに注意して) 計算して `limit` を超えるときや確保に失敗するときは断る。
/// 2 回目の走査で、結果を端から組み立てる。
pub fn replace_all_streaming(
    text: &str,
    query: &str,
    replacement: &str,
    case_sensitive: bool,
    limit: usize,
) -> Result<(String, usize), ReplaceError> {
    let (n, removed) = count_matches(text, query, case_sensitive);
    if n == 0 {
        return Ok((String::new(), 0));
    }
    let new_len = n
        .checked_mul(replacement.len())
        .and_then(|added| (text.len() - removed).checked_add(added))
        .ok_or(ReplaceError::TooLarge)?;
    if new_len > limit {
        return Err(ReplaceError::TooLarge);
    }
    let mut out = String::new();
    out.try_reserve_exact(new_len)
        .map_err(|_| ReplaceError::OutOfMemory)?;
    let mut last = 0;
    for_each_match(text, query, case_sensitive, |s, e| {
        out.push_str(&text[last..s]);
        out.push_str(replacement);
        last = e;
        true
    });
    out.push_str(&text[last..]);
    Ok((out, n))
}

/// すべての一致を `replacement` に置き換える。戻り値は (置換後の文字列, 置換した件数)
#[cfg(test)]
pub fn replace_all(
    text: &str,
    query: &str,
    replacement: &str,
    case_sensitive: bool,
) -> Result<(String, usize), ReplaceError> {
    let (out, n) =
        replace_all_streaming(text, query, replacement, case_sensitive, MAX_REPLACE_RESULT)?;
    Ok((if n == 0 { text.to_owned() } else { out }, n))
}

/// `from` (文字位置) 以降で最初の一致の index。無ければ先頭に折り返す。
/// 一覧は昇順なので二分探索する
pub fn next_index(matches: &[(usize, usize)], from: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let i = matches.partition_point(|&(s, _)| s < from);
    Some(if i == matches.len() { 0 } else { i })
}

/// `from` より前 (終わりが `from` 以下) で最後の一致の index。無ければ末尾に折り返す
#[cfg(test)]
pub fn prev_index(matches: &[(usize, usize)], from: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let i = matches.partition_point(|&(_, e)| e <= from);
    Some(if i == 0 { matches.len() - 1 } else { i - 1 })
}

/// 選択 (sel_start..sel_end) がちょうど一致と同じなら、その一致の index (二分探索)
pub fn exact_index(matches: &[(usize, usize)], sel_start: usize, sel_end: usize) -> Option<usize> {
    let i = matches.partition_point(|&(s, _)| s < sel_start);
    (matches.get(i) == Some(&(sel_start, sel_end))).then_some(i)
}

#[cfg(test)]
/// 現在の選択 (sel_start..sel_end) に対応する一致の index。
/// 選択がちょうど一致と同じならその一致、そうでなければ選択の後ろの次の一致
/// (「次を検索」で移動する先) を返す
pub fn current_index(
    matches: &[(usize, usize)],
    sel_start: usize,
    sel_end: usize,
) -> Option<usize> {
    exact_index(matches, sel_start, sel_end).or_else(|| next_index(matches, sel_end))
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

    /// 一覧の上限: 短い検索語を大きな文書で探しても、保存する一致は上限まで
    #[test]
    fn stored_matches_are_capped() {
        let text = "a".repeat(1_000_000);
        let (r, capped) = find_bytes_limited(&text, "a", true, MAX_STORED_MATCHES);
        assert_eq!(r.len(), MAX_STORED_MATCHES);
        assert!(capped);
        assert_eq!(r[0], (0, 1));
        let (r, capped) = find_bytes_limited(&text, "a", false, 10);
        assert_eq!((r.len(), capped), (10, true));
        // ちょうど上限なら打ち切りではない
        let (r, capped) = find_bytes_limited("aaa", "a", true, 3);
        assert_eq!((r.len(), capped), (3, false));
        // 件数の数え上げは範囲を保存せず、上限なしで数える
        assert_eq!(count_matches(&text, "a", true), (1_000_000, 1_000_000));
        assert_eq!(count_matches("あいあ", "あ", false), (2, 6));
    }

    /// 位置から探す: 一覧と同じ一致を、位置の前後・折り返しも含めて返す
    #[test]
    fn find_next_prev_match_the_list() {
        for cs in [true, false] {
            let text = "ab Ab aB xx AB
ab";
            let all = find_bytes(text, "ab", cs);
            for from in 0..=text.len() {
                let next = all.iter().find(|&&(s, _)| s >= from).or(all.first()).copied();
                let prev = all.iter().rev().find(|&&(_, e)| e <= from).or(all.last()).copied();
                assert_eq!(find_next(text, "ab", cs, from), next, "cs={cs} from={from}");
                assert_eq!(find_prev(text, "ab", cs, from), prev, "cs={cs} from={from}");
            }
        }
        assert_eq!(find_next("abc", "x", true, 0), None);
        assert_eq!(find_prev("abc", "x", true, 3), None);
        assert_eq!(find_next("abc", "", true, 0), None);
        // 重なるパターンでも一覧 (左から重ならない) と同じ
        assert_eq!(find_next("aaa", "aa", true, 1), Some((0, 2)));
        // 大文字小文字を畳み込む検索 (複数文字に畳まれる İ) でも元の文字の境界
        assert_eq!(find_next("aİb", "b", false, 0), Some((3, 4)));
    }

    /// 大きな文書でも、一覧を作らずに次・前の一致へ進める
    #[test]
    fn find_next_in_huge_text_needs_no_list() {
        let mut text = "a".repeat(2_000_000);
        text.push_str("needle");
        assert_eq!(find_next(&text, "needle", true, 5), Some((2_000_000, 2_000_006)));
        assert_eq!(find_prev(&text, "needle", false, 10), Some((2_000_000, 2_000_006)));
        assert_eq!(find_next(&text, "a", true, 1_999_999), Some((1_999_999, 2_000_000)));
    }

    #[test]
    fn streaming_replace_matches_list_based() {
        for cs in [true, false] {
            for (t, q, r) in [
                ("abAB ab", "ab", "X"),
                ("aaaa", "aa", "b"),
                ("東京都東京", "東京", "大阪府"),
                ("aİb", "b", "ccc"),
                ("none", "zz", "y"),
            ] {
                let (got, n) = replace_all_streaming(t, q, r, cs, MAX_REPLACE_RESULT).unwrap();
                let ranges = find_bytes(t, q, cs);
                assert_eq!(n, ranges.len());
                if n > 0 {
                    assert_eq!(got, replace_ranges(t, &ranges, r, MAX_REPLACE_RESULT).unwrap());
                }
            }
        }
        // 上限は、結果を作る前に (数えただけで) 判定する
        let text = "a".repeat(1000);
        assert_eq!(
            replace_all_streaming(&text, "a", &"x".repeat(1000), true, 100_000),
            Err(ReplaceError::TooLarge)
        );
        assert_eq!(
            replace_all_streaming("abc", "", "x", true, 10),
            Ok((String::new(), 0))
        );
    }

    #[test]
    fn exact_match_check() {
        assert!(is_exact_match("xx abc yy", 3, 6, "abc", true));
        assert!(is_exact_match("xx ABC yy", 3, 6, "abc", false));
        assert!(!is_exact_match("xx ABC yy", 3, 6, "abc", true));
        assert!(!is_exact_match("xx abc yy", 3, 5, "abc", true));
        assert!(!is_exact_match("xx abc yy", 2, 6, "abc", true));
        assert!(!is_exact_match("あい", 1, 2, "あ", true), "文字の途中は範囲外");
        assert!(!is_exact_match("abc", 1, 1, "", true));
    }

    /// 二分探索の index は、線形探索と同じ結果になる
    #[test]
    fn index_lookups_match_linear_scan() {
        let m: Vec<(usize, usize)> = (0..20).map(|i| (i * 3 + 1, i * 3 + 3)).collect();
        for from in 0..70 {
            let next = m.iter().position(|&(s, _)| s >= from).unwrap_or(0);
            assert_eq!(next_index(&m, from), Some(next));
            let prev = m.iter().rposition(|&(_, e)| e <= from).unwrap_or(m.len() - 1);
            assert_eq!(prev_index(&m, from), Some(prev));
        }
        for &sel in &m {
            assert_eq!(exact_index(&m, sel.0, sel.1), m.iter().position(|&x| x == sel));
        }
        assert_eq!(exact_index(&m, 2, 3), None);
        assert_eq!(exact_index(&[], 0, 0), None);
        assert_eq!(byte_to_char("あいう", 6), 2);
    }
}
