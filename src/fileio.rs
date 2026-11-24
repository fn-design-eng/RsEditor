//! ファイルの読み書き。UI に依存しない部分 (安全な保存・外部変更の検出・読み込み)

use crate::encoding::{self, Enc, Eol};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// これより大きいファイルは、開く前に確認する (バイト)
pub const LARGE_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// ファイルの識別子 (ボリュームのシリアル番号, ファイルのインデックス)。
/// 同じ大きさ・同じ更新日時で別のファイルに差し替えられた場合も見分けるために使う
type FileId = (u32, u64);

/// ファイルの「指紋」。読み込み・保存の時点で控えておき、上書きの直前にもう一度取って比べて、
/// 他のプログラムによる変更を検出する (大きさ + 更新日時 + ファイルの識別子)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
    /// 取得できなかった (Windows 以外・開けなかった) ときは `None`
    id: Option<FileId>,
}

#[cfg(windows)]
fn file_id(f: &std::fs::File) -> Option<FileId> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    // SAFETY: 開いているファイルのハンドルと、書き込み先の構造体 (ゼロ初期化) を渡すだけ。
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as isize, &mut info) };
    (ok != 0).then(|| {
        (
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
}

#[cfg(not(windows))]
fn file_id(_f: &std::fs::File) -> Option<FileId> {
    None
}

impl Fingerprint {
    /// 開いているファイルから取る (読み込み中のファイルと指紋が確実に同じ実体になる)
    fn from_file(f: &std::fs::File) -> Option<Self> {
        let m = f.metadata().ok()?;
        Some(Fingerprint {
            len: m.len(),
            modified: m.modified().ok(),
            id: file_id(f),
        })
    }
}

/// 現在のファイルの指紋。存在しない・読めないときは `None`
pub fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // 内容は読まないので、アクセス権なし (属性の取得だけ) で開く。共有モードは
        // std の既定 (読み書き・削除を妨げない) のまま
        o.access_mode(0);
    }
    Fingerprint::from_file(&o.open(path).ok()?)
}

/// 控えておいた指紋と今のファイルが違うか (削除された・作り直された場合も含む)
#[cfg(test)]
pub fn changed_since(known: &Fingerprint, path: &Path) -> bool {
    fingerprint(path).as_ref() != Some(known)
}

/// 書き込む直前に、保存先がどうなっていることを期待するか
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    /// 確かめない (テスト用)
    #[cfg(test)]
    Any,
    /// 保存先の指紋がこのとおりであること (`None` は「存在しないこと」)
    Is(Option<Fingerprint>),
}

/// 書き込みの失敗
#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    /// 保存先が、期待した状態ではなくなっていた (他のプログラムが変更・作成・削除した)。
    /// 何も書き換えていない
    Conflict,
    /// その他の失敗 (表示用のメッセージ)
    Failed(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Conflict => {
                f.write_str("保存先のファイルが、他のプログラムによって変更されました")
            }
            WriteError::Failed(m) => f.write_str(m),
        }
    }
}

