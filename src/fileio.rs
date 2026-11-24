//! ファイルの読み書き。UI に依存しない部分 (安全な保存・外部変更の検出・読み込み)

use crate::encoding::{self, Enc, Eol};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// これより大きいファイルは、開く前に確認する (バイト)
pub const LARGE_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// ファイルの「指紋」。読み込み・保存の時点で控えておき、上書きの直前にもう一度取って比べて、
/// 他のプログラムによる変更を検出する (大きさ + 更新日時だけなので軽い)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

impl Fingerprint {
    fn from_metadata(m: &std::fs::Metadata) -> Self {
        Fingerprint {
            len: m.len(),
            modified: m.modified().ok(),
        }
    }
}

/// 現在のファイルの指紋。存在しない・読めないときは `None`
pub fn fingerprint(path: &Path) -> Option<Fingerprint> {
    std::fs::metadata(path)
        .ok()
        .map(|m| Fingerprint::from_metadata(&m))
}

/// 控えておいた指紋と今のファイルが違うか (削除された・作り直された場合も含む)
pub fn changed_since(known: &Fingerprint, path: &Path) -> bool {
    fingerprint(path).as_ref() != Some(known)
}

/// 同じファイルかどうかを比べるためのキー。
///
/// 正規化 (シンボリックリンクや `..` の解決) した絶対パスにする。Windows ではファイル名の
/// 大文字小文字を区別しないので小文字にそろえる。存在しないパスは、そのままのパスを使う
pub fn path_key(path: &Path) -> String {
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// 一時ファイルの名前に付ける連番 (同じ保存先に同時に書いても衝突しないように)
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_path_for(path: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{}.{}.{}.rstmp", name, std::process::id(), n))
}

#[cfg(windows)]
fn replace_file(target: &Path, temp: &Path, target_exists: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        REPLACEFILE_IGNORE_MERGE_ERRORS,
    };
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (t, s) = (wide(target), wide(temp));
    // SAFETY: t / s は NUL 終端の UTF-16 文字列で、呼び出しの間は生きている。
    // 予約引数 (バックアップ名・除外・予約) は仕様どおりすべて NULL。
    let ok = unsafe {
        if target_exists {
            // 元のファイルの属性・ACL・代替データストリームを引き継いだまま置き換える
            ReplaceFileW(
                t.as_ptr(),
                s.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_IGNORE_MERGE_ERRORS,
                std::ptr::null(),
                std::ptr::null(),
            ) != 0
        } else {
            MoveFileExW(
                s.as_ptr(),
                t.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            ) != 0
        }
    };
    if ok {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn replace_file(target: &Path, temp: &Path, _target_exists: bool) -> io::Result<()> {
    std::fs::rename(temp, target)
}

/// ファイルを安全に書き込む。
///
/// 同じフォルダの一時ファイルへ書ききって (`sync_all` でディスクへ反映)、それから保存先と
/// 入れ替える。書き込みの途中で失敗しても (ディスクの空き不足・ネットワーク切断など)
/// 元のファイルには手を付けない。失敗したときは一時ファイルを削除する。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let existing = std::fs::metadata(path).ok();
    if let Some(m) = &existing {
        if m.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ファイルが読み取り専用です",
            ));
        }
    }
    let temp = temp_path_for(path);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        replace_file(path, &temp, existing.is_some())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// 読み込んだ結果
pub struct Loaded {
    /// 改行を LF に統一した本文
    pub text: String,
    pub enc: Enc,
    /// 先頭の改行で判定した改行コード
    pub eol: Eol,
    /// 改行コードが混在していたか
    pub mixed_eol: bool,
    /// デコードできないバイトがあった (U+FFFD に置き換わっている) か
    pub lossy: bool,
    /// 読み込んだ時点のファイルの指紋
    pub fingerprint: Option<Fingerprint>,
}

