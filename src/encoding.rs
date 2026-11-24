//! 文字コードと改行コードの判定・変換

use encoding_rs::Encoding;

/// エディタが扱う文字コード
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Enc {
    Utf8,
    Utf8Bom,
    /// BOM 付きの UTF-16 (新規保存・メニューから選んだときの既定)
    Utf16Le,
    Utf16Be,
    /// BOM なしの UTF-16。BOM の有無は文書ごとに保持し、読み込んだ形のまま書き戻す
    Utf16LeNoBom,
    Utf16BeNoBom,
    /// encoding_rs がサポートするその他の文字コード (Shift_JIS, EUC-JP など)
    Legacy(&'static Encoding),
}

impl Enc {
    pub fn label(&self) -> String {
        match self {
            Enc::Utf8 => "UTF-8".into(),
            Enc::Utf8Bom => "UTF-8 (BOM付き)".into(),
            Enc::Utf16Le => "UTF-16 LE (BOM付き)".into(),
            Enc::Utf16Be => "UTF-16 BE (BOM付き)".into(),
            Enc::Utf16LeNoBom => "UTF-16 LE (BOMなし)".into(),
            Enc::Utf16BeNoBom => "UTF-16 BE (BOMなし)".into(),
            Enc::Legacy(e) => e.name().to_string(),
        }
    }

    /// メニューに並べる文字コード一覧
    pub fn all() -> Vec<Enc> {
        vec![
            Enc::Utf8,
            Enc::Utf8Bom,
            Enc::Utf16Le,
            Enc::Utf16Be,
            Enc::Utf16LeNoBom,
            Enc::Utf16BeNoBom,
            Enc::Legacy(encoding_rs::SHIFT_JIS),
            Enc::Legacy(encoding_rs::EUC_JP),
            Enc::Legacy(encoding_rs::ISO_2022_JP),
            Enc::Legacy(encoding_rs::WINDOWS_1252),
            Enc::Legacy(encoding_rs::GBK),
            Enc::Legacy(encoding_rs::BIG5),
            Enc::Legacy(encoding_rs::EUC_KR),
        ]
    }
}

/// 未対応の BOM (UTF-32) なら、その名前を返す。
///
/// UTF-32LE の BOM (FF FE 00 00) は UTF-16LE の BOM (FF FE) で始まるため、
/// 先に調べないと UTF-16LE として誤って開いてしまう。
pub fn unsupported_bom(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        Some("UTF-32 LE")
    } else if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        Some("UTF-32 BE")
    } else {
        None
    }
}

/// BOM なしの UTF-16 らしさを調べる。
///
/// ASCII 中心の UTF-16 は NUL バイトを多く含み (LE なら奇数番目、BE なら偶数番目に集中)、
/// そのままでも UTF-8 として妥当になってしまうため、UTF-8 の判定より前に確かめる。
/// 判定: 長さが偶数で、片側の位置の 30% 以上が 0 かつ、もう片側の 0 が 5% 以下。
fn guess_utf16_without_bom(bytes: &[u8]) -> Option<Enc> {
    if bytes.len() < 4 || bytes.len() % 2 != 0 {
        return None;
    }
    // 先頭の一部だけで十分 (偶数長に切る)
    let sample = &bytes[..bytes.len().min(64 * 1024) & !1];
    let n = sample.len() / 2;
    let (mut even_zero, mut odd_zero) = (0usize, 0usize);
    for pair in sample.chunks_exact(2) {
        even_zero += (pair[0] == 0) as usize;
        odd_zero += (pair[1] == 0) as usize;
    }
    let dense = |z: usize| z * 10 >= n * 3;
    let sparse = |z: usize| z * 20 <= n;
    if dense(odd_zero) && sparse(even_zero) {
        Some(Enc::Utf16LeNoBom)
    } else if dense(even_zero) && sparse(odd_zero) {
        Some(Enc::Utf16BeNoBom)
    } else {
        None
    }
}