fn failed(e: impl std::fmt::Display) -> WriteError {
    WriteError::Failed(e.to_string())
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

/// 一時ファイル・控えの名前に付ける連番 (同じ保存先に同時に書いても衝突しないように)
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn sibling_path(path: &Path, ext: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{}.{}.{}.{}", name, std::process::id(), n, ext))
}

fn exists_at(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// まだ存在しない名前を選ぶ (プロセス番号の再利用などで、他のファイルとぶつからないように)
fn unused_sibling(path: &Path, ext: &str) -> PathBuf {
    loop {
        let p = sibling_path(path, ext);
        if !exists_at(&p) {
            return p;
        }
    }
}

/// 一時ファイルを削除する。呼んでよいのは、そのファイルを自分で作った (`create_new` が
/// 成功した) 場合だけ
fn remove_own_temp(temp: &Path) {
    let _ = std::fs::remove_file(temp);
}

/// 置き換えに失敗した後の後始末。ファイルの有無を実際に確かめて判断する
/// (エラー番号だけに頼らない)。
///
/// - 元のファイルが控え (backup) の名前に移ったまま戻っていなければ、元の名前へ戻す
///   (`ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` = 1177 のとき)。
/// - 元のファイルが元の名前のままなら (1175 / 1176 など)、一時ファイルを消すだけ。
/// - 戻せない・状態が想定外のときは、何も消さずに、残っているファイルの場所を知らせる
///   (新旧どちらの内容も残っているので、利用者が手で戻せる)。
#[cfg_attr(not(windows), allow(dead_code))]
fn recover_failed_replace(target: &Path, temp: &Path, backup: &Path, err: &io::Error) -> WriteError {
    let (target_there, backup_there) = (exists_at(target), exists_at(backup));
    let left_over = |why: &str| {
        WriteError::Failed(format!(
            "{why}\n元のファイルの控え: {}\n新しい内容の一時ファイル: {}\n\
             (控えのファイル名を {} に戻すと、元の状態に戻せます)",
            backup.display(),
            temp.display(),
            target.display()
        ))
    };
    if backup_there {
        if target_there {
            return left_over(&format!(
                "保存先の置き換えに失敗し、状態を確認できませんでした: {err}"
            ));
        }
        if let Err(e2) = std::fs::rename(backup, target) {
            return left_over(&format!(
                "保存先の置き換えに失敗し、元のファイルも戻せませんでした: {err} / {e2}"
            ));
        }
    }
    remove_own_temp(temp);
    if err.raw_os_error() == Some(2) && !exists_at(target) {
        return WriteError::Conflict; // 置き換える相手のファイルが、その間に消えた
    }
    WriteError::Failed(format!(
        "保存先のファイルを置き換えられませんでした: {err}\n\
         (元のファイルの属性やアクセス権を引き継げないときは、「名前を付けて保存」で別の場所に保存してください)"
    ))
}

#[cfg(windows)]
fn replace_file(
    target: &Path,
    temp: &Path,
    backup: &Path,
    target_exists: bool,
) -> Result<(), WriteError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH,
    };
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (t, s, b) = (wide(target), wide(temp), wide(backup));
    if target_exists {
        // SAFETY: t / s / b は NUL 終端の UTF-16 文字列で、呼び出しの間は生きている。
        // 除外・予約の引数は仕様どおり NULL。
        //
        // - 控え (b) を必ず指定する: 指定しないと、1176 (置き換え後の名前変更に失敗) のときに
        //   元のファイルが消えたまま、新しい内容が一時ファイルの名前で残る。指定しておけば
        //   どの失敗でも、新旧どちらも元の名前か控えの名前で残る。
        // - フラグは 0: REPLACEFILE_IGNORE_MERGE_ERRORS を付けると、元のファイルの ACL や属性を
        //   引き継げなくても保存が成功してしまう。引き継げないときは失敗させる。
        let ok = unsafe {
            ReplaceFileW(
                t.as_ptr(),
                s.as_ptr(),
                b.as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            ) != 0
        };
        if ok {
            let _ = std::fs::remove_file(backup); // 元の内容の控えはもう要らない
            Ok(())
        } else {
            Err(recover_failed_replace(
                target,
                temp,
                backup,
                &io::Error::last_os_error(),
            ))
        }
    } else {
        // 新規ファイル: MOVEFILE_REPLACE_EXISTING は付けない。確認してから作るまでの間に
        // 他のプログラムが同じ名前のファイルを作っていたら、上書きせずに衝突として扱う。
        // SAFETY: s / t は NUL 終端の UTF-16 文字列で、呼び出しの間は生きている。
        let ok = unsafe { MoveFileExW(s.as_ptr(), t.as_ptr(), MOVEFILE_WRITE_THROUGH) != 0 };
        if ok {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        remove_own_temp(temp);
        match err.raw_os_error() {
            Some(183) | Some(80) => Err(WriteError::Conflict), // ALREADY_EXISTS / FILE_EXISTS
            _ => Err(failed(err)),
        }
    }
}

#[cfg(not(windows))]
fn replace_file(
    target: &Path,
    temp: &Path,
    backup: &Path,
    target_exists: bool,
) -> Result<(), WriteError> {
    if target_exists {
        match std::fs::rename(temp, target) {
            Ok(()) => Ok(()),
            Err(e) => Err(recover_failed_replace(target, temp, backup, &e)),
        }
    } else {
        // hard_link は、すでにある名前を上書きしない
        let r = std::fs::hard_link(temp, target);
        remove_own_temp(temp);
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(WriteError::Conflict),
            Err(e) => Err(failed(e)),
        }
    }
}

