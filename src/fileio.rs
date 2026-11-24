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

/// 同じファイルかどうかを比べるためのキー (パスの正規化形)。
///
/// 正規化 (シンボリックリンクや `..` の解決) した絶対パスにする。Windows ではファイル名の
/// 大文字小文字を区別しないので小文字にそろえる。存在しないパスは、そのままのパスを使う。
/// 存在するファイルの同一判定には、これではなく `FileKey` を使う (大文字小文字を区別する
/// フォルダでは、名前が大文字小文字だけ違う別のファイルが並べて存在できるため)
pub fn path_key(path: &Path) -> String {
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// 「同じファイルか」を判定するための目印。
///
/// 存在するファイルは、ファイルの識別子 (ボリュームのシリアル番号 + ファイルのインデックス) で
/// 比べる。パスの書き方 (大文字小文字・`.`・シンボリックリンク) が違っても同じファイルなら
/// 一致し、大文字小文字を区別するフォルダにある別のファイル (`Readme.txt` と `README.txt`) は
/// 一致しない。識別子を取れないとき (Windows 以外・開けない) だけ、パスの正規化形で比べる
#[derive(Clone, Debug)]
pub struct FileKey {
    path: String,
    /// 存在するか (存在しないファイルと存在するファイルは、同じファイルではない)
    exists: bool,
    id: Option<FileId>,
}

impl FileKey {
    pub fn of(path: &Path) -> Self {
        let fp = fingerprint(path);
        FileKey {
            path: path_key(path),
            exists: fp.is_some(),
            id: fp.and_then(|f| f.id),
        }
    }

    pub fn same_as(&self, other: &FileKey) -> bool {
        if self.exists != other.exists {
            return false;
        }
        match (self.id, other.id) {
            (Some(a), Some(b)) => a == b,
            _ => self.path == other.path,
        }
    }
}

/// 一時ファイル・控えの名前に付ける連番 (同じ保存先に同時に書いても衝突しないように)
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 一時ファイル・控えの名前。元のファイル名には依存させない (短い固定長にする)。
///
/// 元の名前に付け足すと、ファイル名の長さの上限 (255 文字) に近い名前のファイルを
/// 開けても保存できなくなる。同じフォルダに置く (同じボリュームでないと置き換えが
/// 原子的にならない) ので、衝突しないよう、プロセス番号と連番を入れる
fn sibling_path(path: &Path, ext: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(".rse-{}-{}.{}", std::process::id(), n, ext))
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

/// Win32 の API に渡すパス (NUL 終端の UTF-16)。
///
/// `\\?\` の付かないパスは、全体で 260 文字 (MAX_PATH) までしか扱えない。Rust の標準ライブラリは
/// 長いパスに自動で `\\?\` を付けるので、そのパスで開いたファイルを、ここでの保存だけが
/// 失敗しないよう、長いパスには同じ接頭辞を付ける (短いパスはそのまま渡す)
#[cfg(windows)]
fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let bs = u16::from(b'\\');
    let mut w: Vec<u16> = p.as_os_str().encode_wide().collect();
    if w.len() >= 240 && !w.starts_with(&[bs, bs, u16::from(b'?'), bs]) {
        if let Ok(abs) = std::path::absolute(p) {
            let v: Vec<u16> = abs.as_os_str().encode_wide().collect();
            let mut out: Vec<u16> = Vec::with_capacity(v.len() + 8);
            if v.starts_with(&[bs, bs]) {
                // UNC (\\server\share\...) は \\?\UNC\server\share\...
                out.extend(r"\\?\UNC\".encode_utf16());
                out.extend_from_slice(&v[2..]);
            } else {
                out.extend(r"\\?\".encode_utf16());
                out.extend_from_slice(&v);
            }
            w = out;
        }
    }
    w.push(0);
    w
}

/// 一時ファイル専用のセキュリティ記述子: 現在のユーザーだけにアクセスを許し、
/// 親フォルダからの継承も受けない (保護された DACL)。
///
/// 既存のファイルを上書きするとき、一時ファイルを既定のセキュリティ (フォルダから継承した、
/// 他のユーザーも読める設定) で作ると、元のファイルが個別の ACL で保護されていても、保存中は
/// 新しい内容 (全文) が他のユーザーに読めてしまう (異常終了で残った場合も同じ)。
/// そこで、元のファイル以上に厳しい (自分だけの) 設定で作る。置き換え (`ReplaceFileW`) の
/// ときに、元のファイルの ACL が最終的なファイルへ引き継がれる
#[cfg(windows)]
struct PrivateSecurity {
    // sd の DACL は acl の中を指すので、acl が生きている間だけ sd を使える
    _acl: Vec<u32>,
    sd: windows_sys::Win32::Security::SECURITY_DESCRIPTOR,
}

#[cfg(windows)]
impl PrivateSecurity {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
            InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
            TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, SECURITY_DESCRIPTOR,
            SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
        let fail = io::Error::last_os_error;
        // SAFETY: 以下はすべて Win32 の呼び出し。渡すポインタは、呼び出しの間
        // (SD が指す ACL は、返す値が生きている間) 有効なバッファを指す。
        unsafe {
            let mut token = 0isize;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(fail());
            }
            // 現在のユーザーの SID を得る (必要な大きさを先に問い合わせる)
            let mut need = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut need);
            let mut buf = vec![0u64; (need as usize).div_ceil(8).max(1)];
            let got = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut need,
            );
            let err = fail();
            CloseHandle(token);
            if got == 0 {
                return Err(err);
            }
            let sid = (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid;
            // ACL: ヘッダ + 許可の ACE 1 つ (ACE の末尾の SID の分は GetLengthSid で足す)
            let ace_len = std::mem::size_of::<ACCESS_ALLOWED_ACE>() - std::mem::size_of::<u32>()
                + GetLengthSid(sid) as usize;
            let acl_len = (std::mem::size_of::<ACL>() + ace_len).next_multiple_of(4);
            let mut acl = vec![0u32; acl_len / 4];
            let pacl = acl.as_mut_ptr().cast::<ACL>();
            if InitializeAcl(pacl, acl_len as u32, ACL_REVISION) == 0 {
                return Err(fail());
            }
            if AddAccessAllowedAce(pacl, ACL_REVISION, FILE_ALL_ACCESS, sid) == 0 {
                return Err(fail());
            }
            let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
            let psd = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
            if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) == 0 {
                return Err(fail());
            }
            if SetSecurityDescriptorDacl(psd, 1, pacl, 0) == 0 {
                return Err(fail());
            }
            // 保護: 親フォルダの ACL を継承しない
            if SetSecurityDescriptorControl(psd, SE_DACL_PROTECTED, SE_DACL_PROTECTED) == 0 {
                return Err(fail());
            }
            Ok(PrivateSecurity { _acl: acl, sd })
        }
    }
}