/// バイト列から文字コードを推定する (UTF-32 の BOM は先に `unsupported_bom` で除くこと)
pub fn detect(bytes: &[u8]) -> Enc {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Enc::Utf8Bom;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Enc::Utf16Le;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Enc::Utf16Be;
    }
    if let Some(enc) = guess_utf16_without_bom(bytes) {
        return enc;
    }
    // ISO-2022-JP は 7bit なので UTF-8 としても妥当になってしまう。エスケープシーケンスで先に判定する
    if bytes.is_ascii()
        && (contains(bytes, b"\x1b$B") || contains(bytes, b"\x1b$@") || contains(bytes, b"\x1b(J"))
    {
        return Enc::Legacy(encoding_rs::ISO_2022_JP);
    }
    if std::str::from_utf8(bytes).is_ok() {
        return Enc::Utf8;
    }
    let mut det = chardetng::EncodingDetector::new();
    det.feed(bytes, true);
    let guess = det.guess(Some(b"jp"), true);
    if guess == encoding_rs::UTF_8 {
        Enc::Utf8
    } else if guess == encoding_rs::UTF_16LE {
        Enc::Utf16Le
    } else if guess == encoding_rs::UTF_16BE {
        Enc::Utf16Be
    } else {
        Enc::Legacy(guess)
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 指定した文字コードでデコードする。戻り値の bool はデコードエラー (不正なバイト) の有無
pub fn decode(bytes: &[u8], enc: Enc) -> (String, bool) {
    let (s, err) = match enc {
        Enc::Utf8 | Enc::Utf8Bom => encoding_rs::UTF_8.decode_with_bom_removal(bytes),
        Enc::Utf16Le => encoding_rs::UTF_16LE.decode_with_bom_removal(bytes),
        Enc::Utf16Be => encoding_rs::UTF_16BE.decode_with_bom_removal(bytes),
        Enc::Utf16LeNoBom => encoding_rs::UTF_16LE.decode_without_bom_handling(bytes),
        Enc::Utf16BeNoBom => encoding_rs::UTF_16BE.decode_without_bom_handling(bytes),
        Enc::Legacy(e) => e.decode_without_bom_handling(bytes),
    };
    (s.into_owned(), err)
}

fn utf16_bytes(text: &str, bom: Option<[u8; 2]>, to_bytes: fn(u16) -> [u8; 2]) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len() * 2 + 2);
    if let Some(b) = bom {
        v.extend_from_slice(&b);
    }
    for u in text.encode_utf16() {
        v.extend_from_slice(&to_bytes(u));
    }
    v
}

/// 指定した文字コードでエンコードする。
///
/// 戻り値の bool は「保存した内容が元の文字列と一致しない」かどうか。
/// 変換できない文字 (`&#数値;` に置き換わる) だけでなく、エンコードしてデコードし直すと
/// 別の文字になるもの (例: Shift_JIS では ¥ U+00A5 が 0x5C になり、開き直すと \ に変わる。
/// encoding_rs のエンコーダはこれをエラーにしない) も含む。
pub fn encode(text: &str, enc: Enc) -> (Vec<u8>, bool) {
    match enc {
        Enc::Utf8 => (text.as_bytes().to_vec(), false),
        Enc::Utf8Bom => {
            let mut v = Vec::with_capacity(text.len() + 3);
            v.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            v.extend_from_slice(text.as_bytes());
            (v, false)
        }
        // encoding_rs は UTF-16 へのエンコードをサポートしないため自前で行う
        Enc::Utf16Le => (utf16_bytes(text, Some([0xFF, 0xFE]), u16::to_le_bytes), false),
        Enc::Utf16Be => (utf16_bytes(text, Some([0xFE, 0xFF]), u16::to_be_bytes), false),
        Enc::Utf16LeNoBom => (utf16_bytes(text, None, u16::to_le_bytes), false),
        Enc::Utf16BeNoBom => (utf16_bytes(text, None, u16::to_be_bytes), false),
        Enc::Legacy(e) => {
            let (b, _, err) = e.encode(text);
            let b = b.into_owned();
            let lossy = err || {
                let (back, derr) = e.decode_without_bom_handling(&b);
                derr || back != text
            };
            (b, lossy)
        }
    }
}