/// ファイルを安全に書き込む。
///
/// 同じフォルダの一時ファイルへ書ききって (`sync_all` でディスクへ反映)、それから保存先と
/// 入れ替える。書き込みの途中で失敗しても (ディスクの空き不足・ネットワーク切断など)
/// 元のファイルには手を付けない。失敗したときは (自分で作った) 一時ファイルを削除する。
///
/// `expect` は置き換えの直前 (一時ファイルを書いて同期した後) に確かめる。違っていれば
/// `WriteError::Conflict` を返して何も置き換えない。
/// なお、この最後の確認から置き換えの瞬間までの間に他のプログラムが書き込む競合は、
/// 原理的には残る (Windows のファイル置き換えに「指紋が同じときだけ」という条件を付ける
/// 方法が無いため)。その間隔は一瞬に絞ってある。
pub fn atomic_write(path: &Path, bytes: &[u8], expect: &Expect) -> Result<(), WriteError> {
    let temp = unused_sibling(path, "rstmp");
    let backup = unused_sibling(path, "rsbak");
    atomic_write_at(path, bytes, expect, &temp, &backup)
}

/// `atomic_write` の本体。一時ファイル・控えの名前を指定する
fn atomic_write_at(
    path: &Path,
    bytes: &[u8],
    expect: &Expect,
    temp: &Path,
    backup: &Path,
) -> Result<(), WriteError> {
    let existing = std::fs::metadata(path).ok();
    if let Some(m) = &existing {
        if m.permissions().readonly() {
            return Err(WriteError::Failed("ファイルが読み取り専用です".into()));
        }
    }
    // 一時ファイルは、自分で作れたとき (create_new が成功したとき) だけ削除の対象にする。
    // すでに同じ名前のファイルがあった場合は、他のファイルなので触らずに失敗する
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(failed)?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = written {
        remove_own_temp(temp);
        return Err(failed(e));
    }
    // 置き換えの直前の確認 (承知した状態から変わっていないか)
    match expect {
        Expect::Is(fp) if fingerprint(path) != *fp => {
            remove_own_temp(temp);
            return Err(WriteError::Conflict);
        }
        Expect::Is(_) => {}
        #[cfg(test)]
        Expect::Any => {}
    }
    replace_file(path, temp, backup, existing.is_some())
}

/// 読み込みの失敗
#[derive(Debug)]
pub enum LoadError {
    Message(String),
    /// UTF-32 の BOM。`as_utf16` は、UTF-16 LE (BOM 付き) としても読めるときの文字コード
    Utf32 {
        name: &'static str,
        as_utf16: Option<Enc>,
    },
}

impl LoadError {
    pub fn message(&self) -> String {
        match self {
            LoadError::Message(m) => m.clone(),
            LoadError::Utf32 { name, .. } => format!(
                "{name} のファイルには対応していません。\nUTF-16 / UTF-8 / Shift_JIS などに変換してから開いてください。"
            ),
        }
    }
}