/// 一時ファイルを新しく作って開く (すでにあれば失敗する)。
/// `private` のとき (既存のファイルの上書き) は、自分だけが開ける設定で作る。
/// 設定を作れなければ、既定の設定で書くことはせず失敗させる
#[cfg(windows)]
fn create_temp(temp: &Path, private: bool) -> io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    let name = wide_path(temp);
    let mut sec = if private {
        Some(PrivateSecurity::new().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("一時ファイルのアクセス権を設定できませんでした: {e}"),
            )
        })?)
    } else {
        None
    };
    let mut attrs = sec.as_mut().map(|s| SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut s.sd as *mut windows_sys::Win32::Security::SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    });
    let pattrs = attrs
        .as_mut()
        .map_or(std::ptr::null(), |a| a as *const SECURITY_ATTRIBUTES);
    // SAFETY: name は NUL 終端の UTF-16。pattrs は NULL か、呼び出しの間生きている
    // SECURITY_ATTRIBUTES (その記述子は `sec` が持つバッファを指す)。
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            pattrs,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW が返した、自分だけが持つ有効なハンドル
    Ok(unsafe { std::fs::File::from_raw_handle(h as _) })
}

#[cfg(not(windows))]
fn create_temp(temp: &Path, private: bool) -> io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // 既存のファイルの上書きでは、置き換えまでの間は自分だけが読めるようにする
        if private {
            o.mode(0o600);
        }
    }
    let _ = private;
    o.open(temp)
}

