//! IME の文節情報 (どの文節を変換中か) の取得。
//!
//! egui の `ImeEvent::Preedit` は文字列しか持たず、egui-winit 0.29 は winit の文節範囲も
//! 捨てる。そのため Windows では IMM32 を直接呼び、変換中の合成文字列の属性
//! (`GCS_COMPATTR`) と文節境界 (`GCS_COMPCLAUSE`) を取得する。
//! 取得できない (非 Windows / 失敗 / 文字列の不一致) ときは空を返し、呼び出し側は
//! 未確定文字列全体を 1 つの範囲として表示する。

use std::ops::Range;

/// 文節の状態
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 未変換の入力 (Space を押す前)
    Input,
    /// いま変換の対象になっている文節
    Target,
    /// 変換済みで、対象ではない文節
    Converted,
}

/// 未確定文字列内の 1 文節 (文字位置の範囲)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub range: Range<usize>,
    pub kind: Kind,
}

// IMM32 の ATTR_* 値
const ATTR_INPUT: u8 = 0;
const ATTR_TARGET_CONVERTED: u8 = 1;
const ATTR_CONVERTED: u8 = 2;
const ATTR_TARGET_NOTCONVERTED: u8 = 3;
const ATTR_INPUT_ERROR: u8 = 4;
const ATTR_FIXEDCONVERTED: u8 = 5;

fn attr_kind(a: u8) -> Kind {
    match a {
        ATTR_TARGET_CONVERTED | ATTR_TARGET_NOTCONVERTED => Kind::Target,
        ATTR_CONVERTED | ATTR_FIXEDCONVERTED => Kind::Converted,
        ATTR_INPUT | ATTR_INPUT_ERROR => Kind::Input,
        _ => Kind::Input,
    }
}

/// 合成文字列・属性 (UTF-16 単位ごと)・文節境界 (UTF-16 オフセット) から、文節の一覧
/// (文字位置) を作る。整合しないときは `None`。
///
/// 文節境界が不正・空のときは、属性が同じ範囲を 1 文節とみなす。
pub fn segments(text: &str, attrs: &[u8], clauses: &[u32]) -> Option<Vec<Segment>> {
    // 文字ごとの UTF-16 開始位置 (末尾に全長)
    let mut starts = Vec::with_capacity(text.len() + 1);
    let mut u = 0usize;
    for c in text.chars() {
        starts.push(u);
        u += c.len_utf16();
    }
    starts.push(u);
    let nchars = starts.len() - 1;
    if nchars == 0 || attrs.len() != u {
        return None;
    }
    let to_char = |off: usize| starts.binary_search(&off).ok();

    let valid = clauses.len() >= 2
        && clauses[0] == 0
        && clauses[clauses.len() - 1] as usize == u
        && clauses.windows(2).all(|w| w[0] < w[1]);
    // UTF-16 の境界列 (文節境界 or 属性の変わり目)
    let bounds: Vec<usize> = if valid {
        clauses.iter().map(|&c| c as usize).collect()
    } else {
        let mut b = vec![0];
        for i in 1..attrs.len() {
            if attr_kind(attrs[i]) != attr_kind(attrs[i - 1]) {
                b.push(i);
            }
        }
        b.push(u);
        b
    };

    let mut out = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (to_char(w[0])?, to_char(w[1])?);
        let slice = &attrs[w[0]..w[1]];
        let kind = if slice.iter().any(|&x| attr_kind(x) == Kind::Target) {
            Kind::Target
        } else {
            attr_kind(slice[0])
        };
        out.push(Segment { range: a..b, kind });
    }
    Some(out)
}

/// 変換中の文節を取得する。`expected` は egui が把握している未確定文字列で、
/// IME の合成文字列と一致しない (タイミングのずれなど) 場合は空を返す。
#[cfg(windows)]
pub fn query(hwnd: isize, expected: &str) -> Vec<Segment> {
    use windows_sys::Win32::UI::Input::Ime::{
        ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPATTR, GCS_COMPCLAUSE,
        GCS_COMPSTR,
    };

    if hwnd == 0 || expected.is_empty() {
        return Vec::new();
    }
    // SAFETY: hwnd は eframe が所有する自ウィンドウのハンドル。取得した IMC は必ず
    // ImmReleaseContext で解放し、バッファは各呼び出しの直前に報告された大きさで確保する。
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc == 0 {
            return Vec::new();
        }
        // 戻り値はバイト数 (負ならエラー)。データは要素 T のバイト列として読む
        let read = |flag: u32, elem: usize| -> Option<Vec<u8>> {
            let n = ImmGetCompositionStringW(himc, flag, std::ptr::null_mut(), 0);
            if n <= 0 || n as usize % elem != 0 {
                return None;
            }
            let mut buf = vec![0u8; n as usize];
            let m = ImmGetCompositionStringW(himc, flag, buf.as_mut_ptr().cast(), n as u32);
            (m == n).then_some(buf)
        };
        let s = read(GCS_COMPSTR, 2);
        let a = read(GCS_COMPATTR, 1);
        let c = read(GCS_COMPCLAUSE, 4);
        ImmReleaseContext(hwnd, himc);

        let (Some(s), Some(a)) = (s, a) else {
            return Vec::new();
        };
        let units: Vec<u16> = s
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        if String::from_utf16(&units).ok().as_deref() != Some(expected) {
            return Vec::new();
        }
        let clauses: Vec<u32> = c
            .unwrap_or_default()
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        segments(expected, &a, &clauses).unwrap_or_default()
    }
}

#[cfg(not(windows))]
pub fn query(_hwnd: isize, _expected: &str) -> Vec<Segment> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(a: usize, b: usize, kind: Kind) -> Segment {
        Segment { range: a..b, kind }
    }

    #[test]
    fn unconverted_is_single_input_clause() {
        let r = segments("あいう", &[0, 0, 0], &[0, 3]).unwrap();
        assert_eq!(r, vec![seg(0, 3, Kind::Input)]);
    }

    #[test]
    fn converted_clauses_with_target() {
        // 「今日は」「晴れ」: 2 文節目が対象
        let r = segments("今日は晴れ", &[2, 2, 2, 1, 1], &[0, 3, 5]).unwrap();
        assert_eq!(r, vec![seg(0, 3, Kind::Converted), seg(3, 5, Kind::Target)]);
    }

    #[test]
    fn surrogate_pairs_use_utf16_offsets() {
        // 𠮷 (U+20BB7) は UTF-16 で 2 単位 = 1 文字
        let r = segments("𠮷あ", &[1, 1, 2], &[0, 2, 3]).unwrap();
        assert_eq!(r, vec![seg(0, 1, Kind::Target), seg(1, 2, Kind::Converted)]);
        // サロゲートペアの途中を指す境界は不正
        assert_eq!(segments("𠮷あ", &[1, 1, 2], &[0, 1, 3]), None);
    }

    #[test]
    fn invalid_clauses_fall_back_to_attribute_runs() {
        let r = segments("あいう", &[2, 1, 1], &[]).unwrap();
        assert_eq!(r, vec![seg(0, 1, Kind::Converted), seg(1, 3, Kind::Target)]);
        let r = segments("あいう", &[2, 1, 1], &[0, 2, 2, 3]).unwrap();
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn mismatched_lengths_are_rejected() {
        assert_eq!(segments("あいう", &[0, 0], &[0, 3]), None);
        assert_eq!(segments("", &[], &[]), None);
        // 属性は UTF-16 単位数であること (サロゲートペアで 2)
        assert_eq!(segments("𠮷", &[1], &[0, 2]), None);
    }
}