/// `encode` で文字が変わってしまうとき、変わる文字の例を `max` 個まで ("¥ → \" の形で) 返す
pub fn lossy_examples(text: &str, enc: Enc, max: usize) -> Vec<String> {
    let Enc::Legacy(e) = enc else {
        return Vec::new(); // UTF 系は全文字を表現できる
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in text.chars().filter(|c| !c.is_ascii()) {
        if out.len() >= max {
            break;
        }
        if !seen.insert(c) {
            continue;
        }
        let one = c.to_string();
        let (b, _, err) = e.encode(&one);
        let (back, derr) = e.decode_without_bom_handling(&b);
        if err || derr || back != one {
            out.push(format!("{c} → {back}"));
        }
    }
    out
}

/// 改行コード
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Eol {
    Lf,
    CrLf,
    Cr,
}

impl Eol {
    pub fn label(&self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
            Eol::Cr => "CR",
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
            Eol::Cr => "\r",
        }
    }
    pub fn all() -> [Eol; 3] {
        [Eol::CrLf, Eol::Lf, Eol::Cr]
    }
}

/// 最初に現れた改行で改行コードを判定 (改行が無ければ Windows 標準の CRLF)
pub fn detect_eol(s: &str) -> Eol {
    let b = s.as_bytes();
    match b.iter().position(|&c| c == b'\r' || c == b'\n') {
        Some(i) if b[i] == b'\r' => {
            if b.get(i + 1) == Some(&b'\n') {
                Eol::CrLf
            } else {
                Eol::Cr
            }
        }
        Some(_) => Eol::Lf,
        None => Eol::CrLf,
    }
}

/// 改行コードが混在しているか (CRLF / LF / CR のうち 2 種類以上が現れるか)
pub fn eol_mixed(s: &str) -> bool {
    let b = s.as_bytes();
    let (mut crlf, mut lf, mut cr) = (false, false, false);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                crlf = true;
                i += 1;
            }
            b'\r' => cr = true,
            b'\n' => lf = true,
            _ => {}
        }
        i += 1;
    }
    (crlf as u8 + lf as u8 + cr as u8) >= 2
}

/// 内部表現では改行を LF に統一する
pub fn normalize_eol(s: String) -> String {
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s
    }
}