/// 置き換えの結果。`Some(path)` は、置き換えで退避した「元の保存先」を、承知した状態とは
/// 違う内容のまま残した場所 (消さずに、利用者が見つけられる名前で残してある)
type Preserved = Option<PathBuf>;

#[cfg(windows)]
fn replace_file(
    target: &Path,
    temp: &Path,
    backup: &Path,
    target_exists: bool,
    expect: &Expect,
) -> Result<Preserved, WriteError> {
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH,
    };
    let (t, s, b) = (wide_path(target), wide_path(temp), wide_path(backup));
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
            #[cfg(test)]
            test_hooks::run_after_replace(target);
            // 控え (b) は、置き換えられた (元の) ファイルそのもの。最後の確認から置き換えまでの
            // 間に他のプログラムが新しい版を書いていたら、それがここへ移っている。
            // 承知した状態と指紋が一致するときだけ捨てる。違う・確かめられないときは消さずに残す
            if backup_matches(backup, expect) {
                let _ = std::fs::remove_file(backup); // 元の内容の控えはもう要らない
                Ok(None)
            } else {
                Ok(Some(preserve_backup(target, backup)))
            }
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
            #[cfg(test)]
            test_hooks::run_after_replace(target);
            return Ok(None);
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
    _expect: &Expect,
) -> Result<Preserved, WriteError> {
    if target_exists {
        match std::fs::rename(temp, target) {
            Ok(()) => {
                #[cfg(test)]
                test_hooks::run_after_replace(target);
                Ok(None)
            }
            Err(e) => Err(recover_failed_replace(target, temp, backup, &e)),
        }
    } else {
        // hard_link は、すでにある名前を上書きしない
        let r = std::fs::hard_link(temp, target);
        remove_own_temp(temp);
        match r {
            Ok(()) => {
                #[cfg(test)]
                test_hooks::run_after_replace(target);
                Ok(None)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(WriteError::Conflict),
            Err(e) => Err(failed(e)),
        }
    }
}


/// 置き換えで退避した元のファイル (控え) が、承知した状態と同じか。
/// 指紋を取れない (確かめられない) ときは、同じとは扱わない
#[cfg_attr(not(windows), allow(dead_code))]
fn backup_matches(backup: &Path, expect: &Expect) -> bool {
    match expect {
        #[cfg(test)]
        Expect::Any => true,
        Expect::Is(fp) => fp.is_some() && fingerprint(backup) == *fp,
    }
}

/// 退避名に入れる現在時刻 (`yyyyMMdd-HHmmss`、ローカル時刻)
#[cfg(windows)]
fn local_timestamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: 書き込み先の構造体 (ゼロ初期化) を渡すだけ。
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

#[cfg(not(windows))]
fn local_timestamp() -> String {
    let s = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("{s}")
}

/// ファイル名の長さの上限 (255) に収まるか (Windows は UTF-16 の単位数、それ以外はバイト数)
fn name_fits(name: &str) -> bool {
    if cfg!(windows) {
        name.encode_utf16().count() <= 255
    } else {
        name.len() <= 255
    }
}

/// 退避したファイルの、見つけやすい名前: `<元の名前> (RsEditor 退避 yyyyMMdd-HHmmss).<拡張子>`。
/// 255 文字の上限を超えるときは、元の名前の部分を切り詰める
fn preserved_name(target: &Path, stamp: &str, serial: u32) -> String {
    let mut stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = target
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let more = if serial > 0 { format!(" {serial}") } else { String::new() };
    let tail = format!(" (RsEditor 退避 {stamp}{more}){ext}");
    loop {
        let name = format!("{stem}{tail}");
        if name_fits(&name) || stem.is_empty() {
            return name;
        }
        stem.pop();
    }
}

/// 置き換えで退避した控えを、利用者が見つけられる名前 (隠しでない、保存先の隣) に改めて残す。
/// 名前を変えられなければ、控えの名前のまま残す。残した場所を返す
fn preserve_backup(target: &Path, backup: &Path) -> PathBuf {
    let stamp = local_timestamp();
    for serial in 0..100 {
        let to = target.with_file_name(preserved_name(target, &stamp, serial));
        if exists_at(&to) {
            continue;
        }
        return match std::fs::rename(backup, &to) {
            Ok(()) => to,
            Err(_) => backup.to_path_buf(),
        };
    }
    backup.to_path_buf()
}

/// テスト用の差し込み口: 置き換えの直前・直後に、他のプログラムの書き込みを再現する
#[cfg(test)]
pub mod test_hooks {
    use std::cell::RefCell;
    use std::path::Path;
    type Hook = Box<dyn FnOnce(&Path)>;
    thread_local! {
        static BEFORE: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static AFTER: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }
    /// 最後の確認の後、置き換えの直前に 1 回だけ呼ぶ
    pub fn before_replace(f: impl FnOnce(&Path) + 'static) {
        BEFORE.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }
    /// 置き換えの直後 (保存先の指紋を取る前) に 1 回だけ呼ぶ
    pub fn after_replace(f: impl FnOnce(&Path) + 'static) {
        AFTER.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }
    pub(super) fn run_before_replace(target: &Path) {
        if let Some(f) = BEFORE.with(|h| h.borrow_mut().take()) {
            f(target);
        }
    }
    pub(super) fn run_after_replace(target: &Path) {
        if let Some(f) = AFTER.with(|h| h.borrow_mut().take()) {
            f(target);
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
pub fn atomic_write(path: &Path, bytes: &[u8], expect: &Expect) -> Result<Written, WriteError> {
    let temp = unused_sibling(path, "tmp");
    let backup = unused_sibling(path, "bak");
    atomic_write_at(path, bytes, expect, &temp, &backup)
}

/// 書き込みが成功した後の、保存先の状態
#[derive(Debug, PartialEq, Eq)]
pub enum Written {
    /// 保存先が、いま書いた内容そのものであることを確かめられた。その指紋
    Verified(Fingerprint),
    /// 置き換えの後で保存先を確かめたら、書いたものと一致しなかった (同期ツールなどが
    /// 置き換えの直後に別の版へ差し替えた可能性がある) か、確かめられなかった。
    /// 保存先の指紋は「不明」として扱い、次の保存の前に確認する
    Unverified,
    /// 置き換えの直前に他のプログラムが新しい版を書いていた (退避した元のファイルが、承知した
    /// 状態と一致しなかった)。その版は消さずに `backup` へ残してある。保存先は、書いた内容。
    /// `fp` は、書いた内容と保存先が一致したことを確かめられたときの保存先の指紋
    Preserved {
        backup: PathBuf,
        fp: Option<Fingerprint>,
    },
}

/// `atomic_write` の本体。一時ファイル・控えの名前を指定する。
///
/// 書いた後の確認: 置き換えの前に、一時ファイル自身のハンドルから (内容を同期した後の)
/// 指紋 (大きさ・更新日時・ファイルの識別子) を取っておき、置き換えの後で保存先の指紋と
/// 全項目を比べる。置き換えは一時ファイルそのものを保存先の名前にするので、一致するはず
/// である。保存後にもう一度ファイルを開いて指紋を取り直す方法だと、その間に同期ツールが
/// ファイルを差し替えた場合に、他者の版の指紋を「自分が書いたもの」として覚えてしまう
fn atomic_write_at(
    path: &Path,
    bytes: &[u8],
    expect: &Expect,
    temp: &Path,
    backup: &Path,
) -> Result<Written, WriteError> {
    let existing = std::fs::metadata(path).ok();
    if let Some(m) = &existing {
        if m.permissions().readonly() {
            return Err(WriteError::Failed("ファイルが読み取り専用です".into()));
        }
    }
    // 一時ファイルは、自分で作れたとき (create_new が成功したとき) だけ削除の対象にする。
    // すでに同じ名前のファイルがあった場合は、他のファイルなので触らずに失敗する。
    // 既存のファイルの上書きでは、置き換えまでの間 (と、異常終了で残った場合) に、
    // 新しい内容を他のユーザーが読めないよう、自分だけが開ける設定で作る
    let mut f = create_temp(temp, existing.is_some()).map_err(failed)?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all());
    let wrote = written.is_ok().then(|| Fingerprint::from_file(&f)).flatten();
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
    #[cfg(test)]
    test_hooks::run_before_replace(path);
    let preserved = replace_file(path, temp, backup, existing.is_some(), expect)?;
    let state = match (wrote, fingerprint(path)) {
        (Some(w), Some(now)) if w == now => Some(now),
        _ => None,
    };
    Ok(match (preserved, state) {
        (Some(backup), fp) => Written::Preserved { backup, fp },
        (None, Some(now)) => Written::Verified(now),
        (None, None) => Written::Unverified,
    })
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
                .filter(|n| n.starts_with(".rse-"))
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
        atomic_write(p, bytes, &Expect::Any).map(|_| ())
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
        let r = replace_file(&p, &temp, &backup, false, &Expect::Any);
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

    /// 書いた直後の保存先は、一時ファイルのハンドルから取った指紋と全項目が一致する
    /// (置き換えの後も、大きさ・更新日時・ファイルの識別子が変わらない)
    #[test]
    fn written_state_is_verified_for_new_and_replaced_files() {
        let d = TempDir::new();
        let p = d.file("a.txt");
        // 新規
        let w = atomic_write(&p, b"first", &Expect::Is(None)).unwrap();
        assert_eq!(w, Written::Verified(fingerprint(&p).unwrap()));
        // 置き換え (ReplaceFileW を通る)
        let seen = fingerprint(&p);
        let w = atomic_write(&p, b"second, longer", &Expect::Is(seen)).unwrap();
        assert_eq!(w, Written::Verified(fingerprint(&p).unwrap()));
        assert_eq!(std::fs::read(&p).unwrap(), b"second, longer");
        // 書いた後に他者が差し替えれば、書いたときの指紋とは一致しない
        let mine = fingerprint(&p).unwrap();
        std::fs::write(&p, b"someone else's version").unwrap();
        assert_ne!(fingerprint(&p).unwrap(), mine);
    }

    /// 置き換えの直後に保存先が別の版へ差し替わったら、`Unverified` になる (保存済みとは扱わない)
    #[test]
    fn replaced_target_swapped_right_after_is_unverified() {
        let d = TempDir::new();
        let p = d.file("a.txt");
        std::fs::write(&p, b"original").unwrap();
        let seen = fingerprint(&p);
        test_hooks::after_replace(|t| std::fs::write(t, b"synced by someone else").unwrap());
        let w = atomic_write(&p, b"mine", &Expect::Is(seen)).unwrap();
        assert_eq!(w, Written::Unverified);
        assert!(d.leftovers().is_empty(), "{:?}", d.leftovers());
    }

    /// 最後の確認と置き換えの間に他のプログラムが書いた版は、控えと一緒に消さず、
    /// 見つけやすい名前で残す
    #[test]
    fn version_written_between_check_and_replace_is_preserved() {
        let d = TempDir::new();
        let p = d.file("a.txt");
        std::fs::write(&p, b"original").unwrap();
        let seen = fingerprint(&p);
        test_hooks::before_replace(|t| std::fs::write(t, b"external version").unwrap());
        let w = atomic_write(&p, b"mine", &Expect::Is(seen)).unwrap();
        let Written::Preserved { backup, fp } = w else {
            panic!("退避されるはず: {w:?}");
        };
        assert_eq!(std::fs::read(&p).unwrap(), b"mine");
        assert_eq!(std::fs::read(&backup).unwrap(), b"external version");
        assert_eq!(backup.parent(), p.parent());
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("a (RsEditor 退避 ") && name.ends_with(").txt"), "{name}");
        assert!(!name.starts_with('.'), "隠しファイルにしない: {name}");
        assert_eq!(fp, fingerprint(&p), "保存先は書いた内容");
        assert!(d.leftovers().is_empty(), "{:?}", d.leftovers());
        // 2 回目 (同じ秒でも別の名前になる)
        let seen = fingerprint(&p);
        test_hooks::before_replace(|t| std::fs::write(t, b"external 2").unwrap());
        let Written::Preserved { backup: b2, .. } =
            atomic_write(&p, b"mine 2", &Expect::Is(seen)).unwrap()
        else {
            panic!()
        };
        assert_ne!(b2, backup);
        assert_eq!(std::fs::read(&backup).unwrap(), b"external version");
        assert_eq!(std::fs::read(&b2).unwrap(), b"external 2");
    }

    /// 退避の名前は 255 文字に収まる (元の名前を切り詰める)
    #[test]
    fn preserved_name_stays_within_limit() {
        let long = format!("{}.txt", "あ".repeat(240));
        let n = preserved_name(Path::new(&long), "20260101-000000", 0);
        assert!(name_fits(&n), "{}", n.encode_utf16().count());
        assert!(n.ends_with(" (RsEditor 退避 20260101-000000).txt"));
        let short = preserved_name(Path::new("memo.md"), "20260101-000000", 2);
        assert_eq!(short, "memo (RsEditor 退避 20260101-000000 2).md");
    }

    /// 255 文字に近い長いファイル名でも保存できる (一時ファイル・控えの名前は元の名前に依存しない)
    #[test]
    fn long_file_name_can_be_saved() {
        let d = TempDir::new();
        let name = format!("{}.txt", "a".repeat(246));
        assert_eq!(name.len(), 250);
        let p = d.file(&name);
        write(&p, b"first").unwrap(); // 新規
        write(&p, b"second").unwrap(); // 置き換え
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
        assert!(d.leftovers().is_empty(), "{:?}", d.leftovers());
        // 一時ファイル・控えの名前は短く固定長
        let t = sibling_path(&p, "tmp");
        assert!(t.file_name().unwrap().len() < 40, "{t:?}");
    }

    /// 存在するファイルは識別子で比べる。同じファイルなら書き方が違っても一致し、
    /// 別のファイルは一致しない。存在しないものは、存在するものとは別 (パスだけで比べる)
    #[test]
    fn file_key_compares_identity() {
        let d = TempDir::new();
        let a = d.file("Case.txt");
        let b = d.file("other.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap(); // 内容・大きさが同じでも別のファイル
        let ka = FileKey::of(&a);
        assert!(!ka.same_as(&FileKey::of(&b)));
        assert!(ka.same_as(&FileKey::of(&d.0.join(".").join("Case.txt"))));
        #[cfg(windows)]
        {
            assert!(ka.id.is_some(), "Windows では識別子を取れる");
            assert!(ka.same_as(&FileKey::of(&d.file("CASE.TXT"))));
            // 識別子が違えば、パスが (大文字小文字を除いて) 同じでも別のファイルとして扱う
            // (大文字小文字を区別するフォルダの Readme.txt と README.txt を想定)
            let mut other = FileKey::of(&b);
            other.path = ka.path.clone();
            assert!(!ka.same_as(&other));
        }
        // 存在しないパス同士は、パスで比べる。存在するものとは別
        let (m1, m2) = (FileKey::of(&d.file("nope.txt")), FileKey::of(&d.file("NOPE.txt")));
        assert_eq!(m1.same_as(&m2), cfg!(windows));
        assert!(!m1.same_as(&ka));
        assert!(m1.same_as(&FileKey::of(&d.file("nope.txt"))));
    }

    /// Windows: 既存のファイルを上書きするときの一時ファイルは、自分だけが開ける保護された
    /// DACL で作られる。置き換えの後は、元のファイルの ACL が引き継がれる
    #[cfg(windows)]
    mod acl {
        use super::*;
        use windows_sys::Win32::Security::{
            AclSizeInformation, GetAce, GetAclInformation, GetFileSecurityW, GetLengthSid,
            GetSecurityDescriptorControl, GetSecurityDescriptorDacl, ACCESS_ALLOWED_ACE,
            ACE_HEADER, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        };

        /// (DACL が保護されているか, 各 ACE の (種類, マスク, SID のバイト列))
        pub(super) fn dacl(path: &Path) -> (bool, Vec<(u8, u32, Vec<u8>)>) {
            let w = wide_path(path);
            // SAFETY: バッファの大きさは API に伝えたとおり。得たポインタはバッファの中を指す。
            unsafe {
                let mut need = 0u32;
                GetFileSecurityW(
                    w.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    0,
                    &mut need,
                );
                let mut buf = vec![0u64; (need as usize).div_ceil(8).max(1)];
                let ok = GetFileSecurityW(
                    w.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    &mut need,
                );
                assert_ne!(ok, 0, "{}", io::Error::last_os_error());
                let sd = buf.as_mut_ptr().cast();
                let (mut control, mut rev) = (0u16, 0u32);
                assert_ne!(GetSecurityDescriptorControl(sd, &mut control, &mut rev), 0);
                let (mut present, mut defaulted) = (0i32, 0i32);
                let mut acl: *mut ACL = std::ptr::null_mut();
                assert_ne!(
                    GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted),
                    0
                );
                let mut aces = Vec::new();
                if present != 0 && !acl.is_null() {
                    let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
                    assert_ne!(
                        GetAclInformation(
                            acl,
                            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                            AclSizeInformation,
                        ),
                        0
                    );
                    for i in 0..info.AceCount {
                        let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                        assert_ne!(GetAce(acl, i, &mut ace), 0);
                        let hdr = *(ace as *const ACE_HEADER);
                        // 許可 (0) / 拒否 (1) の ACE は、ヘッダ・マスク・SID の順に並ぶ
                        let a = ace as *const ACCESS_ALLOWED_ACE;
                        let sid = std::ptr::addr_of!((*a).SidStart) as *mut std::ffi::c_void;
                        let len = GetLengthSid(sid) as usize;
                        let bytes = std::slice::from_raw_parts(sid as *const u8, len).to_vec();
                        aces.push((hdr.AceType, (*a).Mask, bytes));
                    }
                }
                (control & SE_DACL_PROTECTED != 0, aces)
            }
        }

        #[test]
        fn temp_file_for_existing_target_is_private() {
            let d = TempDir::new();
            let t = d.file(".rse-1-1.tmp");
            drop(create_temp(&t, true).unwrap());
            let (protected, aces) = dacl(&t);
            assert!(protected, "親フォルダの ACL を継承しない");
            assert_eq!(aces.len(), 1, "許可は 1 件だけ: {aces:?}");
            assert_eq!(aces[0].0, 0, "許可の ACE");
            // 新規ファイル用 (private でない) は既定のまま (フォルダから継承する)
            let u = d.file(".rse-1-2.tmp");
            drop(create_temp(&u, false).unwrap());
            assert!(!dacl(&u).0);
            // すでにあれば失敗する (他のファイルを開かない)
            assert!(create_temp(&t, true).is_err());
        }

        #[test]
        fn replace_keeps_original_acl_and_cleans_up() {
            let d = TempDir::new();
            let p = d.file("a.txt");
            std::fs::write(&p, b"original").unwrap();
            // 元のファイルに、個別の ACE (Everyone の読み取り) を足す
            let out = std::process::Command::new("icacls")
                .arg(&p)
                .args(["/grant", "*S-1-1-0:(R)"])
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
            let before = dacl(&p);
            assert!(before.1.len() >= 2, "{before:?}");
            write(&p, b"new content").unwrap();
            assert_eq!(std::fs::read(&p).unwrap(), b"new content");
            assert_eq!(dacl(&p), before, "元のファイルの ACL が引き継がれる");
            assert!(d.leftovers().is_empty(), "{:?}", d.leftovers());
        }
    }
}