impl From<String> for LoadError {
    fn from(s: String) -> Self {
        LoadError::Message(s)
    }
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
pub fn load(path: &Path, forced: Option<Enc>) -> Result<Loaded, LoadError> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    // 読み込み前の指紋 (読み込み中に変更されたら、次の保存で検出できる)
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let fp = Fingerprint::from_file(&file);
    let mut bytes: Vec<u8> = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(meta.len()).unwrap_or(usize::MAX))
        .map_err(|_| "メモリが足りないため、ファイルを読み込めません".to_string())?;
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if forced.is_none() {
        if let Some(name) = encoding::unsupported_bom(&bytes) {
            return Err(LoadError::Utf32 {
                name,
                as_utf16: encoding::utf16_alternative(&bytes),
            });
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
        fingerprint: fp,
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
        /// 取り残された一時ファイル・控え
        fn leftovers(&self) -> Vec<String> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".rstmp") || n.ends_with(".rsbak"))
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

    fn write(p: &Path, bytes: &[u8]) -> Result<(), WriteError> {
        atomic_write(p, bytes, &Expect::Any)
    }

    #[test]
    fn atomic_write_replaces_and_creates() {
        let d = TempDir::new();
        let p = d.file("a.txt");
        write(&p, b"first").unwrap(); // 新規
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        write(&p, b"second, longer").unwrap(); // 置き換え
        assert_eq!(std::fs::read(&p).unwrap(), b"second, longer");
        write(&p, b"x").unwrap(); // 短くなっても残らない
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
        // 一時ファイルも、置き換えのときの控えも残らない
        assert!(d.leftovers().is_empty(), "{:?}", d.leftovers());
        let names: Vec<_> = std::fs::read_dir(&d.0).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn atomic_write_failure_keeps_original_and_cleans_up() {
        let d = TempDir::new();
        // 保存先のフォルダが無い
        assert!(write(&d.file("nodir/a.txt"), b"x").is_err());
        // 読み取り専用のファイルには書けず、中身はそのまま
        let p = d.file("ro.txt");
        std::fs::write(&p, b"original").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm).unwrap();
        assert!(write(&p, b"new").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"original");
        assert!(d.leftovers().is_empty());
    }

    /// すでにある同じ名前のファイル (他のプログラムのもの) は、一時ファイルとして
    /// 使わず、削除もしない
    #[test]
    fn existing_temp_name_is_never_deleted() {
        let d = TempDir::new();
        let p = d.file("a.txt");
        std::fs::write(&p, b"original").unwrap();
        let temp = d.file(".a.txt.1.1.rstmp");
        let backup = d.file(".a.txt.1.1.rsbak");
        std::fs::write(&temp, b"someone else's file").unwrap();
        let r = atomic_write_at(&p, b"new", &Expect::Any, &temp, &backup);
        assert!(matches!(r, Err(WriteError::Failed(_))), "{r:?}");
        assert_eq!(std::fs::read(&temp).unwrap(), b"someone else's file");
        assert_eq!(std::fs::read(&p).unwrap(), b"original");
        // 新規ファイルへの保存でも同じ
        let q = d.file("new.txt");
        let r = atomic_write_at(&q, b"new", &Expect::Any, &temp, &backup);
        assert!(r.is_err());
        assert!(!q.exists());
        assert_eq!(std::fs::read(&temp).unwrap(), b"someone else's file");
    }

    /// 置き換えの直前に保存先が変わっていたら、何も置き換えず、一時ファイルも残さない
    #[test]
    fn expectation_is_checked_before_replace() {
        let d = TempDir::new();
        let p = d.file("a.txt");
        std::fs::write(&p, b"one").unwrap();
        let seen = fingerprint(&p);
        // 承知した状態のままなら書ける
        atomic_write(&p, b"two", &Expect::Is(seen.clone())).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        // 承知した後に変わった (大きさが違う)
        let r = atomic_write(&p, b"three", &Expect::Is(seen));
        assert_eq!(r, Err(WriteError::Conflict));
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        // 存在しないはずなのに、ある
        let r = atomic_write(&p, b"x", &Expect::Is(None));
        assert_eq!(r, Err(WriteError::Conflict));
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        // 存在するはずなのに、消えた
        let gone = fingerprint(&p);
        std::fs::remove_file(&p).unwrap();
        assert_eq!(
            atomic_write(&p, b"x", &Expect::Is(gone)),
            Err(WriteError::Conflict)
        );
        assert!(!p.exists());
        assert!(d.leftovers().is_empty());
        // 存在しないことを承知して新規に作れる。作った後は Is(None) では書けない
        atomic_write(&p, b"fresh", &Expect::Is(None)).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"fresh");
    }

    /// 新規ファイルの作成は、同じ名前のファイルを上書きしない (Windows: REPLACE_EXISTING なし)
    #[test]
    fn new_file_never_overwrites_a_late_arrival() {
        let d = TempDir::new();
        let p = d.file("late.txt");
        // 保存先が無い状態で書き始め、置き換えの直前に他のプログラムが作ったと見なす
        let temp = d.file(".late.1.rstmp");
        let backup = d.file(".late.1.rsbak");
        let existing_before = std::fs::metadata(&p).ok();
        assert!(existing_before.is_none());
        // replace_file を直接呼んで「確認の後に作られた」状況を再現する
        std::fs::write(&temp, b"mine").unwrap();
        std::fs::write(&p, b"theirs").unwrap();
        let r = replace_file(&p, &temp, &backup, false);
        assert_eq!(r, Err(WriteError::Conflict));
        assert_eq!(std::fs::read(&p).unwrap(), b"theirs");
        assert!(!temp.exists(), "自分の一時ファイルは片付ける");
    }

    /// 置き換えに失敗した後の後始末 (ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 = 1177 の状態):
    /// 元のファイルが控えの名前に移ったままなら元の名前へ戻し、自分の一時ファイルを消す
    #[test]
    fn recovery_restores_original_from_backup() {
        let d = TempDir::new();
        let (target, temp, backup) = (d.file("a.txt"), d.file("a.rstmp"), d.file("a.rsbak"));
        std::fs::write(&temp, b"new").unwrap();
        std::fs::write(&backup, b"original").unwrap();
        let e = io::Error::from_raw_os_error(1177);
        let r = recover_failed_replace(&target, &temp, &backup, &e);
        assert!(matches!(r, WriteError::Failed(_)));
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert!(!backup.exists() && !temp.exists());
    }

    /// 1176 (控えを指定したので、新旧とも元の名前のまま): 一時ファイルだけを消す
    #[test]
    fn recovery_keeps_original_when_nothing_moved() {
        let d = TempDir::new();
        let (target, temp, backup) = (d.file("a.txt"), d.file("a.rstmp"), d.file("a.rsbak"));
        std::fs::write(&target, b"original").unwrap();
        std::fs::write(&temp, b"new").unwrap();
        let e = io::Error::from_raw_os_error(1176);
        let r = recover_failed_replace(&target, &temp, &backup, &e);
        assert!(matches!(r, WriteError::Failed(m) if m.contains("名前を付けて保存")));
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert!(!temp.exists());
    }

    /// 戻せない・状態が想定外のときは、何も消さず、残っているファイルの場所を知らせる
    #[test]
    fn recovery_never_deletes_when_it_cannot_restore() {
        let d = TempDir::new();
        let (temp, backup) = (d.file("a.rstmp"), d.file("a.rsbak"));
        std::fs::write(&temp, b"new").unwrap();
        std::fs::write(&backup, b"original").unwrap();
        // 元の名前を戻せない (保存先の場所が存在しない)
        let target = d.file("nodir/a.txt");
        let e = io::Error::from_raw_os_error(1177);
        match recover_failed_replace(&target, &temp, &backup, &e) {
            WriteError::Failed(m) => {
                assert!(m.contains(&backup.display().to_string()), "{m}");
                assert!(m.contains(&temp.display().to_string()), "{m}");
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(std::fs::read(&temp).unwrap(), b"new");
        assert_eq!(std::fs::read(&backup).unwrap(), b"original");
        // 保存先も控えもある (想定外): どちらも消さない
        let target = d.file("b.txt");
        std::fs::write(&target, b"something").unwrap();
        assert!(matches!(
            recover_failed_replace(&target, &temp, &backup, &e),
            WriteError::Failed(_)
        ));
        assert!(temp.exists() && backup.exists() && target.exists());
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

    /// 大きさも更新日時も同じで、別のファイルに差し替えられた場合も検出する (Windows)
    #[cfg(windows)]
    #[test]
    fn fingerprint_detects_same_size_same_mtime_replacement() {
        let d = TempDir::new();
        let a = d.file("a.txt");
        let b = d.file("b.txt");
        std::fs::write(&a, b"abc").unwrap();
        std::fs::write(&b, b"xyz").unwrap();
        let fp = fingerprint(&a).unwrap();
        let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&b)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::rename(&b, &a).unwrap(); // a を同じ大きさ・同じ日時の別ファイルに差し替え
        let after = fingerprint(&a).unwrap();
        assert_eq!(after.len, fp.len);
        assert_eq!(after.modified, fp.modified);
        assert!(changed_since(&fp, &a), "ファイルの識別子の違いで検出する");
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
        // UTF-32 は未対応 (UTF-16 としても読める形なので、その選択肢を添える)
        let p = d.file("u32.txt");
        std::fs::write(&p, [0xFF, 0xFE, 0, 0, b'a', 0, 0, 0]).unwrap();
        match load(&p, None) {
            Err(LoadError::Utf32 { name, as_utf16 }) => {
                assert_eq!(name, "UTF-32 LE");
                assert_eq!(as_utf16, Some(Enc::Utf16Le));
            }
            _ => panic!("UTF-32 として断るはず"),
        }
        // 先頭が U+0000 の UTF-16 LE (BOM + NUL + 'A') は UTF-32 ではない
        let p = d.file("u16nul.txt");
        std::fs::write(&p, [0xFF, 0xFE, 0x00, 0x00, 0x41, 0x00]).unwrap();
        let l = load(&p, None).unwrap();
        assert_eq!(l.enc, Enc::Utf16Le);
        assert_eq!(l.text, "\u{0}A");
        // UTF-16 として開く (文字コードを指定)
        let p = d.file("u32b.txt");
        std::fs::write(&p, [0xFF, 0xFE, 0, 0, b'a', 0, 0, 0]).unwrap();
        let l = load(&p, Some(Enc::Utf16Le)).unwrap();
        assert_eq!(l.text, "\u{0}a\u{0}");
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