/// LF で保持しているテキストを指定の改行コードにする
pub fn apply_eol(s: &str, eol: Eol) -> String {
    if eol == Eol::Lf {
        s.to_owned()
    } else {
        s.replace('\n', eol.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_japanese() {
        let text = "日本語テキスト ABC 123";
        for enc in Enc::all() {
            if matches!(enc, Enc::Legacy(e) if e == encoding_rs::WINDOWS_1252) {
                continue; // 日本語を表現できない
            }
            let (bytes, err) = encode(text, enc);
            if err {
                continue; // GBK/Big5/EUC-KR はかなを含められない場合がある
            }
            let (back, derr) = decode(&bytes, enc);
            assert!(!derr, "{}", enc.label());
            assert_eq!(back, text, "{}", enc.label());
        }
    }

    #[test]
    fn detect_common() {
        let text = "これは文字コード判定のテストです。漢字とひらがなとカタカナ。";
        assert_eq!(detect(text.as_bytes()), Enc::Utf8);
        let sjis = encode(text, Enc::Legacy(encoding_rs::SHIFT_JIS)).0;
        assert_eq!(detect(&sjis), Enc::Legacy(encoding_rs::SHIFT_JIS));
        let euc = encode(text, Enc::Legacy(encoding_rs::EUC_JP)).0;
        assert_eq!(detect(&euc), Enc::Legacy(encoding_rs::EUC_JP));
        let jis = encode(text, Enc::Legacy(encoding_rs::ISO_2022_JP)).0;
        assert_eq!(detect(&jis), Enc::Legacy(encoding_rs::ISO_2022_JP));
        let u16 = encode(text, Enc::Utf16Le).0;
        assert_eq!(detect(&u16), Enc::Utf16Le);
    }

    #[test]
    fn eol() {
        assert_eq!(detect_eol("a\r\nb"), Eol::CrLf);
        assert_eq!(detect_eol("a\nb"), Eol::Lf);
        assert_eq!(detect_eol("a\rb"), Eol::Cr);
        assert_eq!(normalize_eol("a\r\nb\rc".into()), "a\nb\nc");
        assert_eq!(apply_eol("a\nb", Eol::CrLf), "a\r\nb");
    }

    #[test]
    fn utf32_bom_is_unsupported() {
        assert_eq!(
            unsupported_bom(&[0xFF, 0xFE, 0x00, 0x00, 0x41, 0, 0, 0]),
            Some("UTF-32 LE")
        );
        assert_eq!(
            unsupported_bom(&[0x00, 0x00, 0xFE, 0xFF, 0, 0, 0, 0x41]),
            Some("UTF-32 BE")
        );
        // UTF-16LE の BOM は未対応ではない
        assert_eq!(unsupported_bom(&[0xFF, 0xFE, 0x41, 0x00]), None);
    }

    #[test]
    fn detect_bomless_utf16() {
        assert_eq!(detect(&[0x41, 0x00, 0x0A, 0x00]), Enc::Utf16LeNoBom);
        assert_eq!(detect(&[0x00, 0x41, 0x00, 0x0A]), Enc::Utf16BeNoBom);
        let text = "Hello, world!\r\nsecond line\r\n";
        let le = encode(text, Enc::Utf16LeNoBom).0;
        assert_eq!(detect(&le), Enc::Utf16LeNoBom);
        assert_eq!(decode(&le, Enc::Utf16LeNoBom).0, text);
        let be = encode(text, Enc::Utf16BeNoBom).0;
        assert_eq!(detect(&be), Enc::Utf16BeNoBom);
        // BOM は付かない
        assert_eq!(le[..2], [b'H', 0]);
        // 通常の ASCII / UTF-8 / NUL を含まないバイト列は UTF-16 にしない
        assert_eq!(detect(b"abcd"), Enc::Utf8);
        assert_eq!(detect("日本語".as_bytes()), Enc::Utf8);
        // 奇数長は UTF-16 ではない
        assert_eq!(detect(&[0x41, 0x00, 0x42, 0x00, 0x43]), Enc::Utf8);
        // NUL だらけ (両側に 0) のバイナリは UTF-16 とみなさない
        assert_eq!(detect(&[0u8; 16]), Enc::Utf8);
    }

    #[test]
    fn utf16_bom_kept_separately() {
        let t = "abc";
        assert_eq!(
            encode(t, Enc::Utf16Le).0,
            [0xFF, 0xFE, b'a', 0, b'b', 0, b'c', 0]
        );
        assert_eq!(encode(t, Enc::Utf16LeNoBom).0, [b'a', 0, b'b', 0, b'c', 0]);
        assert_eq!(decode(&encode(t, Enc::Utf16Be).0, Enc::Utf16Be).0, t);
    }

    #[test]
    fn sjis_yen_and_overline_are_lossy() {
        let sjis = Enc::Legacy(encoding_rs::SHIFT_JIS);
        // U+00A5 は 0x5C になり、開き直すと \ に変わる (エンコーダはエラーを出さない)
        let (bytes, lossy) = encode("a\u{a5}b", sjis);
        assert_eq!(bytes, [b'a', 0x5C, b'b']);
        assert!(lossy);
        assert!(encode("a\u{203e}", sjis).1);
        // 通常の日本語は変わらない
        assert!(!encode("日本語ABC\\~", sjis).1);
        // 表現できない文字
        assert!(encode("\u{2603}", sjis).1);
        let ex = lossy_examples("a\u{a5}\u{2603}\u{a5}", sjis, 5);
        assert_eq!(
            ex,
            vec!["\u{a5} → \\".to_string(), "\u{2603} → &#9731;".to_string()]
        );
        assert!(lossy_examples("abc", sjis, 5).is_empty());
        assert!(lossy_examples("\u{a5}", Enc::Utf8, 5).is_empty());
    }

    #[test]
    fn invalid_utf8_reports_decode_error() {
        let bytes = [0xEF, 0xBB, 0xBF, b'a', 0xFF, b'b'];
        assert_eq!(detect(&bytes), Enc::Utf8Bom);
        let (text, err) = decode(&bytes, Enc::Utf8Bom);
        assert!(err);
        assert_eq!(text, "a\u{fffd}b");
    }

    #[test]
    fn mixed_eol_detection() {
        assert!(eol_mixed("a\r\nb\nc\r"));
        assert!(eol_mixed("a\r\nb\n"));
        assert!(!eol_mixed("a\r\nb\r\n"));
        assert!(!eol_mixed("a\nb\n"));
        assert!(!eol_mixed("abc"));
        assert!(!eol_mixed("a\rb\r"));
    }
}
