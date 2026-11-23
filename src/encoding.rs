//! 文字コードと改行コードの判定・変換

use encoding_rs::Encoding;

/// エディタが扱う文字コード
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Enc {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
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

/// バイト列から文字コードを推定する
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
        Enc::Legacy(e) => e.decode_without_bom_handling(bytes),
    };
    (s.into_owned(), err)
}

/// 指定した文字コードでエンコードする。戻り値の bool は変換できない文字があったかどうか
pub fn encode(text: &str, enc: Enc) -> (Vec<u8>, bool) {
    match enc {
        Enc::Utf8 => (text.as_bytes().to_vec(), false),
        Enc::Utf8Bom => {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice(text.as_bytes());
            (v, false)
        }
        // encoding_rs は UTF-16 へのエンコードをサポートしないため自前で行う
        Enc::Utf16Le => {
            let mut v = vec![0xFF, 0xFE];
            for u in text.encode_utf16() {
                v.extend_from_slice(&u.to_le_bytes());
            }
            (v, false)
        }
        Enc::Utf16Be => {
            let mut v = vec![0xFE, 0xFF];
            for u in text.encode_utf16() {
                v.extend_from_slice(&u.to_be_bytes());
            }
            (v, false)
        }
        Enc::Legacy(e) => {
            let (b, _, err) = e.encode(text);
            (b.into_owned(), err)
        }
    }
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
}