/// ファイルを読み込んでデコードする (バックグラウンドのスレッドから呼ぶ)。
/// `forced` を指定すると、その文字コードで読む。失敗は表示用のメッセージで返す
pub fn load(path: &Path, forced: Option<Enc>) -> Result<Loaded, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    // 読み込み前の指紋 (読み込み中に変更されたら、次の保存で検出できる)
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let fp = Fingerprint::from_metadata(&meta);
    let mut bytes: Vec<u8> = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(meta.len()).unwrap_or(usize::MAX))
        .map_err(|_| "メモリが足りないため、ファイルを読み込めません".to_string())?;
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if forced.is_none() {
        if let Some(name) = encoding::unsupported_bom(&bytes) {
            return Err(format!(
                "{name} のファイルには対応していません。\nUTF-8 / UTF-16 / Shift_JIS などに変換してから開いてください。"
            ));
        }
    }
    let enc = forced.unwrap_or_else(|| encoding::detect(&bytes));
    let (text, lossy) = encoding::decode(&bytes, enc);
    drop(bytes);
    let eol = encoding::detect_eol(&text);
    let mixed_eol = encoding::eol_mixed(&text);
    Ok(Loaded {
        text: encoding::normalize_eol(text),
        enc,
        eol,
        mixed_eol,
        lossy,
        fingerprint: Some(fp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストごとの作業フォルダ (終わると削除)
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rs_editor_test_{}_{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn leftovers(&self) -> Vec<String> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".rstmp"))
                .collect()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            if let Ok(rd) = std::fs::read_dir(&self.0) {
                for e in rd.flatten() {
                    let p = e.path();
                    if let Ok(m) = std::fs::metadata(&p) {
                        let mut perm = m.permissions();
                        perm.set_readonly(false);
                        let _ = std::fs::set_permissions(&p, perm);
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_write_replaces_and_creates() {
        let d = TempDir::new();
        let p = d.file("a.txt");
        atomic_write(&p, b"first").unwrap(); // 新規
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        atomic_write(&p, b"second, longer").unwrap(); // 置き換え
        assert_eq!(std::fs::read(&p).unwrap(), b"second, longer");
        atomic_write(&p, b"x").unwrap(); // 短くなっても残らない
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
        assert!(d.leftovers().is_empty());
    }

    #[test]
    fn atomic_write_failure_keeps_original_and_cleans_up() {
        let d = TempDir::new();
        // 保存先のフォルダが無い
        assert!(atomic_write(&d.file("nodir/a.txt"), b"x").is_err());
        // 読み取り専用のファイルには書けず、中身はそのまま
        let p = d.file("ro.txt");
        std::fs::write(&p, b"original").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm).unwrap();
        assert!(atomic_write(&p, b"new").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"original");
        assert!(d.leftovers().is_empty());
    }

    #[test]
    fn fingerprint_detects_change_and_deletion() {
        let d = TempDir::new();
        let p = d.file("f.txt");
        std::fs::write(&p, b"abc").unwrap();
        let fp = fingerprint(&p).unwrap();
        assert!(!changed_since(&fp, &p));
        std::fs::write(&p, b"abcdef").unwrap();
        assert!(changed_since(&fp, &p), "大きさが変わった");
        let fp2 = fingerprint(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert!(changed_since(&fp2, &p), "削除された");
        assert!(fingerprint(&p).is_none());
    }

    #[test]
    fn load_reports_flags() {
        let d = TempDir::new();
        // BOM 付き UTF-8 + 不正なバイト 0xFF
        let p = d.file("bad.txt");
        std::fs::write(&p, [0xEF, 0xBB, 0xBF, b'a', 0xFF, b'b']).unwrap();
        let l = load(&p, None).unwrap();
        assert_eq!(l.enc, Enc::Utf8Bom);
        assert!(l.lossy);
        assert_eq!(l.text, "a\u{fffd}b");
        assert!(l.fingerprint.is_some());
        // 混在した改行
        let p = d.file("mixed.txt");
        std::fs::write(&p, b"a\r\nb\nc\r").unwrap();
        let l = load(&p, None).unwrap();
        assert!(l.mixed_eol && !l.lossy);
        assert_eq!(l.eol, Eol::CrLf);
        assert_eq!(l.text, "a\nb\nc\n");
        // UTF-32 は未対応
        let p = d.file("u32.txt");
        std::fs::write(&p, [0xFF, 0xFE, 0, 0, b'a', 0, 0, 0]).unwrap();
        assert!(load(&p, None).is_err());
        // 存在しない
        assert!(load(&d.file("none.txt"), None).is_err());
        // 文字コードを指定
        let p = d.file("sjis.txt");
        std::fs::write(&p, encoding::encode("日本語", Enc::Legacy(encoding_rs::SHIFT_JIS)).0)
            .unwrap();
        let l = load(&p, Some(Enc::Legacy(encoding_rs::SHIFT_JIS))).unwrap();
        assert_eq!(l.text, "日本語");
    }

    #[test]
    fn path_key_identifies_same_file() {
        let d = TempDir::new();
        let p = d.file("Case.txt");
        std::fs::write(&p, b"x").unwrap();
        let dotted = d.0.join(".").join("Case.txt");
        assert_eq!(path_key(&p), path_key(&dotted));
        if cfg!(windows) {
            assert_eq!(path_key(&p), path_key(&d.file("CASE.TXT")));
        }
        std::fs::write(d.file("other.txt"), b"y").unwrap();
        assert_ne!(path_key(&p), path_key(&d.file("other.txt")));
    }
}
