// リリースビルドではコンソールウィンドウを表示しない
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encoding;
mod fileio;
mod ime;
mod search;

use eframe::egui;
use egui::text::{CCursor, CCursorRange};
use egui::widgets::text_edit::TextEditState;
use egui::{Key, KeyboardShortcut, Modifiers, ViewportCommand};
use encoding::{Enc, Eol};
use fileio::Fingerprint;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Weak};

const APP_NAME: &str = "RsEditor";
/// Cargo.toml の version (例: "1.0.0")
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 行番号の欄の右側の余白 (px)
const GUTTER_PAD: f32 = 12.0;

/// 検索一致の背景色 (通常の一致, 現在の一致)。半透明なので文字色の上でも読める
fn match_colors(dark: bool) -> (egui::Color32, egui::Color32) {
    let current = egui::Color32::from_rgba_unmultiplied(255, 140, 0, 200);
    if dark {
        (
            egui::Color32::from_rgba_unmultiplied(200, 170, 0, 70),
            current,
        )
    } else {
        (
            egui::Color32::from_rgba_unmultiplied(255, 230, 0, 120),
            current,
        )
    }
}

/// これより大きい文書は、保存内容との完全な比較 (ハッシュ) をしない (入力のたびに全文を走査して
/// 重くならないように)。その場合は、編集したら変更あり (dirty) のままにする
const EXACT_DIRTY_LIMIT: usize = 20 * 1024 * 1024;

/// 保存 (または読み込み) した時点の内容の要約。元の内容に戻したとき (Undo など) に
/// 「変更あり」を解除するために使う
#[derive(Clone, PartialEq)]
struct Snapshot {
    len: usize,
    hash: u64,
    enc: Enc,
    eol: Eol,
    /// ハッシュを取ったか (大きな文書では取らない)
    exact: bool,
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl Snapshot {
    fn of(text: &str, enc: Enc, eol: Eol) -> Self {
        let exact = text.len() <= EXACT_DIRTY_LIMIT;
        Snapshot {
            len: text.len(),
            hash: if exact { hash_text(text) } else { 0 },
            enc,
            eol,
            exact,
        }
    }
}

/// 文書の版ごとに数え直す値のキャッシュ
struct DocStats {
    rev: u64,
    chars: usize,
    lines: usize,
}

/// galley の各表示行が属する論理行の番号 (1 始まり)。行番号を、見えている行からだけ描くために使う。
///
/// galley そのものは持たない (egui が文字の描画用の画像 (フォントアトラス) を作り直したときに、
/// 古い galley を使い回すと文字が崩れるため。galley は毎フレーム egui の
/// キャッシュ (`Fonts::layout_job`) から受け取る)。どの galley の結果かを `Weak` で覚え、
/// 同じ galley のときだけ使い回す (Weak は確保領域を生かすので、ポインタの再利用で
/// 別の galley と取り違えることはない)
struct RowLines {
    galley: Weak<egui::Galley>,
    lines: Vec<u32>,
}

/// 各表示行の論理行番号。`ends_with_newline` は各表示行が改行で終わるか
fn row_line_numbers(ends_with_newline: impl Iterator<Item = bool>) -> Vec<u32> {
    let mut line = 1u32;
    ends_with_newline
        .map(|nl| {
            let n = line;
            if nl {
                line += 1;
            }
            n
        })
        .collect()
}

/// 画面の上端 `top` より下にかかる最初の表示行の番号 (二分探索)。
/// `galley_pos_y` は galley の左上の画面上の y 座標
fn first_visible_row(galley: &egui::Galley, galley_pos_y: f32, top: f32) -> usize {
    galley
        .rows
        .partition_point(|r| galley_pos_y + r.rect.max.y < top)
}

/// 検索結果のキャッシュ。検索語・検索条件・文書の版が変わったときだけ作り直す。
/// 一致は先頭から `search::MAX_STORED_MATCHES` 件までしか覚えない (表示の強調と件数表示用)
struct FindCache {
    rev: u64,
    query: String,
    case_sensitive: bool,
    /// 一致箇所 (文字位置の範囲)
    matches: Vec<(usize, usize)>,
    /// 一致箇所 (バイト位置の範囲)。表示の強調に使う
    byte_ranges: Vec<(usize, usize)>,
    /// 上限を超える一致が、まだ後ろにあるか
    capped: bool,
}

/// このフレームの、検索の現在位置 (件数の表示と、現在の一致の強調で共有する。
/// 選択範囲から 1 フレームに 1 回だけ求める)
#[derive(Clone, Copy, Default)]
struct FindPos {
    /// 選択がちょうど一致箇所と同じとき、その一致の番号
    exact: Option<usize>,
    /// 選択の後ろの次の一致 (選択が一致箇所そのものならその番号)
    current: Option<usize>,
}

/// 1つのタブ (= 1つの文書)
struct Doc {
    id: u64,
    path: Option<PathBuf>,
    untitled_no: usize,
    text: String,
    enc: Enc,
    eol: Eol,
    dirty: bool,
    /// 読み込み時にデコードできないバイトがあった (U+FFFD に置き換わっている) か。
    /// そのまま上書き保存すると、元の不正なバイトが失われる
    lossy_load: bool,
    /// 保存・読み込みした時点の内容。`dirty` の判定に使う
    saved: Snapshot,
    /// 保存・読み込みした時点のファイルの指紋。外部での変更の検出に使う
    fingerprint: Option<Fingerprint>,
    /// 保存した直後に、保存先が書いた内容と一致するか確かめられなかった
    /// (`fileio::Written::Unverified`)。指紋が当てにならないので、次の保存の前に確認する
    disk_unverified: bool,
    cursor_idx: usize,
    line_col: (usize, usize),
    /// 文字列を変更するたびに増やす版番号 (各種キャッシュの判定に使う)
    rev: u64,
    /// 変更の世代。本文の編集のほか、保存・保存先の変更・文字コード/改行コード/BOM の変更でも
    /// 増える。バックグラウンドの読み込みの結果を取り込んでよいか (読み込み中に文書が
    /// 変わっていないか) の判定に使う
    generation: u64,
    /// 読み込み時に改行コードが混在していて、まだ保存して統一していないか
    mixed_eol: bool,
    find_cache: Option<FindCache>,
    stats: Option<DocStats>,
    row_lines: Option<RowLines>,
}

impl Doc {
    /// 空の文書
    fn blank(id: u64, untitled_no: usize) -> Self {
        Doc {
            id,
            path: None,
            untitled_no,
            text: String::new(),
            enc: Enc::Utf8,
            eol: Eol::CrLf,
            dirty: false,
            lossy_load: false,
            saved: Snapshot::of("", Enc::Utf8, Eol::CrLf),
            fingerprint: None,
            disk_unverified: false,
            cursor_idx: 0,
            line_col: (1, 1),
            rev: 0,
            generation: 0,
            mixed_eol: false,
            find_cache: None,
            stats: None,
            row_lines: None,
        }
    }

    /// 読み込んだ内容から文書を作る
    fn from_loaded(id: u64, path: PathBuf, l: fileio::Loaded) -> Self {
        let mut d = Doc::blank(id, 0);
        d.path = Some(path);
        d.enc = l.enc;
        d.eol = l.eol;
        d.text = l.text;
        d.lossy_load = l.lossy;
        d.mixed_eol = l.mixed_eol;
        d.fingerprint = l.fingerprint;
        d.mark_saved();
        d
    }

    /// 本文を変更したとき (版と世代を進める)
    fn edited(&mut self) {
        self.rev += 1;
        self.generation += 1;
    }

    /// 本文以外 (保存・保存先・文字コード・改行コード) を変更したとき (世代だけ進める)
    fn meta_changed(&mut self) {
        self.generation += 1;
    }

    fn title(&self) -> String {
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            None => format!("無題 {}", self.untitled_no),
        }
    }

    /// 閉じる・終了・保存の前に「未保存の内容がある」と扱うか。保存先を確かめられなかった
    /// 文書 (`disk_unverified`) も含める。確認の判定は、すべてこれで行う
    fn has_unsaved(&self) -> bool {
        self.dirty || self.disk_unverified
    }

    /// いまの内容を「保存済み」として記録する
    fn mark_saved(&mut self) {
        self.saved = Snapshot::of(&self.text, self.enc, self.eol);
        self.dirty = false;
    }

    /// 内容・文字コード・改行コードを変えた後に、保存済みの内容と比べて変更あり (dirty) を更新する。
    /// 元に戻した (Undo、改行コードを戻すなど) ときは変更なしに戻る。
    /// 大きな文書では完全な比較をせず、変更ありのままにする。
    /// 長さが違えばハッシュは取らない (たいていの編集は長さが変わるので軽い)
    fn update_dirty(&mut self) {
        let s = &self.saved;
        self.dirty = if self.disk_unverified {
            // 保存の直後に保存先を確かめられなかった文書は、保存済みの版へ戻しても、
            // いまの内容がディスクにあるとは言えない。変更なしに戻さない
            true
        } else if self.enc != s.enc || self.eol != s.eol {
            true
        } else if !s.exact || self.text.len() > EXACT_DIRTY_LIMIT || self.text.len() != s.len {
            true
        } else {
            hash_text(&self.text) != s.hash
        };
    }

    /// (文字数, 行数)。版が変わったときだけ数え直す
    fn stats(&mut self) -> (usize, usize) {
        if !matches!(&self.stats, Some(s) if s.rev == self.rev) {
            let bytes = self.text.as_bytes();
            // UTF-8 の先頭バイト (継続バイト 10xxxxxx 以外) の数 = 文字数
            let chars = bytes.iter().filter(|&&b| (b & 0xC0) != 0x80).count();
            let lines = bytes.iter().filter(|&&b| b == b'\n').count() + 1;
            self.stats = Some(DocStats {
                rev: self.rev,
                chars,
                lines,
            });
        }
        let s = self.stats.as_ref().unwrap();
        (s.chars, s.lines)
    }

    /// 検索結果が古ければ作り直す
    fn ensure_matches(&mut self, query: &str, case_sensitive: bool) {
        let fresh = matches!(
            &self.find_cache,
            Some(c) if c.rev == self.rev && c.case_sensitive == case_sensitive && c.query == query
        );
        if !fresh {
            let (byte_ranges, capped) = search::find_bytes_limited(
                &self.text,
                query,
                case_sensitive,
                search::MAX_STORED_MATCHES,
            );
            let matches = search::bytes_to_chars(&self.text, &byte_ranges);
            self.find_cache = Some(FindCache {
                rev: self.rev,
                query: query.to_owned(),
                case_sensitive,
                matches,
                byte_ranges,
                capped,
            });
        }
    }

    /// 現在の一致一覧 (ensure_matches を呼んでいなければ空)
    fn matches(&self) -> &[(usize, usize)] {
        self.find_cache
            .as_ref()
            .map(|c| c.matches.as_slice())
            .unwrap_or(&[])
    }
}

/// 検索・置換バーの状態
#[derive(Default)]
struct FindState {
    open: bool,
    /// 置換の行を表示するか
    show_replace: bool,
    query: String,
    replacement: String,
    case_sensitive: bool,
    /// 次の描画で検索欄にフォーカスを移すか
    focus_query: bool,
}

/// 文書ごとの編集欄の id
fn editor_id(doc_id: u64) -> egui::Id {
    egui::Id::new(("editor", doc_id))
}

/// 編集欄の選択範囲 (文字位置, 昇順)
fn editor_sel(ctx: &egui::Context, id: egui::Id) -> Option<(usize, usize)> {
    let range = TextEditState::load(ctx, id)?.cursor.char_range()?;
    let [a, b] = range.sorted();
    Some((a.index, b.index))
}

/// 編集欄の選択範囲を設定する (次の描画から反映される)
fn set_editor_sel(ctx: &egui::Context, id: egui::Id, start: usize, end: usize) {
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(start),
        CCursor::new(end),
    )));
    state.store(ctx, id);
}

/// 本文の大きさに応じた、Undo 履歴の最大件数。
///
/// 履歴は各時点の本文を丸ごと持つので、大きな文書では件数を絞らないとメモリを使い尽くす。
/// egui 0.29 の `TextEditState::undoer()` は履歴全体を複製して返し、複製を避ける API が
/// 無い (複製の確保の失敗も捕まえられない)。そこで、複製されても困らない大きさに収まるよう、
/// 文書の大きさに反比例して件数を減らす (全体で約 256 MiB、最小 2 件、最大 100 件)。
fn undo_limit(text_len: usize) -> usize {
    #[cfg(not(test))]
    const BUDGET: usize = 256 * 1024 * 1024;
    // テストでは、小さな文書で件数が絞られる状況を作れるよう、予算を差し替えられる
    #[cfg(test)]
    let budget = TEST_UNDO_BUDGET.with(|b| b.get()).unwrap_or(256 * 1024 * 1024);
    #[cfg(not(test))]
    let budget = BUDGET;
    (budget / text_len.max(1)).clamp(2, 100)
}

#[cfg(test)]
thread_local! {
    static TEST_UNDO_BUDGET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

type Undoer = egui::util::undoer::Undoer<(CCursorRange, String)>;

fn bounded_undoer(limit: usize) -> Undoer {
    Undoer::with_settings(egui::util::undoer::Settings {
        max_undos: limit,
        ..Default::default()
    })
}

/// 編集欄の Undo 履歴に設定している件数の上限 (egui の履歴からは読み出せないので、
/// 設定したときに記録しておく)。記録が無ければ既定の 100
#[derive(Clone, Copy)]
struct UndoLimit(usize);

fn recorded_undo_limit(ctx: &egui::Context, id: egui::Id) -> usize {
    ctx.data(|d| d.get_temp::<UndoLimit>(id)).map_or(100, |l| l.0)
}

/// `replace_text_with_undo` の、失敗しうる準備 (複製・予約) をすべて済ませたもの。
/// `apply` は失敗しない。保存のように「ディスクへ書いた後で画面を更新する」処理が、
/// 書いた後に失敗して画面とファイルが食い違うことのないよう、準備だけを先にできるようにしてある
struct PreparedReplace {
    new_text: String,
    undoer: Undoer,
    limit: usize,
    range: CCursorRange,
}

/// 置換後の本文と Undo 履歴を用意する。メモリが足りずに複製できないときは `None`
/// (何も変えない)
fn prepare_replace(
    ctx: &egui::Context,
    id: egui::Id,
    text: &str,
    new_text: String,
    new_sel: (usize, usize),
) -> Option<PreparedReplace> {
    let (old_copy, new_copy) = (try_clone(text)?, try_clone(&new_text)?);
    let state = TextEditState::load(ctx, id).unwrap_or_default();
    let old_range = state
        .cursor
        .char_range()
        .unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));
    let new_range = CCursorRange::two(CCursor::new(new_sel.0), CCursor::new(new_sel.1));
    // 大きな文書では、履歴の件数を絞った新しい履歴にする (それ以前の履歴は捨てる)
    let want = undo_limit(text.len().max(new_text.len()));
    let recorded = recorded_undo_limit(ctx, id);
    let (mut undoer, limit) = if want < recorded {
        (bounded_undoer(want), want)
    } else {
        (state.undoer(), recorded)
    };
    undoer.add_undo(&(old_range, old_copy));
    let new_state = (new_range, new_copy);
    // 新しい編集なので、Undo 後に残っている Redo を捨てる。egui 0.29 の Undoer には
    // Redo を消す API が無いが、最新の履歴と異なる状態で redo() を呼ぶと Redo が消える
    let _ = undoer.redo(&new_state);
    undoer.add_undo(&new_state);
    Some(PreparedReplace {
        new_text,
        undoer,
        limit,
        range: new_range,
    })
}

impl PreparedReplace {
    /// 本文・履歴・キャレットを置き換える (新しい確保をしない)
    fn apply(self, ctx: &egui::Context, id: egui::Id, text: &mut String) {
        let mut state = TextEditState::load(ctx, id).unwrap_or_default();
        *text = self.new_text;
        state.set_undoer(self.undoer);
        state.cursor.set_char_range(Some(self.range));
        state.store(ctx, id);
        ctx.data_mut(|d| d.insert_temp(id, UndoLimit(self.limit)));
    }
}

/// 置換など、編集欄の外から本文を書き換える。
///
/// egui の Undo 履歴は編集欄にフォーカスがある間しか記録されないため、そのまま書き換えると
/// Ctrl+Z で「置換の直前に入力した文字」まで一緒に消えてしまう。
/// そこで書き換えの前後の状態を明示的に履歴へ積み、Ctrl+Z 1 回で置換 1 回分だけ戻るようにする。
///
/// 履歴のために本文を 2 回複製する。メモリが足りずに複製できないときは (確保の失敗でプロセスごと
/// 落ちて、他のタブの編集を失わないよう) 何も変えずに false を返す。
fn replace_text_with_undo(
    ctx: &egui::Context,
    id: egui::Id,
    text: &mut String,
    new_text: String,
    new_sel: (usize, usize),
) -> bool {
    match prepare_replace(ctx, id, text, new_text, new_sel) {
        Some(p) => {
            p.apply(ctx, id, text);
            true
        }
        None => false,
    }
}

/// 確保に失敗しても落ちない文字列の複製
fn try_clone(s: &str) -> Option<String> {
    let mut v = String::new();
    v.try_reserve_exact(s.len()).ok()?;
    v.push_str(s);
    Some(v)
}

/// 編集欄の Undo 履歴とキャレットを初期状態に戻す (文書を読み直したとき、読み直す前の履歴で
/// 別の内容に戻ってしまわないように)。履歴の件数は、新しい本文の大きさ `text_len` に合わせる
fn reset_editor_state(ctx: &egui::Context, id: egui::Id, text_len: usize) {
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let limit = undo_limit(text_len);
    state.set_undoer(bounded_undoer(limit));
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(0))));
    state.store(ctx, id);
    ctx.data_mut(|d| d.insert_temp(id, UndoLimit(limit)));
}

/// 編集や貼り付けで文書が大きくなり、Undo 履歴の件数の上限 (`undo_limit`) が下の段階に
/// 入ったときに、履歴を新しい上限の履歴へ作り直す (最近の状態は残す)。
///
/// 読み込み時にだけ件数を決めると、空のタブへ大きな文章を貼り付けた文書などが、
/// 既定の 100 件のまま巨大な本文を何十個も覚えてしまう。
/// egui の `Undoer` には履歴を読み出す API が無いので、複製に対して `undo` を繰り返し呼んで
/// 古い状態を順に取り出し、新しい `Undoer` へ古い方から積み直す。段階をまたいだときだけ
/// 呼ぶので (編集のたびではない)、複製のコストは稀にしか掛からない。
/// 現在の状態 (貼り付けの直後など) も取り消し点として積むので、貼り付けは Ctrl+Z で戻せる。
/// 複製できないほどメモリが足りないときは何もしない (次の編集でもう一度試す)
fn shrink_undo_history(ctx: &egui::Context, id: egui::Id, text: &str, want: usize) {
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let range = state
        .cursor
        .char_range()
        .unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));
    let Some(copy) = try_clone(text) else {
        return;
    };
    let current = (range, copy);
    let mut old = state.undoer();
    // 新しい方から、現在の状態を除いて want - 1 件まで取り出す
    let mut recent = Vec::new();
    let mut probe = current.clone();
    while recent.len() + 1 < want {
        let Some(prev) = old.undo(&probe).cloned() else {
            break;
        };
        probe = prev.clone();
        recent.push(prev);
    }
    drop(old);
    let mut undoer = bounded_undoer(want);
    while let Some(st) = recent.pop() {
        undoer.add_undo(&st);
    }
    undoer.add_undo(&current);
    state.set_undoer(undoer);
    state.store(ctx, id);
    ctx.data_mut(|d| d.insert_temp(id, UndoLimit(want)));
}

/// 閉じた文書の編集欄の状態 (Undo 履歴を含む) を、egui の記憶から取り除く。
/// 取り除かないと、閉じたタブの履歴 (本文の複製を含む) がずっと残る
fn forget_editor_state(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|d| {
        d.remove::<TextEditState>(id);
        d.remove::<UndoLimit>(id);
    });
}

/// 現在行の行頭からキャレット (バイト位置 `pos`) までの空白・タブ (= 引き継ぐインデント)。
/// バイト列を後ろへたどるだけで、本文を複製しない
fn line_indent(text: &str, pos: usize) -> &str {
    let b = text.as_bytes();
    let pos = pos.min(b.len());
    let line_start = b[..pos]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |p| p + 1);
    let n = b[line_start..pos]
        .iter()
        .take_while(|&&c| c == b' ' || c == b'\t')
        .count();
    &text[line_start..line_start + n]
}

/// キャレットや選択範囲を動かす入力か (この入力の後ろにある Enter は、動いた後の位置で
/// インデントを求める必要がある)。Enter 以外のキー (矢印・Home/End・PageUp/Down・Ctrl+A など)、
/// マウスのボタン、ドラッグ中のマウスの移動が該当する。
/// `dragging` はこのフレームの終わりにマウスのボタンが押されたままか
fn moves_caret(e: &egui::Event, dragging: bool) -> bool {
    match e {
        egui::Event::Key { pressed: true, .. } | egui::Event::PointerButton { .. } => true,
        egui::Event::PointerMoved(_) => dragging,
        _ => false,
    }
}

/// 本文を書き換える入力か (この入力の後ろにある Enter は、書き換え後の本文で処理する必要がある)
fn mutates_text(e: &egui::Event) -> bool {
    match e {
        egui::Event::Text(_)
        | egui::Event::Paste(_)
        | egui::Event::Cut
        | egui::Event::Ime(egui::ImeEvent::Preedit(_) | egui::ImeEvent::Commit(_)) => true,
        egui::Event::Key {
            key, pressed: true, ..
        } => matches!(
            key,
            Key::Backspace
                | Key::Delete
                | Key::Tab
                | Key::Enter
                | Key::Z
                | Key::Y
                | Key::H
                | Key::K
                | Key::U
                | Key::W
        ),
        _ => false,
    }
}

/// 編集欄にフォーカスがあるとき、Enter (Ctrl/Alt なし。Shift 付きも含む) の押下を、
/// その場で「改行 + 現在行のインデント」の文字入力 (`Event::Text`) に置き換える。
///
/// イベント列の中の位置はそのままなので、同じフレームの他の入力との順序が保たれる。
/// 文字入力として egui の TextEdit が処理するので、Undo/Redo の履歴も egui の通常の入力と
/// 同じに扱われる (改行とインデントは 1 つの入力として一緒に戻る)。
/// IME の確定 Enter は `ImeGuard` が先に取り除くので、ここへは届かない。
///
/// インデントは「このフレームの最初の時点の本文とキャレット」から求める。同じフレームで
/// Enter より前に本文を書き換える入力 (文字入力・貼り付けなど) や、キャレット・選択範囲を
/// 動かす入力 (矢印キー・マウスなど。`moves_caret`) があると、その結果が反映されていない状態で
/// 求めてしまう。そのときは、その Enter と後ろのイベントをこのフレームの入力から外して返す。
/// 呼び出し側は、それを次のフレームの入力の先頭へ戻す (`App::deferred_events`)。
///
/// 持ち越しの本命は `App::raw_input_hook` (`auto_indent_split`) で行う。ここでの持ち越しは、
/// フレームの途中で条件が食い違ったとき (フォーカスを取った直後など) の安全網
fn apply_auto_indent(ctx: &egui::Context, id: egui::Id, text: &str) -> Vec<egui::Event> {
    if !ctx.memory(|m| m.has_focus(id)) {
        return Vec::new();
    }
    let is_enter = is_auto_indent_enter;
    if !ctx.input(|i| i.events.iter().any(is_enter)) {
        return Vec::new();
    }
    let Some(range) = TextEditState::load(ctx, id).and_then(|s| s.cursor.char_range()) else {
        return Vec::new(); // 状態がまだ無いときは egui の通常の改行に任せる
    };
    let caret = search::char_to_byte(text, range.sorted()[0].index);
    let newline = format!("\n{}", line_indent(text, caret));
    let has_indent = newline.len() > 1;
    ctx.input_mut(|i| {
        let dragging = i.pointer.any_down();
        let mut mutated = false;
        let mut defer_from = None;
        for (n, e) in i.events.iter_mut().enumerate() {
            if is_enter(e) {
                if mutated {
                    defer_from = Some(n);
                    break;
                }
                if has_indent {
                    *e = egui::Event::Text(newline.clone());
                }
                mutated = true;
            } else if mutates_text(e) || moves_caret(e, dragging) {
                mutated = true;
            }
        }
        defer_from
            .map(|n| i.events.split_off(n))
            .unwrap_or_default()
    })
}

/// 自動インデントの対象になる Enter (Ctrl/Alt なしの押下) か
fn is_auto_indent_enter(e: &egui::Event) -> bool {
    matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. }
        if !modifiers.ctrl && !modifiers.alt && !modifiers.command && !modifiers.mac_cmd)
}

/// 自動インデントのために次のフレームへ持ち越す入力の位置を、入力の列 (フレームの最初の
/// 状態) から求める。`None` なら持ち越さない。
///
/// 編集欄が処理する入力の順序を保つため、次のように切る:
/// - Enter より前に本文を書き換える・キャレットを動かす入力があれば、その Enter と
///   それ以降のすべて (Ctrl+S・Ctrl+Tab・Ctrl+W などのショートカットも含む)
/// - Enter が先頭の (影響する入力が前に無い) ときは、その Enter の次以降のすべて
///
/// 後者が必要なのは、ショートカットがフレームの先頭で (編集欄が Enter を処理する前に)
/// 消費されるため。同じフレームに残すと、保存・タブの切り替えが改行より先に行われてしまう。
/// `dragging` はこのフレームの終わりにマウスのボタンが押されたままか
fn auto_indent_split(events: &[egui::Event], dragging: bool) -> Option<usize> {
    let mut mutated = false;
    for (n, e) in events.iter().enumerate() {
        if is_auto_indent_enter(e) {
            if mutated {
                return Some(n);
            }
            return (n + 1 < events.len()).then_some(n + 1);
        } else if mutates_text(e) || moves_caret(e, dragging) {
            mutated = true;
        }
    }
    None
}

/// タブ・フォーカスの行き先を変えるショートカット (Ctrl+Tab / Ctrl+Shift+Tab・Ctrl+W・
/// Ctrl+N / T・Ctrl+O・Ctrl+F / H) か。これより後ろの入力の宛先は、このショートカットを
/// 処理した後でないと決まらない
fn changes_target(e: &egui::Event) -> bool {
    matches!(e, egui::Event::Key { key, pressed: true, modifiers, .. }
        if (modifiers.ctrl || modifiers.command || modifiers.mac_cmd)
            && matches!(
                key,
                Key::Tab | Key::W | Key::N | Key::T | Key::O | Key::F | Key::H
            ))
}

/// 行き先を変えるショートカットで切る位置。ショートカットとそれ以降をすべて次のフレームへ
/// 持ち越す (このフレームはそれより前の入力だけを処理する)。ショートカットが先頭なら、
/// それだけをこのフレームで処理し、次以降を持ち越す。`None` なら切らない
///
/// `esc_closes` は、Esc が検索バーを閉じる操作として処理される (検索バーが開いていて、
/// 確認の画面が無い) とき。Esc も、閉じた後でフォーカスが編集欄へ戻るので、切れ目にする
fn target_change_split(events: &[egui::Event], esc_closes: bool) -> Option<(usize, usize)> {
    let is_esc = |e: &egui::Event| {
        esc_closes
            && matches!(e, egui::Event::Key { key: Key::Escape, pressed: true, modifiers, .. }
                if !modifiers.ctrl && !modifiers.alt && !modifiers.shift
                    && !modifiers.command && !modifiers.mac_cmd)
    };
    let t = events.iter().position(|e| changes_target(e) || is_esc(e))?;
    let at = if t > 0 { t } else { t + 1 };
    (at < events.len()).then_some((t, at))
}

/// 持ち越す入力の切る位置と、持ち越す入力に宛先 (いまの文書) があるか。
/// 自動インデントの Enter (`auto_indent_split`) と、行き先を変えるショートカット
/// (`target_change_split`) のうち、先にあるほうで切る。ショートカットのほうが先なら、
/// 持ち越した入力はショートカットの結果の行き先へ届くよう、宛先を持たない
fn input_split(
    events: &[egui::Event],
    dragging: bool,
    esc_closes: bool,
) -> Option<(usize, bool)> {
    let enter = auto_indent_split(events, dragging);
    match (enter, target_change_split(events, esc_closes)) {
        (Some(e), Some((t, at))) if t < e => Some((at, false)),
        (None, Some((_, at))) => Some((at, false)),
        (Some(e), _) => Some((e, true)),
        (None, None) => None,
    }
}

/// 一致箇所の背景を付けた LayoutJob 用のセクションを作る。全体を隙間なく覆うように並べる
fn highlight_sections(
    text: &str,
    font: &egui::FontId,
    color: egui::Color32,
    ranges: &[(usize, usize)],
    current: Option<usize>,
    bg: (egui::Color32, egui::Color32),
) -> Vec<egui::text::LayoutSection> {
    let section = |range: std::ops::Range<usize>, back: egui::Color32| egui::text::LayoutSection {
        leading_space: 0.0,
        byte_range: range,
        format: egui::TextFormat {
            background: back,
            ..egui::TextFormat::simple(font.clone(), color)
        },
    };
    let mut out = Vec::new();
    let mut pos = 0;
    for (i, &(a, b)) in ranges.iter().enumerate() {
        if a < pos || a >= b || b > text.len() {
            continue;
        }
        if a > pos {
            out.push(section(pos..a, egui::Color32::TRANSPARENT));
        }
        out.push(section(a..b, if Some(i) == current { bg.1 } else { bg.0 }));
        pos = b;
    }
    if !out.is_empty() && pos < text.len() {
        out.push(section(pos..text.len(), egui::Color32::TRANSPARENT));
    }
    out
}

/// 変換中 (未確定) の文字範囲 (文字位置, 昇順)。
///
/// egui 0.29 の TextEdit は、Preedit の文字列を本文へ実際に挿入して選択状態にする
/// (次の Preedit で置き換えるため、この選択を保つ必要がある)。そのため変換中は
/// 「選択範囲 = 未確定文字列」になる。egui-winit は winit の文節情報を捨てるので、
/// 文節ごとの区別はできず、未確定文字列全体を 1 つの範囲として扱う。
fn preedit_range(composing: bool, sel: Option<(usize, usize)>) -> Option<(usize, usize)> {
    sel.filter(|&(a, b)| composing && a < b)
}

/// 変換中は通常の選択色を消す (未確定文字列を選択範囲に見せず、下線と淡い背景で示すため)。
/// 戻り値は元の色で、描画後に `selection.bg_fill` へ戻す
fn hide_selection_fill(ui: &mut egui::Ui, composing: bool) -> egui::Color32 {
    let old = ui.visuals().selection.bg_fill;
    if composing {
        ui.visuals_mut().selection.bg_fill = egui::Color32::TRANSPARENT;
    }
    old
}

/// 未確定文字列の各表示行での範囲 (galley 内の座標)。折り返し・複数行にも対応する
fn preedit_rects(galley: &egui::Galley, range: (usize, usize)) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    let mut offset = 0;
    for row in &galley.rows {
        let len = row.char_count_excluding_newline();
        let (a, b) = (range.0.max(offset), range.1.min(offset + len));
        if a < b {
            let (x0, x1) = (row.x_offset(a - offset), row.x_offset(b - offset));
            out.push(egui::Rect::from_min_max(
                egui::pos2(row.rect.min.x + x0, row.rect.min.y),
                egui::pos2(row.rect.min.x + x1, row.rect.max.y),
            ));
        }
        offset += row.char_count_including_newline();
    }
    out
}

/// 未確定文字列に下線と背景を重ねて描く。
///
/// `segs` (IME から取得した文節) があれば文節ごとに描き分ける:
/// 変換対象の文節は太い下線と濃い背景、他の変換済み文節は細い下線、
/// 未変換の入力は細い点線。文節の間には隙間を空ける。
/// `segs` が空のときは未確定文字列全体を 1 つの範囲として描く。
fn paint_preedit(
    ui: &egui::Ui,
    galley: &egui::Galley,
    galley_pos: egui::Pos2,
    range: (usize, usize),
    segs: &[ime::Segment],
    sel_fill: egui::Color32,
    slot: egui::layers::ShapeIdx,
    painter: &egui::Painter,
) {
    use ime::Kind;
    let accent = ui.visuals().selection.stroke.color;
    let text_color = ui.visuals().text_color();
    let off = galley_pos.to_vec2();
    let n = range.1 - range.0;
    // 背景は TextEdit の表示前に確保したスロットへ入れ、本文の下に描く
    let mut fills: Vec<egui::Shape> = Vec::new();
    let set_fills = |fills: Vec<egui::Shape>| painter.set(slot, egui::Shape::Vec(fills));
    if segs.is_empty() || segs.last().map(|s| s.range.end) != Some(n) {
        // 全体の薄い背景 (選択色の半透明)
        let tint = sel_fill.gamma_multiply(0.5);
        for r in preedit_rects(galley, range) {
            let r = r.translate(off);
            fills.push(egui::Shape::rect_filled(r, 0.0, tint));
        }
        set_fills(fills);
        for r in preedit_rects(galley, range) {
            let r = r.translate(off);
            painter.line_segment(
                [
                    egui::pos2(r.min.x, r.max.y - 1.0),
                    egui::pos2(r.max.x, r.max.y - 1.0),
                ],
                egui::Stroke::new(1.5, accent),
            );
        }
        return;
    }
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        let (a, b) = (range.0 + seg.range.start, range.0 + seg.range.end);
        for r in preedit_rects(galley, (a, b)) {
            let r = r.translate(off);
            // 文節の境目が分かるよう、末尾側を 1px 空ける
            let x1 = if last {
                r.max.x
            } else {
                (r.max.x - 1.0).max(r.min.x)
            };
            match seg.kind {
                Kind::Target => {
                    fills.push(egui::Shape::rect_filled(
                        egui::Rect::from_min_max(r.min, egui::pos2(x1, r.max.y)),
                        0.0,
                        sel_fill,
                    ));
                    painter.line_segment(
                        [
                            egui::pos2(r.min.x, r.max.y - 1.0),
                            egui::pos2(x1, r.max.y - 1.0),
                        ],
                        egui::Stroke::new(1.5, accent),
                    );
                }
                Kind::Converted => {
                    painter.line_segment(
                        [
                            egui::pos2(r.min.x, r.max.y - 0.5),
                            egui::pos2(x1, r.max.y - 0.5),
                        ],
                        egui::Stroke::new(1.0, text_color),
                    );
                }
                Kind::Input => {
                    // 点線
                    let stroke = egui::Stroke::new(1.0, text_color);
                    let y = r.max.y - 0.5;
                    let mut x = r.min.x;
                    while x < x1 {
                        painter.line_segment(
                            [egui::pos2(x, y), egui::pos2((x + 2.0).min(x1), y)],
                            stroke,
                        );
                        x += 4.0;
                    }
                }
            }
        }
    }
    set_fills(fills);
}

/// TextEdit の表示前に呼び、変換中の背景を入れるスロットを確保する
/// (後から `paint_preedit` が埋める。本文より前に描かれるので背景は本文の下になる)
fn reserve_preedit_slot(ui: &egui::Ui) -> egui::layers::ShapeIdx {
    ui.painter().add(egui::Shape::Noop)
}

/// TextEdit の出力から、変換中の範囲に下線などを描く
fn paint_preedit_for(
    ui: &egui::Ui,
    out: &egui::text_edit::TextEditOutput,
    composing: bool,
    segs: &[ime::Segment],
    sel_fill: egui::Color32,
    slot: egui::layers::ShapeIdx,
) {
    if !out.response.has_focus() {
        return;
    }
    let sel = out.cursor_range.map(|c| {
        let [a, b] = c.sorted_cursors();
        (a.ccursor.index, b.ccursor.index)
    });
    let range = preedit_range(composing, sel);
    if let Some(r) = range {
        // TextEdit と同じく、欄の中 (text_clip_rect) だけに描く
        let painter = ui
            .painter()
            .with_clip_rect(out.text_clip_rect.intersect(ui.clip_rect()));
        paint_preedit(ui, &out.galley, out.galley_pos, r, segs, sel_fill, slot, &painter);
    }
    set_ime_area(ui, out, range, segs);
}

/// IME の候補ウィンドウを出す位置を、カーソル (変換中は対象の文節の先頭) にする。
///
/// egui-winit 0.29 は IME の位置として `IMEOutput::rect` (TextEdit 全体の矩形) を
/// Windows に渡す。winit はその左上を候補ウィンドウの位置、矩形全体を「避ける範囲」に
/// するため、本文のような大きな欄では候補ウィンドウが欄の外 (画面の端など) に出てしまう。
/// そこで `rect` を 1 文字分の高さの細い矩形に置き換える。
/// 候補ウィンドウと文字の間の余白 (ポイント)
const IME_AREA_GAP: f32 = 4.0;

fn set_ime_area(
    ui: &egui::Ui,
    out: &egui::text_edit::TextEditOutput,
    preedit: Option<(usize, usize)>,
    segs: &[ime::Segment],
) {
    let transform = ui
        .memory(|m| m.layer_transforms.get(&ui.layer_id()).copied())
        .unwrap_or_default();
    let area = preedit.map(|(a, _)| {
        let start = segs
            .iter()
            .find(|s| s.kind == ime::Kind::Target)
            .map_or(a, |s| a + s.range.start);
        // 折り返しの境目では、その文字を実際に描く次の行の位置を使う
        let ccursor = CCursor {
            index: start,
            prefer_next_row: true,
        };
        let r = out
            .galley
            .pos_from_ccursor(ccursor)
            .translate(out.galley_pos.to_vec2());
        // 横スクロールで欄の外に隠れているときは、欄の見えている端に寄せる
        let vis = out.text_clip_rect.intersect(ui.clip_rect());
        let x = if vis.is_positive() {
            r.min.x.clamp(vis.min.x, vis.max.x)
        } else {
            r.min.x
        };
        transform * egui::Rect::from_min_size(egui::pos2(x, r.min.y), egui::vec2(1.0, r.height()))
    });
    ui.ctx().output_mut(|o| {
        if let Some(ime) = &mut o.ime {
            // 候補ウィンドウが文字に密着しないよう、上下に少し余白を足す
            // (候補ウィンドウはこの矩形を避けて、下か上に出る)
            ime.rect = area
                .unwrap_or(ime.cursor_rect)
                .expand2(egui::vec2(0.0, IME_AREA_GAP));
        }
    });
}

/// 1 行の入力欄 (検索・置換) を、変換中の表示付きで出す
/// 検索欄の id
fn find_query_id() -> egui::Id {
    egui::Id::new("find_query")
}

fn singleline_with_preedit(
    ui: &mut egui::Ui,
    text: &mut String,
    id: Option<egui::Id>,
    composing: bool,
    segs: &[ime::Segment],
) -> egui::Response {
    let old = hide_selection_fill(ui, composing);
    // TextEdit は枠 (frame) を自分の show() 冒頭で確保したスロットに描く。そのスロットより
    // 後ろに背景スロットを置くと、枠の不透明な塗りに隠れてしまう。そこで枠は TextEdit に
    // 任せず (frame(false))、枠 → 変換中の背景 → 本文 の順になるよう自前で枠を描く。
    let frame_slot = ui.painter().add(egui::Shape::Noop);
    let fill_slot = reserve_preedit_slot(ui);
    let mut te = egui::TextEdit::singleline(text)
        .desired_width(200.0)
        .frame(false);
    if let Some(id) = id {
        te = te.id(id);
    }
    let out = te.show(ui);
    ui.visuals_mut().selection.bg_fill = old;
    // egui 0.29 の TextEdit 既定の余白 Margin::symmetric(4.0, 2.0) を戻した外側の矩形
    let frame_rect = {
        let visuals = ui.style().interact(&out.response);
        out.response
            .rect
            .expand2(egui::vec2(4.0, 2.0))
            .expand(visuals.expansion)
    };
    let (rounding, stroke) = {
        let visuals = ui.style().interact(&out.response);
        if out.response.has_focus() {
            (visuals.rounding, ui.visuals().selection.stroke)
        } else {
            (visuals.rounding, visuals.bg_stroke)
        }
    };
    ui.painter().set(
        frame_slot,
        egui::epaint::RectShape::new(frame_rect, rounding, ui.visuals().extreme_bg_color, stroke),
    );
    paint_preedit_for(ui, &out, composing, segs, old, fill_slot);
    out.response
}

/// 保存の後に続けてやること。確認の画面をはさんで保存が中断しても、確認後に元の操作を再開する
#[derive(Clone, Copy, PartialEq, Debug)]
enum Then {
    /// 保存だけ
    None,
    /// 保存できたらそのタブ (文書の id) を閉じる
    Close(u64),
    /// 保存できたら、残りの未保存の文書の保存を続け、すべて終わったら終了する
    SaveAll,
}

/// 保存の途中でユーザーが「それでよい」と答えた確認 (同じ確認を繰り返さないため)
#[derive(Clone, Default, Debug, PartialEq)]
struct Confirmed {
    /// 読み込み時にデコードできなかったバイトが失われること
    lossy_load: bool,
    /// ファイルが外部で変更されていること。「上書きする」と答えたときに見せた状態の指紋を
    /// 覚えておく (`Some(None)` は「存在しない」状態)。単なる真偽値にすると、承知した後に
    /// 別の確認 (文字コードの警告など) を出している間にファイルがもう一度変わっても、
    /// 気づかずに上書きしてしまう。保存の直前に、この指紋のとおりかをもう一度確かめる
    conflict: Option<Option<Fingerprint>>,
    /// 文字コードで表現できない文字が変わること
    lossy: bool,
    /// 保存先を選んだ (保存を始めた) 時点で、保存先が「こうなっているはず」と承知した状態
    /// (`None` はまだ決めていない。`Some(None)` は「存在しない」)。確認の画面をいくつはさんでも
    /// 持ち回り、保存の直前にこのとおりかを確かめる。確認のたびに「いま見えている状態」を
    /// 取り直すと、確認の待ち時間の間に他のプログラムが作った・変えたファイルを、
    /// 気づかずに承知したことにして上書きしてしまう。更新してよいのは、利用者が
    /// 「上書きする」(`conflict`) と答えたときだけ
    baseline: Option<Option<Fingerprint>>,
}

#[derive(Clone)]
enum Modal {
    ConfirmClose(u64),
    ConfirmExit,
    /// 文字コードで表現できない (または別の文字に変わる) 文字がある
    Lossy {
        id: u64,
        path: PathBuf,
        then: Then,
        ok: Confirmed,
        /// 変わる文字の例 ("¥ → \" の形)
        examples: Vec<String>,
    },
    /// 読み込み時にデコードできなかったバイトが、上書き保存で失われる
    LossyLoad {
        id: u64,
        path: PathBuf,
        then: Then,
        ok: Confirmed,
    },
    /// 読み込み後に、ファイルが他のプログラムで変更・削除・作成された。
    /// `seen` は、いま見えているファイルの指紋 (`None` は存在しない)。「上書きする」と
    /// 答えたら、この状態を承知したものとして扱う
    Conflict {
        id: u64,
        path: PathBuf,
        then: Then,
        ok: Confirmed,
        seen: Option<Fingerprint>,
    },
    /// 保存した直後に、保存先が書いた内容と一致するか確かめられなかった (他のプログラムが
    /// 直後に書き換えた可能性がある)。文書は変更ありのまま残している。`preserved` は、
    /// 置き換えの直前に他のプログラムが書いていた版を退避した場所 (あれば)
    Unverified {
        id: u64,
        path: PathBuf,
        preserved: Option<PathBuf>,
    },
    /// 未保存の編集を捨てて、指定した文字コードで読み直す
    ConfirmReload { id: u64, enc: Enc },
    /// 大きなファイルを開く確認
    ConfirmLargeOpen { path: PathBuf, size: u64 },
    /// UTF-32 の BOM で始まるファイル (未対応)。UTF-16 としても読めるときは、その選択肢を出す
    Utf32 {
        path: PathBuf,
        name: &'static str,
        as_utf16: Option<Enc>,
    },
    Info(String),
}

enum Action {
    New,
    Open,
    Save,
    SaveAs,
    Select(usize),
    Close(usize),
    CloseActive,
    NextTab,
    PrevTab,
    Exit,
    Reload(Enc),
    Convert(Enc),
    SetEol(Eol),
    SaveAndClose(u64),
    DiscardAndClose(u64),
    SaveAllAndExit,
    ExitNow,
    /// 確認に答えたうえで、保存を続ける
    SaveConfirmed {
        id: u64,
        path: PathBuf,
        then: Then,
        ok: Confirmed,
    },
    /// 確認の画面から「名前を付けて保存」を選んだ
    SaveAsFor(u64, Then),
    /// 未保存の編集を捨てて読み直す
    ReloadNow(u64, Enc),
    /// 大きなファイルを確認のうえ開く
    OpenAnyway(PathBuf),
    /// 文字コードを指定して (新しいタブで) 開く
    OpenAs(PathBuf, Enc),
    DismissModal,
    /// 検索バーを開く (true なら置換の行も表示する)
    OpenFind(bool),
    CloseFind,
    FindNext,
    FindPrev,
    ReplaceOne,
    ReplaceAll,
}

/// IME の変換確定 (Enter) が改行や「次を検索」として二重に処理されるのを防ぐ。
///
/// 原因: Windows の winit は、IME が処理中のキー入力を論理キー `Named(Process)` として
/// 通知する。egui-winit 0.29 はこれを `None` とみなし、物理キー (Enter) へフォールバックして
/// `Event::Key { key: Enter }` を発行してしまう。そのため確定の Enter が
/// `Event::Ime(Commit)` と別に TextEdit へ届き、改行が入る。
/// 対策: 変換中、および確定直後の Enter の押下イベントを入力から取り除く。
#[derive(Default)]
struct ImeGuard {
    /// 変換中 (未確定文字列がある) か
    composing: bool,
    /// 変換中の未確定文字列 (IME から取得した文節情報との照合用)
    preedit: String,
    /// 変換中の文節 (取得できなければ空 = 全体を 1 つの範囲として扱う)
    segs: Vec<ime::Segment>,
    /// 直近の Commit の時刻 (egui の time)
    last_commit: Option<f64>,
    /// 直近の Commit の後に、まだ取り除ける Enter の数 (Commit のたびに 1 に戻る。
    /// 今の変換の間にすでに Enter を取り除いていたときは 0)。
    /// 確定の Enter は 1 回だけなので、猶予の間でも 2 回目以降の Enter は本物の改行として通す
    commit_enter_budget: u32,
    /// 最後の (空でない) Preedit の後に来た「空の Preedit / Disabled」の数。
    /// 最後の Preedit 以降に Commit が来ていれば None
    ends_since_preedit: Option<u32>,
    /// 取り除いた Enter の押下が、まだ離されていない (Enter の離した通知 `pressed: false` が
    /// まだ来ていない) とき、最後に取り除いた (または Commit が来た) 時刻。
    /// 同じ物理的な 1 回の押下が、変換中の Enter と確定後の遅れた Enter の 2 つに分かれて
    /// 届くことがあるため、離されるまで (猶予の間は) 続く Enter の押下も同じものとして取り除く。
    /// 離した通知が来たら、その後の押下は本物の Enter として通す
    enter_held_since: Option<f64>,
    /// 今の変換の間に Enter の押下を取り除いたか
    dropped_enter_in_composition: bool,
}

impl ImeGuard {
    /// 確定の Enter が Commit の前後どちらに来ても取り除くための猶予 (秒)。
    /// Enter の離した通知が届かなかったときも、この時間が過ぎれば Enter を通す
    const GRACE: f64 = 0.05;

    /// 入力イベントを整える (確定の Enter と、偽の確定を取り除く)。
    ///
    /// 偽の確定: winit 0.30 (Windows) は変換の終了 (WM_IME_ENDCOMPOSITION) 時に、IME の
    /// 確定文字列 (GCS_RESULTSTR) が残っていると、それを Commit として送る。一度候補を
    /// 出した後は古い確定文字列が残るため、Backspace で未確定文字列をすべて消すと、
    /// 消したはずの文字が確定として挿入されてしまう。
    /// 本当の確定では、未確定文字列の後に Disabled が 1 つ来てから Commit が来る。
    /// 偽の確定では、未確定文字列を消した通知 (空の Preedit か Disabled) がその前に
    /// もう 1 つ来る。そこで 2 つ以上来た後の Commit は捨て、空の Preedit に置き換える
    /// (TextEdit に未確定文字列が残っていれば、それを消すため)。
    ///
    /// 変換の取り消し: 変換中に Disabled が来たとき (取り消し)、egui は未確定文字列を
    /// 本文へ挿入したまま残してしまう (Disabled は何も消さない)。そこで Disabled の直前に
    /// 空の Preedit を挿入し、egui に未確定文字列を消させる。後から本物の Commit が来ても、
    /// egui は消した位置 (カーソルが元の位置と同じ) へ確定文字列を挿入するので影響しない。
    /// この挿入した空の Preedit は、上の偽の確定の判定には数えない。
    fn filter(&mut self, time: f64, events: &mut Vec<egui::Event>) {
        let mut out = Vec::with_capacity(events.len() + 1);
        for e in events.drain(..) {
            match &e {
                // 新しい変換の開始: 前の変換の数え上げを持ち越さない
                egui::Event::Ime(egui::ImeEvent::Enabled) => {
                    self.ends_since_preedit = None;
                    self.dropped_enter_in_composition = false;
                    out.push(e);
                }
                egui::Event::Ime(egui::ImeEvent::Preedit(t)) if !t.is_empty() => {
                    if !self.composing {
                        self.dropped_enter_in_composition = false;
                    }
                    self.ends_since_preedit = Some(0);
                    self.composing = true;
                    self.preedit = t.clone();
                    out.push(e);
                }
                egui::Event::Ime(egui::ImeEvent::Preedit(_)) => {
                    if let Some(n) = &mut self.ends_since_preedit {
                        *n += 1;
                    }
                    self.composing = false;
                    self.preedit.clear();
                    out.push(e);
                }
                egui::Event::Ime(egui::ImeEvent::Disabled) => {
                    if let Some(n) = &mut self.ends_since_preedit {
                        *n += 1;
                    }
                    if self.composing {
                        out.push(egui::Event::Ime(egui::ImeEvent::Preedit(String::new())));
                    }
                    self.composing = false;
                    self.preedit.clear();
                    out.push(e);
                }
                egui::Event::Ime(egui::ImeEvent::Commit(_)) => {
                    let fake = self.ends_since_preedit.is_some_and(|n| n >= 2);
                    self.ends_since_preedit = None;
                    self.composing = false;
                    self.preedit.clear();
                    if fake {
                        out.push(egui::Event::Ime(egui::ImeEvent::Preedit(String::new())));
                    } else {
                        self.last_commit = Some(time);
                        // 今の変換の間に Enter を取り除いていれば、それが確定の Enter。
                        // まだなら、確定の後に遅れて届く Enter を 1 回だけ取り除く
                        self.commit_enter_budget = if self.dropped_enter_in_composition {
                            0
                        } else {
                            1
                        };
                        self.dropped_enter_in_composition = false;
                        if let Some(t) = &mut self.enter_held_since {
                            *t = time; // 猶予は確定からの時間で数える
                        }
                        out.push(e);
                    }
                }
                egui::Event::Key {
                    key: Key::Enter,
                    pressed: false,
                    ..
                } => {
                    // 離された: これ以降の押下は新しい押下。確定の Enter の押下が
                    // アプリに届かないまま (変換の処理に取られて) 離された場合も、
                    // 確定の後の Enter 用に残っている取り除く権利は、ここで使い切ったものとする
                    // (残すと、次の本物の Enter が 1 回だけ取り除かれてしまう)
                    self.enter_held_since = None;
                    self.commit_enter_budget = 0;
                    out.push(e);
                }
                egui::Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    ..
                } => {
                    if self.composing {
                        self.dropped_enter_in_composition = true;
                        self.enter_held_since = Some(time);
                        continue;
                    }
                    // 取り除いた押下がまだ離されていない間 (猶予の間) の Enter は、同じ押下の続き
                    if let Some(t) = self.enter_held_since {
                        if time - t <= Self::GRACE {
                            self.enter_held_since = Some(time);
                            continue;
                        }
                        self.enter_held_since = None;
                    }
                    // 確定の直後 (猶予の間) の Enter は 1 回だけ取り除く。それ以降は本物の Enter
                    let after_commit = self.last_commit.is_some_and(|c| time - c <= Self::GRACE);
                    if after_commit && self.commit_enter_budget > 0 {
                        self.commit_enter_budget -= 1;
                        continue;
                    }
                    out.push(e);
                }
                _ => out.push(e),
            }
        }
        *events = out;
    }
}

/// バックグラウンドの読み込みの種類
enum LoadKind {
    /// 新しいタブで開く
    Open,
    /// 開いている文書を、文字コードを指定して読み直す。
    /// `generation` は読み込みを始めた時点の文書の世代 (`Doc::generation`)。
    /// 読み込み中に文書が変わっていたら、結果は取り込まずに捨てる
    Reload { doc_id: u64, generation: u64 },
}

/// 読み込みを頼んだ時点の、ユーザーの操作の状態。読み込みが終わったときに、
/// その間にユーザーが別のタブへ移った・別の欄へフォーカスを移したのなら、
/// 新しいタブを勝手に選択したり、フォーカスを奪ったりしないために使う
struct LoadOrigin {
    /// 選択していた文書
    active_doc: Option<u64>,
    /// フォーカスのあった欄
    focus: Option<egui::Id>,
    /// 選択していた文書の編集の世代 (`Doc::generation`)。読み込みの間に同じタブへ
    /// 入力を続けていた (世代が進んだ) なら、新しいタブを選択して入力の行き先を奪わない
    active_generation: Option<u64>,
}

/// バックグラウンドのスレッドで実行中の読み込み
struct PendingLoad {
    path: PathBuf,
    /// 同じファイルかどうかの判定用 (`fileio::FileKey`)
    key: fileio::FileKey,
    kind: LoadKind,
    origin: LoadOrigin,
    rx: mpsc::Receiver<Result<fileio::Loaded, fileio::LoadError>>,
}

/// 読み込みの結果を伝える状態表示 (デコードエラー・改行コードの混在)
fn load_notes(lossy: bool, mixed_eol: bool, eol: Eol) -> String {
    let mut s = String::new();
    if lossy {
        s.push_str(" ※デコードできないバイトがありました");
    }
    if mixed_eol {
        s.push_str(&format!(
            " ※改行コードが混在しています。保存時は {} に統一されます",
            eol.label()
        ));
    }
    s
}

struct App {
    docs: Vec<Doc>,
    active: usize,
    next_id: u64,
    untitled_counter: usize,
    /// 表示を待っている確認・お知らせ。先頭を表示し、新しいものは後ろへ足す
    /// (保存の途中の確認が、別の通知で上書きされて続きの操作を失わないように)
    modals: VecDeque<Modal>,
    status: String,
    font_size: f32,
    wrap: bool,
    show_line_numbers: bool,
    /// Enter で現在行のインデントを引き継ぐか
    auto_indent: bool,
    allow_exit: bool,
    ime_guard: ImeGuard,
    focus_editor: bool,
    last_title: String,
    find: FindState,
    /// このフレームの検索の現在位置 (件数の表示と強調で共有する)
    find_pos: FindPos,
    /// 一致箇所を画面内にスクロールする要求 (文書id, 文字位置)。次の描画で処理する
    scroll_req: Option<(u64, usize)>,
    /// バックグラウンドスレッドから描画を促すために使う
    ctx: egui::Context,
    /// 実行中の読み込み
    loads: Vec<PendingLoad>,
    /// タブの列をスクロールして見せ終わった、選択中の文書 (選択が変わったときだけスクロールする)
    tab_shown: Option<u64>,
    /// 自動インデントが次のフレームへ持ち越した入力 (`apply_auto_indent`) と、その入力の宛先の
    /// 文書の id。次のフレームの入力の先頭へ戻す。宛先がある (`Some`) ときは、そのとき宛先の
    /// 文書が選択中・フォーカス中でなければ、別のタブへ入力が流れ込まないよう捨てる。
    /// 宛先が無い (`None`) のは、タブやフォーカスを変えるショートカットの後ろの入力。
    /// 宛先は、そのショートカットを処理した後で決まるので、確かめずに戻す
    deferred_events: Option<(Option<u64>, Vec<egui::Event>)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let mut app = App::blank(cc.egui_ctx.clone());
        // コマンドライン引数 (「プログラムから開く」やexeへのドラッグ&ドロップ) のファイルを開く
        for arg in std::env::args_os().skip(1) {
            app.open_path(PathBuf::from(arg));
        }
        app
    }

    /// 空のタブ 1 つだけの状態
    fn blank(ctx: egui::Context) -> Self {
        let mut app = App {
            docs: Vec::new(),
            active: 0,
            next_id: 1,
            untitled_counter: 0,
            modals: VecDeque::new(),
            status: "準備完了".into(),
            font_size: 15.0,
            wrap: true,
            show_line_numbers: true,
            auto_indent: true,
            allow_exit: false,
            ime_guard: ImeGuard::default(),
            focus_editor: true,
            last_title: String::new(),
            find: FindState::default(),
            find_pos: FindPos::default(),
            scroll_req: None,
            ctx,
            loads: Vec::new(),
            tab_shown: None,
            deferred_events: None,
        };
        app.new_doc();
        app
    }

    /// 表示中 (先頭) の確認・お知らせ
    #[cfg(test)]
    fn modal(&self) -> Option<&Modal> {
        self.modals.front()
    }

    /// 確認・お知らせを待ち行列の後ろへ足す
    fn push_modal(&mut self, m: Modal) {
        self.modals.push_back(m);
    }

    /// 表示中の確認・お知らせを閉じる (次のものがあれば、それが表示される)
    fn pop_modal(&mut self) {
        self.modals.pop_front();
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    /// 同じファイル (`fileio::FileKey` が同じ) を開いているタブ
    fn find_doc_by_key(&self, key: &fileio::FileKey) -> Option<usize> {
        self.docs.iter().position(|d| {
            d.path
                .as_deref()
                .is_some_and(|p| fileio::FileKey::of(p).same_as(key))
        })
    }

    fn new_doc(&mut self) {
        self.untitled_counter += 1;
        let id = self.alloc_id();
        self.docs.push(Doc::blank(id, self.untitled_counter));
        self.active = self.docs.len() - 1;
        self.focus_editor = true;
    }

    fn open_dialog(&mut self) {
        let mut dlg = rfd::FileDialog::new()
            .add_filter(
                "テキストファイル",
                &[
                    "txt", "log", "csv", "md", "ini", "json", "xml", "html", "rs", "c", "h", "cpp",
                    "py", "js", "ts",
                ],
            )
            .add_filter("すべてのファイル", &["*"]);
        if let Some(dir) = self
            .docs
            .get(self.active)
            .and_then(|d| d.path.as_ref())
            .and_then(|p| p.parent())
        {
            dlg = dlg.set_directory(dir);
        }
        if let Some(paths) = dlg.pick_files() {
            for p in paths {
                self.open_path(p);
            }
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        self.open_path_checked(path, false, None);
    }

    /// ファイルを開く。既に開いている (または読み込み中の) ファイルなら、そのタブに切り替えるだけ。
    /// 大きなファイルは `confirmed_large` でなければ、先に確認する。
    /// `forced` を指定すると、その文字コードで読む
    fn open_path_checked(&mut self, path: PathBuf, confirmed_large: bool, forced: Option<Enc>) {
        let key = fileio::FileKey::of(&path);
        if let Some(i) = self.find_doc_by_key(&key) {
            self.active = i;
            self.focus_editor = true;
            return;
        }
        if self.loads.iter().any(|l| l.key.same_as(&key)) {
            self.status = format!("{} は読み込み中です", path.display());
            return;
        }
        if !confirmed_large {
            if let Ok(m) = std::fs::metadata(&path) {
                if m.len() > fileio::LARGE_FILE_BYTES {
                    self.push_modal(Modal::ConfirmLargeOpen { path, size: m.len() });
                    return;
                }
            }
        }
        self.start_load(path, key, LoadKind::Open, forced);
    }

    /// 読み込みをバックグラウンドのスレッドで始める (読み込み中も他のタブを操作できる)
    fn start_load(&mut self, path: PathBuf, key: fileio::FileKey, kind: LoadKind, forced: Option<Enc>) {
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        let p = path.clone();
        let spawned = std::thread::Builder::new()
            .name("load".into())
            .spawn(move || {
                let _ = tx.send(fileio::load(&p, forced));
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            self.push_modal(Modal::Info(format!(
                "ファイルを読み込めませんでした:\n{}\n\n{}",
                path.display(),
                e
            )));
            return;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.status = format!("読み込み中… {name}");
        let origin = LoadOrigin {
            active_doc: self.docs.get(self.active).map(|d| d.id),
            focus: self.ctx.memory(|m| m.focused()),
            active_generation: self.docs.get(self.active).map(|d| d.generation),
        };
        self.loads.push(PendingLoad {
            path,
            key,
            kind,
            origin,
            rx,
        });
    }

    /// 終わった読み込みを取り込む。
    ///
    /// 毎フレームの最後 (編集欄の処理の後) に呼ぶ。フレームの最初に呼ぶと、同じフレームの
    /// 入力 (キー入力・Ctrl+S など) が、取り込みで切り替わった新しいタブへ届いてしまう
    fn poll_loads(&mut self, ctx: &egui::Context) {
        let mut finished = Vec::new();
        let mut i = 0;
        while i < self.loads.len() {
            match self.loads[i].rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => i += 1,
                Ok(r) => finished.push((self.loads.remove(i), r)),
                Err(mpsc::TryRecvError::Disconnected) => finished.push((
                    self.loads.remove(i),
                    Err(fileio::LoadError::Message("読み込みが中断されました".into())),
                )),
            }
        }
        if !finished.is_empty() {
            ctx.request_repaint(); // 取り込んだ結果を、次のフレームで画面に出す
        }
        for (l, r) in finished {
            self.finish_load(ctx, l, r);
        }
    }

    /// 読み込みを頼んでから、ユーザーが選択中のタブ・フォーカスを変えていないか
    fn user_unchanged_since(&self, ctx: &egui::Context, o: &LoadOrigin) -> bool {
        let active = self.docs.get(self.active).map(|d| d.id);
        if active != o.active_doc {
            return false;
        }
        // 読み込みの間に、選択中の文書を編集していた (打ち続けている) なら、フォーカスが
        // 同じままでも、新しいタブへ切り替えない (以降の入力が新しいタブへ入ってしまう)
        if self.docs.get(self.active).map(|d| d.generation) != o.active_generation {
            return false;
        }
        let focus = ctx.memory(|m| m.focused());
        // 起動直後などフォーカスが無かったところから、選択中の編集欄へ移っただけなら、
        // ユーザーが変えたのではない
        focus == o.focus
            || (o.focus.is_none() && active.is_some_and(|id| focus == Some(editor_id(id))))
    }

    fn finish_load(
        &mut self,
        ctx: &egui::Context,
        l: PendingLoad,
        result: Result<fileio::Loaded, fileio::LoadError>,
    ) {
        let PendingLoad {
            path,
            key,
            kind,
            origin,
            ..
        } = l;
        // 選択・フォーカスを動かしてよいのは、ユーザーが読み込みの間に何も変えていないときだけ。
        // 自動インデントが入力を次のフレームへ持ち越している間は、その入力が選択中の文書へ
        // 届くよう、選択を動かさない (新しいタブは追加だけして、選択はしない)
        // 検索欄へフォーカスを移す途中 (`focus_query`) も動かさない (新しいタブの編集欄と
        // 検索欄の両方が、同じ入力を受け取ってしまう)
        let follow = self.deferred_events.is_none()
            && !self.find.focus_query
            && self.user_unchanged_since(ctx, &origin);
        match (kind, result) {
            (LoadKind::Open, Err(fileio::LoadError::Utf32 { name, as_utf16 })) => {
                self.push_modal(Modal::Utf32 {
                    path,
                    name,
                    as_utf16,
                });
            }
            (LoadKind::Open, Err(e)) => {
                self.push_modal(Modal::Info(format!(
                    "ファイルを開けませんでした:\n{}\n\n{}",
                    path.display(),
                    e.message()
                )));
            }
            (LoadKind::Reload { .. }, Err(e)) => {
                self.push_modal(Modal::Info(format!(
                    "読み直せませんでした (文書はそのままです):\n{}\n\n{}",
                    path.display(),
                    e.message()
                )));
            }
            (LoadKind::Open, Ok(loaded)) => {
                // 読み込み中に、別の経路 (名前を付けて保存など) で同じファイルが開かれていたら切り替えるだけ
                if let Some(i) = self.find_doc_by_key(&key) {
                    if follow {
                        self.active = i;
                        self.focus_editor = true;
                    }
                    return;
                }
                let notes = load_notes(loaded.lossy, loaded.mixed_eol, loaded.eol);
                let id = self.alloc_id();
                let doc = Doc::from_loaded(id, path, loaded);
                // Undo 履歴の件数を、本文の大きさに合わせる
                reset_editor_state(ctx, editor_id(id), doc.text.len());
                self.status = if follow {
                    format!(
                        "{} を開きました ({} / {}){}",
                        doc.title(),
                        doc.enc.label(),
                        doc.eol.label(),
                        notes
                    )
                } else {
                    format!(
                        "「{}」を開きました ({} / {}){}",
                        doc.title(),
                        doc.enc.label(),
                        doc.eol.label(),
                        notes
                    )
                };
                // 未編集の空タブ1つだけなら置き換える。読み込みの間にそのタブへ入力した (そして
                // 消して空に戻した) ときは、Undo 履歴ごと消さないよう置き換えない (`follow` が偽)
                let replace = follow
                    && self.docs.len() == 1
                    && self.docs[0].path.is_none()
                    && !self.docs[0].has_unsaved()
                    && self.docs[0].text.is_empty();
                if replace {
                    forget_editor_state(ctx, editor_id(self.docs[0].id));
                    self.docs[0] = doc;
                    self.active = 0;
                } else {
                    self.docs.push(doc);
                    if follow {
                        self.active = self.docs.len() - 1;
                    }
                }
                if follow {
                    self.focus_editor = true;
                }
            }
            (LoadKind::Reload { doc_id, generation }, Ok(loaded)) => {
                let Some(i) = self.index_of(doc_id) else {
                    return; // 読み込み中にタブが閉じられた
                };
                // 読み込みを始めてから、文書が変わっていたら (編集・保存・保存先の変更・
                // 文字コードや改行コードの変更) 結果は捨てる。取り込むと、新しい編集が
                // Undo もできずに消えたり、別の保存先の文書に古い内容が入ったりする
                let d = &mut self.docs[i];
                let same_path = d
                    .path
                    .as_deref()
                    .is_some_and(|p| fileio::FileKey::of(p).same_as(&key));
                if d.generation != generation || !same_path {
                    self.status = "読み込み中に変更されたため、開き直しを中止しました".into();
                    return;
                }
                let notes = load_notes(loaded.lossy, loaded.mixed_eol, loaded.eol);
                d.text = loaded.text;
                d.enc = loaded.enc;
                d.eol = loaded.eol;
                d.lossy_load = loaded.lossy;
                d.mixed_eol = loaded.mixed_eol;
                d.fingerprint = loaded.fingerprint;
                d.disk_unverified = false;
                d.mark_saved();
                d.edited();
                d.cursor_idx = 0;
                d.line_col = (1, 1);
                // 読み直す前の Undo 履歴で、別の内容に戻ってしまわないようにする
                reset_editor_state(ctx, editor_id(d.id), d.text.len());
                self.status = format!("{} として再読み込みしました{}", d.enc.label(), notes);
                if follow && self.docs[self.active].id == doc_id {
                    self.focus_editor = true;
                }
            }
        }
    }

    /// 保存。成功 (または保存する変更がない) なら true。
    /// 確認の画面を出して中断したとき、保存に失敗したとき、キャンセルしたときは false
    fn try_save(&mut self, ctx: &egui::Context, idx: usize, save_as: bool, then: Then) -> bool {
        let d = &self.docs[idx];
        // 変更のない文書の上書き保存では何も書かない (デコードできなかったバイトや、
        // 混在していた改行コードを、意図せず書き換えてしまわないように)
        // (保存の直後に保存先を確かめられなかった文書は、変更がなくても確認つきで保存し直す)
        if !save_as && d.path.is_some() && !d.has_unsaved() {
            self.status = format!("{} に変更はありません", d.title());
            return true;
        }
        let path = match (&d.path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let name = if d.path.is_some() {
                    d.title()
                } else {
                    format!("{}.txt", d.title())
                };
                let mut dlg = rfd::FileDialog::new()
                    .set_file_name(name)
                    .add_filter("テキストファイル", &["txt"])
                    .add_filter("すべてのファイル", &["*"]);
                if let Some(dir) = d.path.as_ref().and_then(|p| p.parent()) {
                    dlg = dlg.set_directory(dir);
                }
                match dlg.save_file() {
                    Some(p) => p,
                    None => return false,
                }
            }
        };
        self.write_doc(ctx, idx, path, then, Confirmed::default())
    }

    /// 保存して、成功したら続きの操作 (`then`) を行う
    fn save_then(&mut self, ctx: &egui::Context, idx: usize, save_as: bool, then: Then) -> bool {
        let ok = self.try_save(ctx, idx, save_as, then);
        if ok {
            self.resume(ctx, then);
        }
        ok
    }

    /// 保存の後に続けてやることを行う
    fn resume(&mut self, ctx: &egui::Context, then: Then) {
        match then {
            Then::None => {}
            Then::Close(id) => {
                if let Some(i) = self.index_of(id) {
                    self.close_now(i);
                }
            }
            Then::SaveAll => self.save_all_and_exit(ctx),
        }
    }

    /// 未保存の文書をすべて保存して終了する。確認の画面で止まったときは、
    /// その確認に答えた後で (`Then::SaveAll`) ここへ戻って続きを行う
    fn save_all_and_exit(&mut self, ctx: &egui::Context) {
        let mut i = 0;
        while i < self.docs.len() {
            if self.docs[i].has_unsaved() {
                self.active = i;
                if !self.try_save(ctx, i, false, Then::SaveAll) {
                    return; // キャンセル・失敗・確認待ちのときは終了を中止
                }
            }
            i += 1;
        }
        self.allow_exit = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// 文書を `path` に保存する。各種の確認 (`ok` で答え済みのものは省く) が必要なら、
    /// 確認の画面を設定して false を返す。保存できたら true。
    ///
    /// 失敗しうる準備 (文字コードの変換・画面の文字を保存内容にそろえるための複製) は、
    /// ディスクへ書く前にすべて済ませる。書いた後は失敗しない処理だけにして、
    /// 「ファイルは書けたのに画面が食い違ったまま、保存成功として閉じる・終了する」ことが
    /// ないようにする
    fn write_doc(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        path: PathBuf,
        then: Then,
        ok: Confirmed,
    ) -> bool {
        let key = fileio::FileKey::of(&path);
        // 別のタブで開いているファイルへは保存しない (同じファイルの二重管理になるため)
        let open_elsewhere = self.docs.iter().enumerate().any(|(i, d)| {
            i != idx
                && d.path
                    .as_deref()
                    .is_some_and(|p| fileio::FileKey::of(p).same_as(&key))
        }) || self
            .loads
            .iter()
            .any(|l| matches!(l.kind, LoadKind::Open) && l.key.same_as(&key));
        if open_elsewhere {
            self.modals.push_back(Modal::Info(format!(
                "そのファイルは別のタブで開いています:
{}",
                path.display()
            )));
            return false;
        }

        let d = &self.docs[idx];
        let id = d.id;
        let same_file = d
            .path
            .as_deref()
            .is_some_and(|p| fileio::FileKey::of(p).same_as(&key));
        // 保存先が、ユーザーが承知した状態のままか。承知した状態 (`expected`) は:
        // - 外部の変更を承知した (`ok.conflict`) なら、そのとき見せた状態
        // - すでに保存を始めている (`ok.baseline`。確認の画面をはさんだ後) なら、始めたときの状態
        // - 開いたファイルへの上書きなら、読み込んだときの状態
        // - それ以外 (新規・名前を付けて保存) は、保存先を選んだときに見えている状態
        //   (保存先が無ければ「存在しない」。書くまでの間に作られていないこと)
        // 確認の画面 (文字コードの警告など) の後でも、この状態を取り直さずに持ち回る。
        // 取り直すと、確認を待っている間に他のプログラムが作った・変えたファイルを、
        // 気づかずに承知したことにして上書きしてしまう。
        // 違っていれば確認し直す。実際に置き換える直前にも `atomic_write` が確かめる
        let current = fileio::fingerprint(&path);
        // 内容を最後まで読む指紋は、この保存の間に 1 回だけ取る (大きなファイルで UI を止めない)
        let mut fresh: Option<Option<Fingerprint>> = None;
        let mut digested = || {
            fresh
                .get_or_insert_with(|| fileio::fingerprint_digested(&path))
                .clone()
        };
        // 前回の保存の直後に、保存先を確かめられなかった (指紋が当てにならない) とき
        let unverified =
            ok.conflict.is_none() && ok.baseline.is_none() && same_file && d.disk_unverified;
        let expected = if unverified {
            None // 使わない (確認の画面へ進む)
        } else {
            match (&ok.conflict, &ok.baseline) {
                (Some(seen), _) => seen.clone(),
                (None, Some(b)) => b.clone(),
                (None, None) => match (same_file, &d.fingerprint) {
                    (true, Some(fp)) => Some(fp.clone()),
                    // 保存先を選んだ時点の状態。内容の要約も付けて覚える (置き換えで退避した控えを
                    // 消してよいかの確認に使う。要約が無いと、控えを毎回残してしまう)
                    _ => digested(),
                },
            }
        };
        if unverified || current != expected {
            // 承知してもらう版には、内容の要約も付ける (退避した控えを消してよいかの確認用)
            let seen = digested().or(current);
            self.modals.push_back(Modal::Conflict {
                id,
                path,
                then,
                ok: Confirmed {
                    conflict: None,
                    baseline: None,
                    ..ok
                },
                seen,
            });
            return false;
        }
        let ok = Confirmed {
            baseline: Some(expected.clone()),
            ..ok
        };
        // 元のファイルへの上書きでは、デコードできなかったバイトが失われる
        if same_file && d.lossy_load && !ok.lossy_load {
            self.modals.push_back(Modal::LossyLoad {
                id,
                path,
                then,
                ok,
            });
            return false;
        }
        let expect = fileio::Expect::Is(expected);
        let out = encoding::apply_eol(&d.text, d.eol);
        let (bytes, lossy) = encoding::encode(&out, d.enc);
        drop(out);
        if lossy && !ok.lossy {
            self.modals.push_back(Modal::Lossy {
                id,
                path,
                then,
                ok,
                examples: encoding::lossy_examples(&d.text, d.enc, 5),
            });
            return false;
        }
        // 画面の文字を、実際に書き込む内容 (開き直したときに見える文字) にそろえる準備。
        // 履歴に積むので、Ctrl+Z で元の文字に戻せる。準備ができなければ保存しない
        let prepared = if lossy {
            let (back, _) = encoding::decode(&bytes, d.enc);
            let back = encoding::normalize_eol(back);
            if back != d.text {
                let cur = d.cursor_idx.min(back.chars().count());
                match prepare_replace(ctx, editor_id(id), &d.text, back, (cur, cur)) {
                    Some(p) => Some((p, cur)),
                    None => {
                        self.modals.push_back(Modal::Info(format!(
                            "メモリが足りないため、保存できませんでした:\n{}\n\n\
                             (文書も、保存先のファイルも変更していません)",
                            path.display()
                        )));
                        return false;
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let written = match fileio::atomic_write(&path, &bytes, &expect) {
            Ok(w) => w,
            Err(fileio::WriteError::Conflict) => {
                // 確認してから書くまでの間に、保存先がまた変わった。何も書き換えていない
                self.modals.push_back(Modal::Conflict {
                    id,
                    path: path.clone(),
                    then,
                    ok: Confirmed {
                        conflict: None,
                        baseline: None,
                        ..ok
                    },
                    seen: fileio::fingerprint_digested(&path),
                });
                return false;
            }
            Err(fileio::WriteError::Failed(e)) => {
                // 元のファイルはそのまま。変更ありのままにする
                self.modals.push_back(Modal::Info(format!(
                    "保存に失敗しました:\n{}\n\n{}",
                    path.display(),
                    e
                )));
                return false;
            }
        };

        // ここから先は失敗しない処理だけ
        let d = &mut self.docs[idx];
        d.path = Some(path.clone());
        let mut note = String::new();
        // 保存を「成功」として続き (タブを閉じる・終了) へ進めてよいか。
        // 他のプログラムの版を退避したときは、利用者に知らせて止める
        let mut finished = true;
        // 保存先が書いた内容そのものだと確かめられなかったか
        let mut unverified = false;
        let mut preserved_at = None;
        match written {
            fileio::Written::Verified(fp) => {
                d.fingerprint = Some(fp);
                d.disk_unverified = false;
            }
            fileio::Written::Unverified => unverified = true,
            fileio::Written::Preserved { backup, fp } => {
                finished = false;
                unverified = fp.is_none();
                if let Some(fp) = fp {
                    d.fingerprint = Some(fp);
                    d.disk_unverified = false;
                }
                preserved_at = Some(backup);
            }
        }
        if unverified {
            // 他のプログラムが置き換えの直後に差し替えた可能性がある。その版の指紋を
            // 「自分が書いたもの」として覚えると、次の保存で気づかずに上書きしてしまう。
            // 編集中の内容はディスクに無いかもしれないので、保存済みにはしない
            d.fingerprint = None;
            d.disk_unverified = true;
            finished = false;
        }
        d.lossy_load = false;
        d.mixed_eol = false;
        if let Some((p, cur)) = prepared {
            p.apply(ctx, editor_id(d.id), &mut d.text);
            d.edited();
            d.cursor_idx = cur;
            d.line_col = line_col(&d.text, cur);
            note.push_str(" ※一部の文字は保存形式に合わせて変わりました (Ctrl+Z で戻せます)");
        }
        d.meta_changed();
        if unverified {
            // 保存済みにしない (変更ありのまま)。タブを閉じる・終了する操作で内容を失わないようにする
            d.dirty = true;
            self.status = format!(
                "{} を保存しましたが、保存先が書いた内容と一致するか確認できませんでした",
                d.title()
            );
            self.modals.push_back(Modal::Unverified {
                id,
                path: path.clone(),
                preserved: preserved_at,
            });
            return false;
        }
        d.mark_saved();
        self.status = format!(
            "{} を保存しました ({} / {}){}",
            d.title(),
            d.enc.label(),
            d.eol.label(),
            note
        );
        if let Some(backup) = preserved_at {
            self.modals.push_back(Modal::Info(format!(
                "保存の直前に、他のプログラムがこのファイルを書き換えていました:\n{}\n\n\
                 その版 (書き換えられた内容) は消さずに、次のファイルに残しました:\n{}\n\n\
                 保存先には、いま編集していた内容を書き込みました。\n\
                 (タブを閉じる・終了する操作は中止しました)",
                path.display(),
                backup.display()
            )));
        }
        finished
    }

    fn request_close(&mut self, idx: usize) {
        if self.docs[idx].has_unsaved() {
            self.active = idx;
            self.push_modal(Modal::ConfirmClose(self.docs[idx].id));
        } else {
            self.close_now(idx);
        }
    }

    fn close_now(&mut self, idx: usize) {
        // 閉じた文書の編集欄の状態 (Undo 履歴を含む) を、egui の記憶から取り除く
        forget_editor_state(&self.ctx, editor_id(self.docs[idx].id));
        self.docs.remove(idx);
        if self.docs.is_empty() {
            self.new_doc();
            return;
        }
        if idx < self.active || self.active >= self.docs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.focus_editor = true;
    }

    /// 文字コードを指定してディスクから読み直す (文字化けの修正用)。
    /// 未保存の編集があるときは、捨ててよいか先に確認する
    fn request_reload(&mut self, enc: Enc) {
        let d = &self.docs[self.active];
        if d.path.is_none() {
            self.status = "保存されていない文書は再読み込みできません".into();
            return;
        }
        if d.has_unsaved() {
            let id = d.id;
            self.push_modal(Modal::ConfirmReload { id, enc });
        } else {
            self.start_reload(d.id, enc);
        }
    }

    /// ディスクのファイルをもう一度読み込んで、文書を置き換える (読み込み中の内容は変えない)
    fn start_reload(&mut self, id: u64, enc: Enc) {
        let Some(i) = self.index_of(id) else {
            return;
        };
        let Some(path) = self.docs[i].path.clone() else {
            return;
        };
        let pending = self
            .loads
            .iter()
            .any(|l| matches!(l.kind, LoadKind::Reload { doc_id, .. } if doc_id == id));
        if pending {
            self.status = "読み込み中です".into();
            return;
        }
        let key = fileio::FileKey::of(&path);
        let generation = self.docs[i].generation;
        self.start_load(
            path,
            key,
            LoadKind::Reload {
                doc_id: id,
                generation,
            },
            Some(enc),
        );
    }

    /// 保存時の文字コードを変更する (文字コード変換)
    fn convert_to(&mut self, enc: Enc) {
        let d = &mut self.docs[self.active];
        if d.enc == enc {
            return;
        }
        let from = d.enc.label();
        let (_, lossy) = encoding::encode(&d.text, enc);
        d.enc = enc;
        d.meta_changed();
        d.update_dirty();
        self.status = if lossy {
            format!(
                "⚠ {} → {} に変換: この文字コードで表現できない文字があります (保存時に確認します)",
                from,
                enc.label()
            )
        } else {
            format!(
                "{} → {} に変換しました。保存すると反映されます",
                from,
                enc.label()
            )
        };
    }

    fn set_eol(&mut self, eol: Eol) {
        let d = &mut self.docs[self.active];
        if d.eol != eol {
            d.eol = eol;
            d.meta_changed();
            d.update_dirty();
            self.status = format!(
                "改行コードを {} に変更しました。保存すると反映されます",
                eol.label()
            );
        }
    }

    /// 検索バーを開く。1行の選択範囲があれば検索語に入れる
    fn open_find(&mut self, ctx: &egui::Context, replace: bool) {
        let d = &self.docs[self.active];
        if let Some((s, e)) = editor_sel(ctx, editor_id(d.id)) {
            if s < e {
                let sel: String = d.text.chars().skip(s).take(e - s).collect();
                if !sel.contains('\n') {
                    self.find.query = sel;
                }
            }
        }
        if !self.find.open {
            self.find.show_replace = replace;
        } else if replace {
            self.find.show_replace = true;
        }
        self.find.open = true;
        self.find.focus_query = true;
    }

    /// 次 (forward) / 前の一致を選択し、画面内にスクロールする。
    ///
    /// カーソル位置から文書を直接探す (一致の一覧が上限で打ち切られていても使える)
    fn find_jump(&mut self, ctx: &egui::Context, forward: bool) {
        if self.find.query.is_empty() {
            self.open_find(ctx, false);
            self.status = "検索語を入力してください".into();
            return;
        }
        let cs = self.find.case_sensitive;
        let query = self.find.query.clone();
        let d = &mut self.docs[self.active];
        let id = editor_id(d.id);
        let (cur_s, cur_e) = editor_sel(ctx, id).unwrap_or((d.cursor_idx, d.cursor_idx));
        let found = if forward {
            search::find_next(&d.text, &query, cs, search::char_to_byte(&d.text, cur_e))
        } else {
            search::find_prev(&d.text, &query, cs, search::char_to_byte(&d.text, cur_s))
        };
        let Some((bs, be)) = found else {
            self.status = "見つかりません".into();
            return;
        };
        let s = search::byte_to_char(&d.text, bs);
        let e = s + d.text[bs..be].chars().count();
        let doc_id = d.id;
        d.cursor_idx = e;
        d.line_col = line_col(&d.text, e);
        // 何件目かは、覚えている一覧から求める (上限を超える一致は番号を出さない)
        d.ensure_matches(&query, cs);
        let capped = d.find_cache.as_ref().is_some_and(|c| c.capped);
        let nth = search::exact_index(d.matches(), s, e);
        let total = d.matches().len();
        set_editor_sel(ctx, id, s, e);
        self.scroll_req = Some((doc_id, s));
        self.status = match (nth, capped) {
            (Some(i), false) => format!("{} 件中 {} 件目を選択しました", total, i + 1),
            (Some(i), true) => format!(
                "{} 件目を選択しました ({} 件以上)",
                i + 1,
                with_commas(search::MAX_STORED_MATCHES)
            ),
            (None, _) => "一致を選択しました".into(),
        };
        ctx.request_repaint();
    }

    /// 選択中の一致を置換して次へ進む。
    ///
    /// 仕様 (一致の件数にかかわらず同じ): 選択範囲の文字列そのものが検索語に一致する
    /// (大文字小文字の区別の設定に従う) なら、その選択範囲を置換する。そうでなければ、
    /// 置換せずに次の一致へ移動する。一致の一覧 (件数に上限がある) は参照しないので、
    /// 手で選択した範囲でも、上限を超える件数の文書でも、挙動は変わらない
    fn replace_one(&mut self, ctx: &egui::Context) {
        if self.find.query.is_empty() {
            self.status = "検索語を入力してください".into();
            return;
        }
        let cs = self.find.case_sensitive;
        let query = self.find.query.clone();
        let repl = self.find.replacement.clone();
        let d = &mut self.docs[self.active];
        let id = editor_id(d.id);
        let (sel_s, sel_e) = editor_sel(ctx, id).unwrap_or((d.cursor_idx, d.cursor_idx));
        let bs = search::char_to_byte(&d.text, sel_s);
        let be = search::char_to_byte(&d.text, sel_e);
        let is_match = search::is_exact_match(&d.text, bs, be, &query, cs);
        if !is_match {
            self.find_jump(ctx, true);
            return;
        }
        let new_text = match search::replace_ranges(
            &d.text,
            &[(bs, be)],
            &repl,
            search::MAX_REPLACE_RESULT,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.push_modal(Modal::Info(replace_error_message(&e)));
                return;
            }
        };
        let cur = sel_s + repl.chars().count();
        // 置換後の本文で次の一致を探し、それを選択した状態にする
        let next = search::find_next(&new_text, &query, cs, bs + repl.len()).map(|(nb, ne)| {
            let ts = search::byte_to_char(&new_text, nb);
            (ts, ts + new_text[nb..ne].chars().count())
        });
        let (ts, te) = next.unwrap_or((cur, cur));
        if !replace_text_with_undo(ctx, id, &mut d.text, new_text, (ts, te)) {
            self.push_modal(Modal::Info(replace_error_message(
                &search::ReplaceError::OutOfMemory,
            )));
            return;
        }
        d.edited();
        d.update_dirty();
        d.cursor_idx = te;
        d.line_col = line_col(&d.text, te);
        let doc_id = d.id;
        // Ctrl+Z ですぐ取り消せるよう、編集欄にフォーカスを戻す
        self.focus_editor = true;
        if next.is_some() {
            self.scroll_req = Some((doc_id, ts));
        }
        self.status = if next.is_some() {
            "1 件置換しました".into()
        } else {
            "1 件置換しました (これ以上の一致はありません)".into()
        };
        ctx.request_repaint();
    }

    /// すべての一致を置換する。
    ///
    /// 一致の一覧は使わず、文書を 2 回走査する (1 回目は件数と置換後の大きさだけを数え、
    /// 上限を超えるときは何も作らずに断る。2 回目で結果を組み立てる)
    fn replace_all_matches(&mut self, ctx: &egui::Context) {
        if self.find.query.is_empty() {
            self.status = "検索語を入力してください".into();
            return;
        }
        let cs = self.find.case_sensitive;
        let (query, repl) = (self.find.query.clone(), self.find.replacement.clone());
        let d = &mut self.docs[self.active];
        let (new_text, n) = match search::replace_all_streaming(
            &d.text,
            &query,
            &repl,
            cs,
            search::MAX_REPLACE_RESULT,
        ) {
            Ok(r) => r,
            Err(e) => {
                self.push_modal(Modal::Info(replace_error_message(&e)));
                return;
            }
        };
        if n == 0 {
            self.status = "見つかりません".into();
            return;
        }
        let cur = d.cursor_idx.min(new_text.chars().count());
        if !replace_text_with_undo(ctx, editor_id(d.id), &mut d.text, new_text, (cur, cur)) {
            self.push_modal(Modal::Info(replace_error_message(
                &search::ReplaceError::OutOfMemory,
            )));
            return;
        }
        d.edited();
        d.update_dirty();
        d.cursor_idx = cur;
        d.line_col = line_col(&d.text, cur);
        self.focus_editor = true;
        self.status = format!("{} 件を置換しました (Ctrl+Z で元に戻せます)", n);
        ctx.request_repaint();
    }

    /// 選択中の文書に対する操作か (宛先の文書を固定して実行するもの)
    fn targets_active(act: &Action) -> bool {
        matches!(
            act,
            Action::Save
                | Action::SaveAs
                | Action::CloseActive
                | Action::NextTab
                | Action::PrevTab
                | Action::Reload(_)
                | Action::Convert(_)
                | Action::SetEol(_)
                | Action::OpenFind(_)
                | Action::FindNext
                | Action::FindPrev
                | Action::ReplaceOne
                | Action::ReplaceAll
        )
    }

    /// 操作を `owner` (操作を集めた時点の選択中の文書) に対して実行する。
    /// その文書がもう無ければ (閉じられた) 何もしない
    fn run_for(&mut self, ctx: &egui::Context, owner: u64, act: Action) {
        if Self::targets_active(&act) {
            match self.index_of(owner) {
                Some(i) => self.active = i,
                None => return,
            }
        }
        self.run(ctx, act);
    }

    fn run(&mut self, ctx: &egui::Context, act: Action) {
        match act {
            Action::OpenFind(replace) => self.open_find(ctx, replace),
            Action::CloseFind => {
                self.find.open = false;
                self.focus_editor = true;
            }
            Action::FindNext => self.find_jump(ctx, true),
            Action::FindPrev => self.find_jump(ctx, false),
            Action::ReplaceOne => self.replace_one(ctx),
            Action::ReplaceAll => self.replace_all_matches(ctx),
            Action::New => self.new_doc(),
            Action::Open => self.open_dialog(),
            Action::Save => {
                self.save_then(ctx, self.active, false, Then::None);
            }
            Action::SaveAs => {
                self.save_then(ctx, self.active, true, Then::None);
            }
            Action::Select(i) => {
                if i < self.docs.len() {
                    self.active = i;
                    self.focus_editor = true;
                }
            }
            Action::Close(i) => {
                if i < self.docs.len() {
                    self.request_close(i)
                }
            }
            Action::CloseActive => self.request_close(self.active),
            Action::NextTab => {
                self.active = (self.active + 1) % self.docs.len();
                self.focus_editor = true;
            }
            Action::PrevTab => {
                self.active = (self.active + self.docs.len() - 1) % self.docs.len();
                self.focus_editor = true;
            }
            Action::Exit => ctx.send_viewport_cmd(ViewportCommand::Close),
            Action::Reload(e) => self.request_reload(e),
            Action::Convert(e) => self.convert_to(e),
            Action::SetEol(e) => self.set_eol(e),
            Action::SaveAndClose(id) => {
                self.pop_modal();
                if let Some(i) = self.index_of(id) {
                    self.save_then(ctx, i, false, Then::Close(id));
                }
            }
            Action::DiscardAndClose(id) => {
                self.pop_modal();
                if let Some(i) = self.index_of(id) {
                    self.close_now(i);
                }
            }
            Action::SaveAllAndExit => {
                self.pop_modal();
                self.save_all_and_exit(ctx);
            }
            Action::ExitNow => {
                self.pop_modal();
                self.allow_exit = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Action::SaveConfirmed {
                id,
                path,
                then,
                ok,
            } => {
                self.pop_modal();
                if let Some(i) = self.index_of(id) {
                    if self.write_doc(ctx, i, path, then, ok) {
                        self.resume(ctx, then);
                    }
                }
            }
            Action::SaveAsFor(id, then) => {
                self.pop_modal();
                if let Some(i) = self.index_of(id) {
                    self.save_then(ctx, i, true, then);
                }
            }
            Action::ReloadNow(id, enc) => {
                self.pop_modal();
                self.start_reload(id, enc);
            }
            Action::OpenAnyway(path) => {
                self.pop_modal();
                self.open_path_checked(path, true, None);
            }
            Action::OpenAs(path, enc) => {
                self.pop_modal();
                self.open_path_checked(path, true, Some(enc));
            }
            Action::DismissModal => self.pop_modal(),
        }
    }

    fn shortcuts(&self, ctx: &egui::Context, acts: &mut Vec<Action>) {
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let c = Modifiers::COMMAND;
        let sc = |m, k| KeyboardShortcut::new(m, k);
        let find_open = self.find.open;
        ctx.input_mut(|i| {
            // Shift付きを先に判定する (Ctrl+S が Ctrl+Shift+S にもマッチするため)
            if i.consume_shortcut(&sc(cs, Key::S)) {
                acts.push(Action::SaveAs);
            }
            if i.consume_shortcut(&sc(c, Key::S)) {
                acts.push(Action::Save);
            }
            if i.consume_shortcut(&sc(c, Key::N)) || i.consume_shortcut(&sc(c, Key::T)) {
                acts.push(Action::New);
            }
            if i.consume_shortcut(&sc(c, Key::O)) {
                acts.push(Action::Open);
            }
            if i.consume_shortcut(&sc(c, Key::W)) {
                acts.push(Action::CloseActive);
            }
            if i.consume_shortcut(&sc(c, Key::F)) {
                acts.push(Action::OpenFind(false));
            }
            if i.consume_shortcut(&sc(c, Key::H)) {
                acts.push(Action::OpenFind(true));
            }
            // F3 も Shift 付きを先に判定する
            if i.consume_shortcut(&sc(Modifiers::SHIFT, Key::F3)) {
                acts.push(Action::FindPrev);
            }
            if i.consume_shortcut(&sc(Modifiers::NONE, Key::F3)) {
                acts.push(Action::FindNext);
            }
            if find_open && i.consume_key(Modifiers::NONE, Key::Escape) {
                acts.push(Action::CloseFind);
            }
            if i.consume_shortcut(&sc(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab)) {
                acts.push(Action::PrevTab);
            }
            if i.consume_shortcut(&sc(Modifiers::CTRL, Key::Tab)) {
                acts.push(Action::NextTab);
            }
        });
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("ファイル", |ui| {
                let items: [(&str, &str, fn() -> Action); 6] = [
                    ("新規タブ", "Ctrl+N", || Action::New),
                    ("開く...", "Ctrl+O", || Action::Open),
                    ("上書き保存", "Ctrl+S", || Action::Save),
                    ("名前を付けて保存...", "Ctrl+Shift+S", || {
                        Action::SaveAs
                    }),
                    ("タブを閉じる", "Ctrl+W", || Action::CloseActive),
                    ("終了", "", || Action::Exit),
                ];
                for (i, (label, key, act)) in items.into_iter().enumerate() {
                    if i == 5 {
                        ui.separator();
                    }
                    if ui
                        .add(egui::Button::new(label).shortcut_text(key))
                        .clicked()
                    {
                        acts.push(act());
                        ui.close_menu();
                    }
                }
            });

            let cur = self.docs[self.active].enc;
            let has_path = self.docs[self.active].path.is_some();
            ui.menu_button("文字コード", |ui| {
                ui.label(format!("現在: {}", cur.label()));
                ui.separator();
                ui.menu_button("文字コード変換 (保存時の文字コード)", |ui| {
                    ui.label("テキストを保ったまま、保存する文字コードを変更します");
                    ui.separator();
                    for e in Enc::all() {
                        if ui.radio(cur == e, e.label()).clicked() {
                            acts.push(Action::Convert(e));
                            ui.close_menu();
                        }
                    }
                });
                ui.add_enabled_ui(has_path, |ui| {
                    ui.menu_button("指定した文字コードで開き直す", |ui| {
                        ui.label("文字化けしたときに使います (ディスクから読み直します。未保存の編集は確認のうえ破棄)");
                        ui.separator();
                        for e in Enc::all() {
                            if ui.radio(cur == e, e.label()).clicked() {
                                acts.push(Action::Reload(e));
                                ui.close_menu();
                            }
                        }
                    });
                });
                ui.separator();
                let cur_eol = self.docs[self.active].eol;
                ui.menu_button("改行コード", |ui| {
                    for e in Eol::all() {
                        if ui.radio(cur_eol == e, e.label()).clicked() {
                            acts.push(Action::SetEol(e));
                            ui.close_menu();
                        }
                    }
                });
            });

            ui.menu_button("検索", |ui| {
                let items: [(&str, &str, fn() -> Action); 4] = [
                    ("検索", "Ctrl+F", || Action::OpenFind(false)),
                    ("置換", "Ctrl+H", || Action::OpenFind(true)),
                    ("次を検索", "F3", || Action::FindNext),
                    ("前を検索", "Shift+F3", || Action::FindPrev),
                ];
                for (label, key, act) in items {
                    if ui
                        .add(egui::Button::new(label).shortcut_text(key))
                        .clicked()
                    {
                        acts.push(act());
                        ui.close_menu();
                    }
                }
            });

            ui.menu_button("表示", |ui| {
                ui.checkbox(&mut self.wrap, "右端で折り返す");
                ui.checkbox(&mut self.show_line_numbers, "行番号を表示");
                ui.checkbox(&mut self.auto_indent, "自動インデント");
                ui.add(egui::Slider::new(&mut self.font_size, 8.0..=40.0).text("文字サイズ"));
                ui.separator();
                let mut dark = ui.ctx().style().visuals.dark_mode;
                if ui.checkbox(&mut dark, "ダークテーマ").changed() {
                    ui.ctx().set_visuals(if dark {
                        egui::Visuals::dark()
                    } else {
                        egui::Visuals::light()
                    });
                }
            });
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        // 選択が変わったときだけ、選択中のタブが見えるまでタブの列をスクロールする
        // (Ctrl+Tab でタブが多いとき、選択中のタブが画面の外に出てしまうため)
        let active_id = self.docs[self.active].id;
        let scroll_to_active = self.tab_shown != Some(active_id);
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, d) in self.docs.iter().enumerate() {
                    let selected = i == self.active;
                    let title = if d.has_unsaved() {
                        format!("● {}", d.title())
                    } else {
                        d.title()
                    };
                    let frame = egui::Frame::none()
                        .inner_margin(egui::Margin::symmetric(6.0, 3.0))
                        .rounding(egui::Rounding {
                            nw: 6.0,
                            ne: 6.0,
                            sw: 0.0,
                            se: 0.0,
                        })
                        .fill(if selected {
                            ui.visuals().extreme_bg_color
                        } else {
                            ui.visuals().faint_bg_color
                        })
                        .stroke(if selected {
                            ui.visuals().widgets.active.bg_stroke
                        } else {
                            egui::Stroke::NONE
                        });
                    let shown = frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let r = ui.selectable_label(selected, title);
                            let r = match &d.path {
                                Some(p) => r.on_hover_text(p.display().to_string()),
                                None => r,
                            };
                            if r.clicked() {
                                acts.push(Action::Select(i));
                            }
                            if r.middle_clicked() {
                                acts.push(Action::Close(i));
                            }
                            if ui.small_button("×").on_hover_text("閉じる").clicked() {
                                acts.push(Action::Close(i));
                            }
                        });
                    });
                    if selected && scroll_to_active {
                        shown.response.scroll_to_me(None);
                    }
                }
                if ui
                    .button(" + ")
                    .on_hover_text("新規タブ (Ctrl+N)")
                    .clicked()
                {
                    acts.push(Action::New);
                }
            });
        });
        self.tab_shown = Some(active_id);
    }

    /// 検索・置換バー (開いているときだけ表示)
    /// このフレームのフォーカスの行き先を、どの欄を描くよりも前に一度だけ決める。
    /// 検索欄へ移す指示 (`focus_query`) があれば検索欄 (編集欄への要求は取り下げる)、
    /// なければ編集欄への指示 (`focus_editor`) を選択中の編集欄へ。
    /// 描いている途中で移すと、先に描いた欄と後に描いた欄の両方が同じ入力を受け取ってしまう
    fn apply_focus(&mut self, ctx: &egui::Context) {
        if self.find.open && self.find.focus_query {
            self.find.focus_query = false;
            self.focus_editor = false;
            ctx.memory_mut(|m| m.request_focus(find_query_id()));
        } else if std::mem::take(&mut self.focus_editor) {
            let id = editor_id(self.docs[self.active].id);
            ctx.memory_mut(|m| m.request_focus(id));
        }
    }

    fn find_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let qid = find_query_id();
        let composing = self.ime_guard.composing;
        let segs = self.ime_guard.segs.clone();
        // 検索欄にフォーカスがあるときの Enter / Shift+Enter。欄に入力させないよう先に取り除く
        // (Shift を含む組み合わせを先に判定する。consume_key は余分な Shift を無視するため)
        if ui.memory(|m| m.has_focus(qid)) {
            if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter)) {
                acts.push(Action::FindPrev);
            } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                acts.push(Action::FindNext);
            }
        }

        // 件数の表示 (文書の検索結果キャッシュと、このフレームの現在位置を使う)
        let has_query = !self.find.query.is_empty();
        let label = if has_query {
            let d = &self.docs[self.active];
            let capped = d.find_cache.as_ref().is_some_and(|c| c.capped);
            let matches = d.matches();
            if matches.is_empty() {
                "見つかりません".to_owned()
            } else if capped {
                // 上限を超える一致は数えない
                format!("{} 件以上", with_commas(search::MAX_STORED_MATCHES))
            } else {
                let cur = self.find_pos.current.unwrap_or(0);
                format!("{} 件中 {} 件目", matches.len(), cur + 1)
            }
        } else {
            String::new()
        };

        // 幅が狭いときは折り返して、ボタンが欄の外に切れないようにする
        ui.horizontal_wrapped(|ui| {
            ui.label("検索:");
            singleline_with_preedit(ui, &mut self.find.query, Some(qid), composing, &segs);
            if ui
                .button("▲前")
                .on_hover_text("前を検索 (Shift+F3)")
                .clicked()
            {
                acts.push(Action::FindPrev);
            }
            if ui.button("▼次").on_hover_text("次を検索 (F3)").clicked() {
                acts.push(Action::FindNext);
            }
            ui.checkbox(&mut self.find.case_sensitive, "大文字/小文字を区別");
            ui.label(label);
            if ui.small_button("×").on_hover_text("閉じる (Esc)").clicked() {
                acts.push(Action::CloseFind);
            }
        });

        if self.find.show_replace {
            ui.horizontal_wrapped(|ui| {
                ui.label("置換:");
                singleline_with_preedit(ui, &mut self.find.replacement, None, composing, &segs);
                if ui
                    .button("置換")
                    .on_hover_text("現在の一致を置換して次へ")
                    .clicked()
                {
                    acts.push(Action::ReplaceOne);
                }
                if ui.button("すべて置換").clicked() {
                    acts.push(Action::ReplaceAll);
                }
            });
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let d = &self.docs[self.active];
        let chars = d.stats.as_ref().map_or(0, |s| s.chars);
        ui.horizontal(|ui| {
            ui.label(&self.status);
            if !self.loads.is_empty() {
                ui.spinner();
                ui.label("読み込み中…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("{} v{}", APP_NAME, APP_VERSION));
                ui.separator();
                ui.label(d.enc.label());
                ui.separator();
                ui.label(d.eol.label());
                ui.separator();
                if d.mixed_eol {
                    // 読み込み時の改行コードの混在は、保存して統一するまで表示し続ける
                    // (状態表示は他のタブの読み込みなどで書き換わるため)
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("改行混在 → {} で保存", d.eol.label()),
                    );
                    ui.separator();
                }
                ui.label(format!("{} 文字", chars));
                ui.separator();
                ui.label(format!("{} 行, {} 列", d.line_col.0, d.line_col.1));
            });
        });
    }

    fn editor(&mut self, ui: &mut egui::Ui) {
        let font = egui::FontId::monospace(self.font_size);
        let wrap = self.wrap;
        let show_ln = self.show_line_numbers;
        let scroll_req = self.scroll_req.take();
        // 検索バーが開いていて検索語があるときだけ一致を強調する
        let find_on = self.find.open && !self.find.query.is_empty();
        let cs = self.find.case_sensitive;
        let dark = ui.visuals().dark_mode;
        let composing = self.ime_guard.composing;
        let segs = self.ime_guard.segs.clone();
        let auto_indent = self.auto_indent;
        let d = &mut self.docs[self.active];
        let id = editor_id(d.id);
        // 自動インデント付きの改行 (egui の TextEdit が処理する前に Enter を取り除いて挿入する)
        if auto_indent && !composing {
            let deferred = apply_auto_indent(ui.ctx(), id, &d.text);
            if !deferred.is_empty() {
                // 本文を書き換える入力の後ろの Enter は、次のフレームで (更新後の本文で) 処理する。
                // (`raw_input_hook` がすでに持ち越していれば、その前に処理する分として前へ足す)
                let mut deferred = deferred;
                if let Some((_, later)) = self.deferred_events.take() {
                    deferred.extend(later);
                }
                self.deferred_events = Some((Some(d.id), deferred));
                ui.ctx().request_repaint();
            }
        }
        if find_on {
            // 文書や検索語が変わったときだけ一致を数え直す
            d.ensure_matches(&self.find.query, cs);
        }
        // 行数は版が変わったときだけ数え直す
        let (_, line_count) = d.stats();
        // 以降は欄ごとに独立して借りる (本文を TextEdit に渡しながら、キャッシュも更新するため)
        let Doc {
            text,
            find_cache,
            row_lines,
            cursor_idx,
            line_col: line_col_pos,
            id: doc_id,
            ..
        } = &mut *d;
        let doc_id = *doc_id;
        let find_cache = if find_on { find_cache.as_ref() } else { None };
        let byte_ranges: &[(usize, usize)] = find_cache.map_or(&[], |c| c.byte_ranges.as_slice());
        // 選択がちょうど一致箇所と同じなら、それを「現在の一致」として濃く表示する
        // (このフレームの最初に求めた値。件数の表示と共有する)
        let exact = if find_on { self.find_pos.exact } else { None };
        let colors = match_colors(dark);

        // 行番号の桁数 (行数は改行の数から求める)
        let digits = line_count.to_string().len();
        let cur_line = line_col_pos.0;

        let scroll = if wrap {
            egui::ScrollArea::vertical()
        } else {
            egui::ScrollArea::both()
        };
        let changed = ui.horizontal_top(|ui| {
            // 行番号の欄はスクロール領域の「外」に置く。横スクロールしても左端に固定され、
            // 縦位置だけを本文 (galley) の位置に合わせて描く
            let gutter = if show_ln {
                let char_w = ui.fonts(|f| f.glyph_width(&font, '0'));
                let width = char_w * digits as f32 + GUTTER_PAD;
                let height = ui.available_height();
                Some(
                    ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover())
                        .0,
                )
            } else {
                None
            };
            let shown = scroll
                .id_salt(("scroll", doc_id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // この描画で何回目のレイアウトか。egui の TextEdit は、描画の最初に 1 回
                    // (編集前の本文で) レイアウトし、入力で本文が変わった場合だけもう一度
                    // (編集後の本文で) レイアウトする。版番号 (rev) は描画の後で上げるので、
                    // 検索結果 (編集前の本文の位置) の強調ができるのは 1 回目だけ。
                    // 2 回目は本文が変わっているため、強調しない。
                    //
                    // 結果 (galley) は自前で使い回さず、毎回 egui の `Fonts::layout_job` に任せる。
                    // egui は文字の描画用の画像 (フォントアトラス) を作り直すことがあり、その後は
                    // 古い galley の文字の位置情報が無効になる。egui 自身のキャッシュは
                    // それを考慮しているので、そちらに任せる
                    let calls = std::cell::Cell::new(0u32);
                    let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
                        let first = calls.get() == 0;
                        calls.set(calls.get() + 1);
                        let text_color = ui.visuals().text_color();
                        let mut job = egui::text::LayoutJob::simple(
                            s.to_owned(),
                            font.clone(),
                            text_color,
                            if wrap { wrap_width } else { f32::INFINITY },
                        );
                        if first && !byte_ranges.is_empty() {
                            let sections = highlight_sections(
                                s, &font, text_color, byte_ranges, exact, colors,
                            );
                            if !sections.is_empty() {
                                job.sections = sections;
                            }
                        }
                        ui.fonts(|f| f.layout_job(job))
                    };
                    let old_fill = hide_selection_fill(ui, composing);
                    let fill_slot = reserve_preedit_slot(ui);
                    let out = egui::TextEdit::multiline(&mut *text)
                        .id(id)
                        .frame(false)
                        .lock_focus(true)
                        .desired_width(if wrap {
                            ui.available_width()
                        } else {
                            f32::INFINITY
                        })
                        .min_size(ui.available_size())
                        .layouter(&mut layouter)
                        .show(ui);
                    ui.visuals_mut().selection.bg_fill = old_fill;
                    paint_preedit_for(ui, &out, composing, &segs, old_fill, fill_slot);
                    let changed = out.response.changed();
                    if let Some(cr) = out.cursor_range {
                        let idx = cr.primary.ccursor.index;
                        if idx != *cursor_idx || changed {
                            *cursor_idx = idx;
                            *line_col_pos = line_col(text, idx);
                        }
                    }

                    // 一致箇所を画面内に表示する (折り返し後の位置を含めて galley から求める)
                    if let Some((req_id, idx)) = scroll_req {
                        if req_id == doc_id {
                            let rect = out
                                .galley
                                .pos_from_ccursor(CCursor::new(idx))
                                .translate(out.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect, Some(egui::Align::Center));
                        }
                    }

                    (out.galley.clone(), out.galley_pos, changed)
                });
            let (galley, galley_pos, changed) = shown.inner;

            // 行番号: 論理行の先頭の行 (折り返しの 2 行目以降は番号を付けない) にだけ描く。
            // 見えている行から描く (表示行の先頭からたどらない)
            if let Some(g) = gutter {
                let painter = ui.painter_at(g);
                painter.rect_filled(g, 0.0, ui.visuals().faint_bg_color);
                let weak = ui.visuals().weak_text_color();
                let strong = ui.visuals().strong_text_color();
                let lines = row_lines_for(row_lines, &galley);
                let first = first_visible_row(&galley, galley_pos.y, g.top());
                for i in first..galley.rows.len() {
                    let row = &galley.rows[i];
                    let y = galley_pos.y + row.rect.min.y;
                    if y > g.bottom() {
                        break; // 以降は画面の下 (描く必要なし)
                    }
                    let line_no = lines[i];
                    let line_start = i == 0 || lines[i - 1] != line_no;
                    if line_start {
                        let color = if line_no as usize == cur_line {
                            strong
                        } else {
                            weak
                        };
                        painter.text(
                            egui::pos2(g.right() - GUTTER_PAD / 2.0, y),
                            egui::Align2::RIGHT_TOP,
                            line_no.to_string(),
                            font.clone(),
                            color,
                        );
                    }
                }
            }
            changed
        })
        .inner;
        if changed {
            // 版を上げ、保存済みの内容と比べて変更あり (dirty) を更新する。
            // キャッシュは版で判定するので、次の描画で作り直される
            d.edited();
            d.update_dirty();
            // 文書が大きくなって履歴の件数の段階が下がったら、履歴を作り直す
            let want = undo_limit(d.text.len());
            if want < recorded_undo_limit(ui.ctx(), id) {
                shrink_undo_history(ui.ctx(), id, &d.text, want);
            }
            ui.ctx().request_repaint();
        }
    }
}

/// 3 桁ごとにカンマで区切った数 (例: 100000 → "100,000")
fn with_commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 置換できなかった理由の説明
fn replace_error_message(e: &search::ReplaceError) -> String {
    match e {
        search::ReplaceError::TooLarge => format!(
            "置換後のサイズが上限 ({} MiB) を超えるため、置換できません。
(文書は変更されていません)",
            search::MAX_REPLACE_RESULT / (1024 * 1024)
        ),
        search::ReplaceError::OutOfMemory => {
            "メモリが足りないため、置換できません。
(文書は変更されていません)".to_string()
        }
    }
}

/// galley の各表示行の論理行番号。galley が変わったときだけ作り直す
fn row_lines_for<'a>(cache: &'a mut Option<RowLines>, galley: &Arc<egui::Galley>) -> &'a [u32] {
    let fresh = matches!(cache, Some(c) if Weak::ptr_eq(&c.galley, &Arc::downgrade(galley)));
    if !fresh {
        *cache = Some(RowLines {
            galley: Arc::downgrade(galley),
            lines: row_line_numbers(galley.rows.iter().map(|r| r.ends_with_newline)),
        });
    }
    cache.as_ref().map_or(&[], |c| c.lines.as_slice())
}

/// 入力された文字列・貼り付けた文字列の改行を LF に統一する。
///
/// egui-winit は貼り付けの CRLF だけを LF にするため、単独の CR (古い Mac 形式など) が
/// 本文に入り込むことがある。本文の内部表現は LF だけなので、入り口で揃える
fn normalize_text_events(events: &mut [egui::Event]) {
    for e in events {
        if let egui::Event::Text(s) | egui::Event::Paste(s) = e {
            if s.contains('\r') {
                *s = encoding::normalize_eol(std::mem::take(s));
            }
        }
    }
}

fn line_col(text: &str, char_idx: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for c in text.chars().take(char_idx) {
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let time = raw_input.time.unwrap_or(0.0);
        self.ime_guard.filter(time, &mut raw_input.events);
        normalize_text_events(&mut raw_input.events);
        // 前のフレームで持ち越した入力 (自動インデントの Enter など) を先頭へ戻す。
        // すでに ImeGuard を通っているので、もう一度は通さない。
        // 宛先の文書がもう選択中・フォーカス中でなければ (タブの切り替えなど)、
        // 別の文書へ入力が流れ込むのを避けて捨てる
        if let Some((doc_id, deferred)) = self.deferred_events.take() {
            let still_target = doc_id.map_or(true, |doc_id| {
                self.docs.get(self.active).is_some_and(|d| d.id == doc_id)
                    && ctx.memory(|m| m.has_focus(editor_id(doc_id)))
            });
            if still_target {
                raw_input.events.splice(0..0, deferred);
            }
        }
        // 自動インデントの Enter と、それ以降の入力を次のフレームへ持ち越すかを、ここで
        // (update の中のショートカットの処理より前に) 決める。ショートカットの Ctrl+S などが
        // 改行より先に処理されて、保存やタブの切り替えが入力の順序と食い違うのを防ぐ
        // タブ・フォーカスを変えるショートカットでの切れ目は、自動インデントの設定や
        // 編集欄のフォーカスに関係なく行う (後ろの入力を、切り替え後の行き先へ届けるため)
        if let Some(d) = self.docs.get(self.active) {
            let id = editor_id(d.id);
            let focused = self.focus_editor || ctx.memory(|m| m.has_focus(id));
            // 検索欄へフォーカスを移す途中 (`focus_query`) は、入力は編集欄ではなく検索欄へ届く。
            // 編集欄の自動インデントの切れ目や宛先 (編集欄の文書) は付けない
            let indent = self.auto_indent
                && !self.find.focus_query
                && !self.ime_guard.composing
                && focused
                && TextEditState::load(ctx, id).is_some();
            let esc_closes =
                self.find.open && self.modals.is_empty() && !self.ime_guard.composing;
            let split = if indent {
                let mut down = ctx.input(|i| i.pointer.any_down());
                for e in &raw_input.events {
                    if let egui::Event::PointerButton { pressed, .. } = e {
                        down = *pressed;
                    }
                }
                input_split(&raw_input.events, down, esc_closes)
            } else {
                target_change_split(&raw_input.events, esc_closes).map(|(_, at)| (at, false))
            };
            if let Some((n, addressed)) = split {
                let rest = raw_input.events.split_off(n);
                self.deferred_events = Some((addressed.then_some(d.id), rest));
                ctx.request_repaint();
            }
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd = match frame.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        self.update_ui(ctx, hwnd);
    }
}

impl App {
    /// 1 フレーム分の画面の更新。`hwnd` は IME の文節の取得に使うウィンドウハンドル
    /// (取得できなければ 0)。eframe に依存しないので、テストから直接呼べる
    fn update_ui(&mut self, ctx: &egui::Context, hwnd: isize) {
        self.update_ui_with(ctx, hwnd, Vec::new());
    }

    /// このフレームの検索の現在位置を求める (選択範囲から 1 回だけ。件数の表示と、
    /// 現在の一致の強調で共有する。一覧は昇順なので二分探索)
    fn update_find_pos(&mut self, ctx: &egui::Context) {
        self.find_pos = FindPos::default();
        if !self.find.open || self.find.query.is_empty() {
            return;
        }
        let cs = self.find.case_sensitive;
        self.docs[self.active].ensure_matches(&self.find.query, cs);
        let d = &self.docs[self.active];
        let (s, e) = editor_sel(ctx, editor_id(d.id)).unwrap_or((d.cursor_idx, d.cursor_idx));
        let m = d.matches();
        let exact = search::exact_index(m, s, e);
        self.find_pos = FindPos {
            exact,
            current: exact.or_else(|| search::next_index(m, e)),
        };
    }

    /// `update_ui` の本体。`extra` は、画面の操作のほかに、このフレームで実行する操作
    /// (テストで、ボタンを押したのと同じ状況を作るために使う)
    fn update_ui_with(&mut self, ctx: &egui::Context, hwnd: isize, extra: Vec<Action>) {
        // 変換中は IME から文節 (どれが変換対象か) を取得する。失敗時は空 (全体表示)
        if self.ime_guard.composing {
            self.ime_guard.segs = ime::query(hwnd, &self.ime_guard.preedit);
        } else if !self.ime_guard.segs.is_empty() {
            self.ime_guard.segs.clear();
        }
        let mut acts = extra;
        // このフレームで集める操作 (保存・置換・文字コードの変更など、選択中の文書に対する操作)
        // の宛先は、フレームの最初に選択していた文書。操作を実行するときに、
        // 「そのときの選択中」ではなく、この文書へ向ける (`run_for`)
        let owner = self.docs[self.active].id;

        // ウィンドウへのドラッグ&ドロップで開く
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for p in dropped {
            self.open_path(p);
        }
        // フォーカスの行き先は、このフレームでタブを切り替えうる処理 (上のドロップ) の後に、
        // 一度だけ決める (ショートカットの処理・欄の描画より前)
        self.apply_focus(ctx);

        // ウィンドウを閉じる要求。未保存の文書があるかの判断は、この描画の編集欄の処理と、
        // 画面の操作 (「すべて置換」など本文や保存形式を変える操作) がすべて終わった後で行う
        // (同じ描画で入力や操作と閉じる要求が届くと、先に判断すると変更を見逃してしまうため)
        let close_requested = ctx.input(|i| i.viewport().close_requested());

        let enabled = self.modals.is_empty();
        if enabled {
            self.shortcuts(ctx, &mut acts);
        }

        // 文字数・行数 (版が変わったときだけ数え直す)
        self.docs[self.active].stats();

        // ウィンドウタイトル
        let d = &self.docs[self.active];
        let title = format!(
            "{}{} - {} v{}",
            if d.has_unsaved() { "*" } else { "" },
            d.title(),
            APP_NAME,
            APP_VERSION
        );
        if title != self.last_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        self.update_find_pos(ctx);
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            ui.add_enabled_ui(enabled, |ui| self.menu_bar(ui, &mut acts));
        });
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_enabled_ui(enabled, |ui| self.tab_bar(ui, &mut acts));
        });
        if self.find.open {
            egui::TopBottomPanel::top("find").show(ctx, |ui| {
                ui.add_enabled_ui(enabled, |ui| self.find_bar(ui, &mut acts));
            });
        }
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(enabled, |ui| self.editor(ui));
        });

        // 画面の操作・ショートカットは、読み込みの取り込みより先に実行する。取り込みで選択中の
        // タブが切り替わると、同じフレームの Ctrl+S や「すべて置換」が、新しく開いたタブへ
        // 向いてしまうため (宛先もフレームの最初の文書に固定してある)
        for a in acts {
            self.run_for(ctx, owner, a);
        }

        // バックグラウンドの読み込みが終わっていれば取り込む。編集欄の処理の後に行う
        // (フレームの最初に行うと、このフレームの入力が、取り込みで切り替わった新しいタブへ届く)
        self.poll_loads(ctx);

        // 確認の画面の答えは、文書の id を持っているので、取り込みの後でよい
        if let Some(m) = self.modals.front().cloned() {
            let mut modal_acts = Vec::new();
            self.show_modal(ctx, m, &mut modal_acts);
            for a in modal_acts {
                self.run(ctx, a);
            }
        }

        // 本文や保存形式を変える操作がすべて終わったので、未保存の文書があるか確かめる。
        // あれば閉じるのを取り消して確認する (なければ、閉じる要求をそのまま通す)
        if close_requested && !self.allow_exit && self.docs.iter().any(|d| d.has_unsaved()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if !self.modals.iter().any(|m| matches!(m, Modal::ConfirmExit)) {
                self.push_modal(Modal::ConfirmExit);
            }
            ctx.request_repaint(); // 確認の画面を、次のフレームで出す
        }
    }

    fn show_modal(&self, ctx: &egui::Context, m: Modal, acts: &mut Vec<Action>) {
        let win = |title: &str| {
            egui::Window::new(title.to_owned())
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        };
        match m {
            Modal::ConfirmClose(id) => {
                let name = self
                    .index_of(id)
                    .map(|i| self.docs[i].title())
                    .unwrap_or_default();
                win("確認").show(ctx, |ui| {
                    ui.label(format!("「{}」は変更されています。保存しますか?", name));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("保存して閉じる").clicked() {
                            acts.push(Action::SaveAndClose(id));
                        }
                        if ui.button("保存せずに閉じる").clicked() {
                            acts.push(Action::DiscardAndClose(id));
                        }
                        if ui.button("キャンセル").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::ConfirmExit => {
                let n = self.docs.iter().filter(|d| d.has_unsaved()).count();
                win("終了の確認").show(ctx, |ui| {
                    ui.label(format!("未保存の文書が {} 個あります。", n));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("すべて保存して終了").clicked() {
                            acts.push(Action::SaveAllAndExit);
                        }
                        if ui.button("保存せずに終了").clicked() {
                            acts.push(Action::ExitNow);
                        }
                        if ui.button("キャンセル").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::Lossy {
                id,
                path,
                then,
                ok,
                examples,
            } => {
                let enc = self
                    .index_of(id)
                    .map(|i| self.docs[i].enc.label())
                    .unwrap_or_default();
                win("文字コードの警告").show(ctx, |ui| {
                    ui.label(format!(
                        "{} では正しく保存できない文字が含まれています。\n\
                         そのまま保存すると、表現できない文字は「&#数値;」形式に、\n\
                         別の文字と同じ符号になる文字 (例: ¥ → \\) はその文字に変わります。",
                        enc
                    ));
                    if !examples.is_empty() {
                        ui.add_space(4.0);
                        ui.label(format!("変わる文字の例: {}", examples.join("、")));
                    }
                    ui.label("保存すると、画面の文字も保存した内容に置き換わります (Ctrl+Z で戻せます)。");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("それでも保存").clicked() {
                            acts.push(Action::SaveConfirmed {
                                id,
                                path: path.clone(),
                                then,
                                ok: Confirmed {
                                    lossy: true,
                                    ..ok.clone()
                                },
                            });
                        }
                        if ui.button("キャンセル").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::LossyLoad {
                id,
                path,
                then,
                ok,
            } => {
                win("上書き保存の確認").show(ctx, |ui| {
                    ui.label(
                        "このファイルには、文字コードとして読めないバイトが含まれていました。\n\
                         上書き保存すると、その元のバイトは失われ、別の文字 (U+FFFD) に置き換わります。",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("保存").clicked() {
                            acts.push(Action::SaveConfirmed {
                                id,
                                path: path.clone(),
                                then,
                                ok: Confirmed {
                                    lossy_load: true,
                                    ..ok.clone()
                                },
                            });
                        }
                        if ui.button("名前を付けて保存").clicked() {
                            acts.push(Action::SaveAsFor(id, then));
                        }
                        if ui.button("キャンセル").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::Conflict {
                id,
                path,
                then,
                ok,
                seen,
            } => {
                win("ファイルが変更されています").show(ctx, |ui| {
                    ui.label(format!(
                        "このファイルは、開いた後に他のプログラムで変更・削除・作成されています:\n{}\n\n\
                         上書きすると、その変更は失われます。",
                        path.display()
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("上書きする").clicked() {
                            // いま見えている状態を承知したものとして、保存の直前にもう一度確かめる
                            acts.push(Action::SaveConfirmed {
                                id,
                                path: path.clone(),
                                then,
                                ok: Confirmed {
                                    conflict: Some(seen.clone()),
                                    ..ok.clone()
                                },
                            });
                        }
                        if ui.button("キャンセル").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::Unverified {
                id,
                path,
                preserved,
            } => {
                win("保存先を確認できませんでした").show(ctx, |ui| {
                    ui.label(format!(
                        "保存した直後に、このファイルが他のプログラム (同期ツールなど) に\n\
                         変更された可能性があります:\n{}\n\n\
                         編集中の内容はディスクに無いかもしれないので、タブは保存済みにせず、\n\
                         閉じる・終了する操作は中止しました。",
                        path.display()
                    ));
                    if let Some(p) = &preserved {
                        ui.add_space(4.0);
                        ui.label(format!(
                            "また、置き換えの直前に他のプログラムが書いていた版は、次のファイルに残しました:\n{}",
                            p.display()
                        ));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("名前を付けて保存").clicked() {
                            acts.push(Action::SaveAsFor(id, Then::None));
                        }
                        if ui.button("閉じる (タブはそのまま)").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::ConfirmReload { id, enc } => {
                let name = self
                    .index_of(id)
                    .map(|i| self.docs[i].title())
                    .unwrap_or_default();
                win("確認").show(ctx, |ui| {
                    ui.label(format!(
                        "「{}」の未保存の編集は破棄されます。\n{} でディスクから読み直しますか?",
                        name,
                        enc.label()
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("破棄して読み直す").clicked() {
                            acts.push(Action::ReloadNow(id, enc));
                        }
                        if ui.button("キャンセル").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::ConfirmLargeOpen { path, size } => {
                win("大きなファイル").show(ctx, |ui| {
                    ui.label(format!(
                        "{}\nのサイズは約 {} MB です。開くのに時間とメモリがかかります。\n開きますか?",
                        path.display(),
                        size / (1024 * 1024)
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("開く").clicked() {
                            acts.push(Action::OpenAnyway(path.clone()));
                        }
                        if ui.button("キャンセル").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::Utf32 {
                path,
                name,
                as_utf16,
            } => {
                win("対応していない形式").show(ctx, |ui| {
                    ui.label(format!(
                        "{}\nは {} の BOM で始まっています。UTF-32 には対応していません。\n\
                         UTF-16 / UTF-8 / Shift_JIS などに変換してから開いてください。",
                        path.display(),
                        name
                    ));
                    if as_utf16.is_some() {
                        ui.add_space(4.0);
                        ui.label(
                            "先頭が U+0000 の UTF-16 LE (BOM 付き) のファイルである可能性もあります。",
                        );
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if let Some(enc) = as_utf16 {
                            if ui.button("UTF-16 として開く").clicked() {
                                acts.push(Action::OpenAs(path.clone(), enc));
                            }
                        }
                        if ui.button("閉じる").clicked() {
                            acts.push(Action::DismissModal);
                        }
                    });
                });
            }
            Modal::Info(msg) => {
                win("お知らせ").show(ctx, |ui| {
                    ui.label(msg);
                    ui.add_space(8.0);
                    if ui.button("OK").clicked() {
                        acts.push(Action::DismissModal);
                    }
                });
            }
        }
    }
}

/// egui の既定フォントには日本語が含まれないので、Windows のフォントを読み込む
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into()))
        .join("Fonts");
    let load = |names: &[&str]| names.iter().find_map(|n| std::fs::read(dir.join(n)).ok());

    if let Some(b) = load(&["meiryo.ttc", "YuGothM.ttc", "msgothic.ttc"]) {
        fonts
            .font_data
            .insert("jp_ui".into(), egui::FontData::from_owned(b));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("jp_ui".into());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("jp_ui".into());
    }
    // 等幅は MS ゴシック / BIZ UDゴシック を優先 (半角:全角 = 1:2 で揃う)
    if let Some(b) = load(&["msgothic.ttc", "BIZ-UDGothicR.ttc"]) {
        fonts
            .font_data
            .insert("jp_mono".into(), egui::FontData::from_owned(b));
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "jp_mono".into());
    }
    // 日本語フォントに無い文字 (ハングル・簡体字など) のフォールバック
    for (name, file) in [("ko", "malgun.ttf"), ("zh", "msyh.ttc")] {
        if let Some(b) = load(&[file]) {
            fonts
                .font_data
                .insert(name.into(), egui::FontData::from_owned(b));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().push(name.into());
            }
        }
    }
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} v{}", APP_NAME, APP_VERSION))
        .with_inner_size([1000.0, 700.0])
        .with_min_inner_size([400.0, 300.0])
        .with_drag_and_drop(true);
    // ウィンドウ/タスクバーのアイコン。読み込みに失敗してもアイコン無しで起動する
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(APP_NAME, options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// 置換の取り消し (Undo) を、画面を出さずに egui を動かして確かめるテスト
#[cfg(test)]
mod undo_tests {
    use super::*;

    /// 検索欄 + 本文の 2 つの TextEdit だけを持つ最小の画面
    struct Harness {
        ctx: egui::Context,
        text: String,
        field: String,
        t: f64,
        /// 自動インデントを有効にするか (アプリの `editor` と同じ呼び出しを再現する)
        auto_indent: bool,
        /// 自動インデントが次のフレームへ持ち越した入力 (アプリの `deferred_events` と同じ)
        deferred: Vec<egui::Event>,
    }

    fn ed() -> egui::Id {
        editor_id(1)
    }
    fn enter() -> egui::Event {
        egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }
    fn field() -> egui::Id {
        egui::Id::new("field")
    }

    impl Harness {
        fn new(text: &str) -> Self {
            Harness {
                ctx: egui::Context::default(),
                text: text.into(),
                field: String::new(),
                t: 0.0,
                auto_indent: false,
                deferred: Vec::new(),
            }
        }
        fn with_indent(text: &str) -> Self {
            Harness {
                auto_indent: true,
                ..Harness::new(text)
            }
        }
        /// 本文にフォーカスを置き、選択範囲を設定して反映させる
        fn select(&mut self, a: usize, b: usize) {
            self.focus(ed());
            set_editor_sel(&self.ctx, ed(), a, b);
            self.frame(0.05, vec![]);
        }
        fn frame(&mut self, dt: f64, mut events: Vec<egui::Event>) {
            self.t += dt;
            // 前のフレームで持ち越した入力を先頭へ戻す (アプリの `raw_input_hook` と同じ)
            events.splice(0..0, std::mem::take(&mut self.deferred));
            let raw = egui::RawInput {
                time: Some(self.t),
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let (text, fld, ai) = (&mut self.text, &mut self.field, self.auto_indent);
            let deferred = &mut self.deferred;
            let _ = self.ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(fld).id(field()));
                    if ai {
                        *deferred = apply_auto_indent(ctx, ed(), text);
                    }
                    ui.add(egui::TextEdit::multiline(text).id(ed()));
                });
            });
        }
        fn focus(&mut self, id: egui::Id) {
            self.ctx.memory_mut(|m| m.request_focus(id));
            self.frame(0.05, vec![]);
        }
        fn key(&mut self, key: Key, modifiers: Modifiers) {
            let ev = egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            };
            self.frame(0.05, vec![ev]);
            self.frame(0.05, vec![]);
        }
        fn guarded(&mut self, g: &mut ImeGuard, dt: f64, mut events: Vec<egui::Event>) {
            g.filter(self.t + dt, &mut events);
            self.frame(dt, events);
        }
        fn type_text(&mut self, s: &str) {
            self.frame(0.05, vec![egui::Event::Text(s.into())]);
        }
    }

    /// 入力 → 置換 → Ctrl+Z で、置換だけが取り消され、直前の入力は残る
    #[test]
    fn undo_replace_all_keeps_typed_text() {
        let mut h = Harness::new("abc abc");
        h.focus(ed());
        h.frame(2.0, vec![]);
        h.type_text("X");
        let typed = h.text.clone();
        h.focus(field()); // 置換バーへ移動 (入力の直後、1 秒以内)
        let new_text = search::replace_all(&h.text, "abc", "Z", true).unwrap().0;
        replace_text_with_undo(&h.ctx, ed(), &mut h.text, new_text, (0, 0));
        assert_eq!(h.text, typed.replace("abc", "Z"));
        h.focus(ed()); // アプリは置換後に編集欄へフォーカスを戻す
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text, typed, "1 回目の Ctrl+Z で置換前に戻る");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text, "abc abc", "2 回目の Ctrl+Z で入力も取り消される");
        h.key(Key::Y, Modifiers::COMMAND);
        assert_eq!(h.text, typed, "Ctrl+Y でやり直せる");
    }

    /// 1 件ずつの置換は、Ctrl+Z 1 回で 1 件ずつ戻る
    #[test]
    fn undo_replace_one_step_by_step() {
        let mut h = Harness::new("a a a");
        h.focus(ed());
        h.frame(2.0, vec![]);
        h.focus(field());
        for (s, e) in [(0, 1), (2, 3)] {
            let bs = search::char_to_byte(&h.text, s);
            let be = search::char_to_byte(&h.text, e);
            let mut t = h.text.clone();
            t.replace_range(bs..be, "b");
            replace_text_with_undo(&h.ctx, ed(), &mut h.text, t, (e, e));
        }
        assert_eq!(h.text, "b b a");
        h.focus(ed());
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text, "b a a");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text, "a a a");
    }

    /// IME: 変換 → 確定の Enter (同フレーム、Enter が先) は改行を入れない
    #[test]
    fn ime_confirm_enter_same_frame_no_newline() {
        let mut h = Harness::new("");
        let mut g = ImeGuard::default();
        h.focus(ed());
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Enabled)],
        );
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Preedit("あ".into()))],
        );
        h.guarded(
            &mut g,
            0.05,
            vec![
                enter(),
                egui::Event::Ime(egui::ImeEvent::Commit("あ".into())),
            ],
        );
        assert_eq!(h.text, "あ");
    }

    /// Commit の後に同フレームで Enter が来ても改行を入れない
    #[test]
    fn ime_confirm_enter_after_commit_same_frame() {
        let mut h = Harness::new("");
        let mut g = ImeGuard::default();
        h.focus(ed());
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Enabled)],
        );
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Preedit("い".into()))],
        );
        h.guarded(
            &mut g,
            0.05,
            vec![
                egui::Event::Ime(egui::ImeEvent::Commit("い".into())),
                enter(),
            ],
        );
        assert_eq!(h.text, "い");
    }

    /// Backspace で未確定文字列を消し切ったとき、winit が送る偽の確定 (古い確定文字列) は
    /// 挿入しない。winit の送り方 (空の Preedit / Disabled の後に Disabled + Commit) を再現する
    #[test]
    fn ime_fake_commit_after_backspace_ignored() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        // 消した通知が空の Preedit の場合と、Disabled の場合
        for cleared in [Preedit(String::new()), Disabled] {
            let mut h = Harness::new("x");
            let mut g = ImeGuard::default();
            h.focus(ed());
            h.guarded(&mut g, 0.05, vec![ime(Enabled), ime(Preedit("あい".into()))]);
            h.guarded(&mut g, 0.05, vec![ime(Preedit("あ".into()))]);
            h.guarded(&mut g, 0.05, vec![ime(cleared.clone())]);
            h.guarded(
                &mut g,
                0.05,
                vec![ime(Disabled), ime(Commit("あ".into())), ime(Disabled), ime(Disabled)],
            );
            assert_eq!(h.text, "x", "{cleared:?}");
            // その後の通常の変換・確定は効く
            h.guarded(&mut g, 0.05, vec![ime(Enabled), ime(Preedit("かんじ".into()))]);
            h.guarded(
                &mut g,
                0.05,
                vec![ime(Disabled), ime(Commit("漢字".into())), ime(Disabled)],
            );
            h.guarded(&mut g, 0.05, vec![ime(Disabled)]);
            assert_eq!(h.text, "x漢字", "{cleared:?}");
        }
    }

    /// 変換を取り消した (確定が続かずに Disabled だけが来た) とき、egui が本文へ挿入した
    /// 未確定文字列を残さない
    #[test]
    fn ime_cancel_removes_preedit_text() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        let mut h = Harness::new("x");
        let mut g = ImeGuard::default();
        h.select(1, 1);
        h.guarded(&mut g, 0.05, vec![ime(Enabled), ime(Preedit("あ".into()))]);
        assert_eq!(h.text, "xあ", "変換中は未確定文字列が本文に入っている");
        h.guarded(&mut g, 0.05, vec![ime(Disabled), ime(Disabled)]);
        assert_eq!(h.text, "x", "取り消しで未確定文字列が消える");
        assert_eq!(editor_sel(&h.ctx, ed()), Some((1, 1)));
        // 取り消しの後の通常の変換・確定は効く
        h.guarded(&mut g, 0.05, vec![ime(Enabled), ime(Preedit("かんじ".into()))]);
        h.guarded(
            &mut g,
            0.05,
            vec![ime(Disabled), ime(Commit("漢字".into())), ime(Disabled)],
        );
        assert_eq!(h.text, "x漢字", "本物の確定は、挿入した空の Preedit の後でも入る");
    }

    /// 挿入した空の Preedit は、偽の確定の判定に数えない (確定の前に Disabled が 1 つ来る
    /// ふつうの確定を、偽物として捨てない)
    #[test]
    fn ime_injected_preedit_is_not_counted_as_fake_commit_evidence() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        let mut g = ImeGuard::default();
        let mut evs = vec![ime(Enabled), ime(Preedit("あ".into())), ime(Disabled)];
        g.filter(0.0, &mut evs);
        assert_eq!(
            evs,
            vec![
                ime(Enabled),
                ime(Preedit("あ".into())),
                ime(Preedit(String::new())),
                ime(Disabled)
            ]
        );
        let mut evs = vec![ime(Commit("あ".into())), ime(Disabled)];
        g.filter(0.01, &mut evs);
        assert_eq!(evs[0], ime(Commit("あ".into())), "確定は捨てない");
        // 変換中でない Disabled には何も足さない
        let mut evs = vec![ime(Disabled)];
        g.filter(0.02, &mut evs);
        assert_eq!(evs, vec![ime(Disabled)]);
    }

    fn enter_release() -> egui::Event {
        egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn count_enter_presses(evs: &[egui::Event]) -> usize {
        evs.iter()
            .filter(|e| matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, .. }))
            .count()
    }

    /// 変換中の Enter → 確定 → Enter の離した通知 → 次の Enter: 最後の Enter は本物の改行
    #[test]
    fn ime_enter_release_ends_the_dropped_press() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        let mut g = ImeGuard::default();
        g.filter(1.0, &mut vec![ime(Enabled), ime(Preedit("あ".into()))]);
        let mut evs = vec![enter()]; // 変換中: 確定の Enter (取り除く)
        g.filter(1.01, &mut evs);
        assert_eq!(count_enter_presses(&evs), 0);
        g.filter(1.02, &mut vec![ime(Disabled), ime(Commit("あ".into()))]);
        // 離した通知
        let mut evs = vec![enter_release()];
        g.filter(1.03, &mut evs);
        assert_eq!(evs.len(), 1, "離した通知は通す");
        // 確定の直後 (猶予の間) でも、離した後の押下は本物の Enter
        let mut evs = vec![enter()];
        g.filter(1.04, &mut evs);
        assert_eq!(count_enter_presses(&evs), 1);
    }

    /// 確定 → Enter の離した通知 (確定の Enter の押下はアプリに届かなかった) → 次の Enter:
    /// 離した通知で、確定の後の Enter 用に残っていた取り除く権利が使い切られるので、
    /// 次の Enter (猶予の間でも、同じフレームでも) は本物の改行
    #[test]
    fn ime_release_expires_unused_post_commit_budget() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        // 離した通知の後の別のフレームの Enter
        let mut g = ImeGuard::default();
        g.filter(1.0, &mut vec![ime(Enabled), ime(Preedit("あ".into()))]);
        g.filter(1.01, &mut vec![ime(Disabled), ime(Commit("あ".into()))]);
        let mut evs = vec![enter_release()];
        g.filter(1.02, &mut evs);
        assert_eq!(evs.len(), 1, "離した通知は通す");
        let mut evs = vec![enter()];
        g.filter(1.03, &mut evs);
        assert_eq!(count_enter_presses(&evs), 1, "次の Enter は本物");
        // 離した通知と Enter が同じフレーム
        let mut g = ImeGuard::default();
        g.filter(2.0, &mut vec![ime(Enabled), ime(Preedit("あ".into()))]);
        g.filter(2.01, &mut vec![ime(Disabled), ime(Commit("あ".into()))]);
        let mut evs = vec![enter_release(), enter()];
        g.filter(2.02, &mut evs);
        assert_eq!(count_enter_presses(&evs), 1);
        // 離した通知が無ければ、従来どおり確定の直後の Enter を 1 回だけ取り除く
        let mut g = ImeGuard::default();
        g.filter(3.0, &mut vec![ime(Enabled), ime(Preedit("あ".into()))]);
        g.filter(3.01, &mut vec![ime(Disabled), ime(Commit("あ".into()))]);
        let mut evs = vec![enter()];
        g.filter(3.02, &mut evs);
        assert_eq!(count_enter_presses(&evs), 0);
    }

    /// 確定 → (遅れて届く) Enter → 離した通知 → 次の Enter: 最後の Enter は本物の改行
    #[test]
    fn ime_late_confirm_enter_then_release_then_real_enter() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        let mut g = ImeGuard::default();
        g.filter(1.0, &mut vec![ime(Enabled), ime(Preedit("あ".into()))]);
        g.filter(1.01, &mut vec![ime(Disabled), ime(Commit("あ".into()))]);
        let mut evs = vec![enter()];
        g.filter(1.02, &mut evs);
        assert_eq!(count_enter_presses(&evs), 0, "遅れて届いた確定の Enter");
        let mut evs = vec![enter_release(), enter()];
        g.filter(1.03, &mut evs);
        assert_eq!(count_enter_presses(&evs), 1, "離した後の Enter は本物");
    }

    /// 変換中の Enter → 確定 → 離した通知が来ないまま Enter: 同じ押下の続きとして取り除くが、
    /// 猶予を過ぎれば通す (離した通知が届かなくても、Enter が効かなくなり続けない)
    #[test]
    fn ime_enter_without_release_times_out() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        let mut g = ImeGuard::default();
        g.filter(1.0, &mut vec![ime(Enabled), ime(Preedit("あ".into()))]);
        g.filter(1.01, &mut vec![enter()]);
        g.filter(1.02, &mut vec![ime(Disabled), ime(Commit("あ".into()))]);
        let mut evs = vec![enter()];
        g.filter(1.03, &mut evs);
        assert_eq!(count_enter_presses(&evs), 0, "同じ押下の続き");
        let mut evs = vec![enter()];
        g.filter(1.2, &mut evs);
        assert_eq!(count_enter_presses(&evs), 1, "猶予を過ぎたら通す");
    }

    /// 変換中の文字列なしに来る確定 (全角スペースなど) はそのまま入る
    #[test]
    fn ime_direct_commit_kept() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        let mut h = Harness::new("");
        let mut g = ImeGuard::default();
        h.focus(ed());
        h.guarded(&mut g, 0.05, vec![ime(Enabled), ime(Preedit("あ".into()))]);
        h.guarded(&mut g, 0.05, vec![ime(Disabled), ime(Commit("あ".into())), ime(Disabled)]);
        h.guarded(&mut g, 0.05, vec![ime(Disabled)]);
        assert_eq!(h.text, "あ");
        // ImeGuard が Commit をそのまま通すことを確かめる (TextEdit への反映は egui 次第)
        let mut evs = vec![ime(Disabled), ime(Commit("\u{3000}".into())), ime(Disabled)];
        g.filter(h.t + 0.05, &mut evs);
        assert!(evs.contains(&ime(Commit("\u{3000}".into()))));
    }

    /// 確定せずに終わった変換の後、次の変換 (Enabled から始まる) の確定は捨てない
    #[test]
    fn ime_fake_commit_check_resets_on_enabled() {
        use egui::ImeEvent::*;
        let ime = |e: egui::ImeEvent| egui::Event::Ime(e);
        let mut g = ImeGuard::default();
        let mut evs = vec![ime(Enabled), ime(Preedit("あ".into())), ime(Disabled), ime(Disabled)];
        g.filter(0.0, &mut evs);
        let mut evs = vec![ime(Enabled), ime(Disabled), ime(Commit("\u{3000}".into())), ime(Disabled)];
        g.filter(1.0, &mut evs);
        assert!(evs.contains(&ime(Commit("\u{3000}".into()))));
    }

    /// Enter が次のフレームに来ても (直後なら) 改行を入れない。変換中の Enter も同様
    #[test]
    fn ime_confirm_enter_next_frame_suppressed() {
        let mut h = Harness::new("");
        let mut g = ImeGuard::default();
        h.focus(ed());
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Preedit("う".into()))],
        );
        // 変換中に Enter だけのフレーム、その次に Commit
        h.guarded(&mut g, 0.01, vec![enter()]);
        h.guarded(
            &mut g,
            0.01,
            vec![egui::Event::Ime(egui::ImeEvent::Commit("う".into()))],
        );
        // Commit の直後のフレームに Enter
        h.guarded(&mut g, 0.01, vec![enter()]);
        assert_eq!(h.text, "う");
    }

    /// IME を使わない Enter は改行になり、確定から時間が経った Enter も改行になる
    #[test]
    fn plain_enter_inserts_newline() {
        let mut h = Harness::new("");
        let mut g = ImeGuard::default();
        h.focus(ed());
        h.guarded(&mut g, 0.05, vec![enter()]);
        assert_eq!(h.text, "\n");
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Preedit("え".into()))],
        );
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Commit("え".into()))],
        );
        h.guarded(&mut g, 0.5, vec![enter()]);
        assert_eq!(h.text, "\nえ\n");
    }

    fn shift_enter() -> egui::Event {
        egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        }
    }

    /// 空白でインデントされた行の末尾で Enter: 同じ空白が次の行に入り、キャレットはその末尾
    #[test]
    fn auto_indent_spaces() {
        let mut h = Harness::with_indent("    abc");
        h.select(7, 7);
        h.frame(0.05, vec![enter()]);
        assert_eq!(h.text, "    abc\n    ");
        assert_eq!(editor_sel(&h.ctx, ed()), Some((12, 12)));
        // 続けて入力すると、インデントの後ろに入る
        h.type_text("x");
        assert_eq!(h.text, "    abc\n    x");
    }

    /// タブと空白の混在もそのまま写す
    #[test]
    fn auto_indent_tabs() {
        let mut h = Harness::with_indent("\t \tfoo");
        h.select(6, 6);
        h.frame(0.05, vec![enter()]);
        assert_eq!(h.text, "\t \tfoo\n\t \t");
        assert_eq!(editor_sel(&h.ctx, ed()), Some((10, 10)));
    }

    /// インデントの無い行では普通の改行
    #[test]
    fn auto_indent_none() {
        let mut h = Harness::with_indent("abc");
        h.select(3, 3);
        h.frame(0.05, vec![enter()]);
        assert_eq!(h.text, "abc\n");
        assert_eq!(editor_sel(&h.ctx, ed()), Some((4, 4)));
    }

    /// 2 行目以降でも、その行のインデントを使う
    #[test]
    fn auto_indent_uses_current_line() {
        let mut h = Harness::with_indent("a\n  b\nc");
        h.select(5, 5); // "  b" の末尾
        h.frame(0.05, vec![enter()]);
        assert_eq!(h.text, "a\n  b\n  \nc");
        assert_eq!(editor_sel(&h.ctx, ed()), Some((8, 8)));
    }

    /// キャレットがインデントの途中にあるときは、キャレットより前の空白だけを写す
    #[test]
    fn auto_indent_caret_inside_indent() {
        let mut h = Harness::with_indent("    abc");
        h.select(2, 2);
        h.frame(0.05, vec![enter()]);
        assert_eq!(h.text, "  \n    abc");
        assert_eq!(editor_sel(&h.ctx, ed()), Some((5, 5)));
    }

    /// 選択範囲は「改行 + インデント」に置き換わる
    #[test]
    fn auto_indent_replaces_selection() {
        let mut h = Harness::with_indent("  abcdef");
        h.select(4, 6); // "cd"
        h.frame(0.05, vec![enter()]);
        assert_eq!(h.text, "  ab\n  ef");
        assert_eq!(editor_sel(&h.ctx, ed()), Some((7, 7)));
    }

    /// Shift+Enter も Enter と同じ
    #[test]
    fn auto_indent_shift_enter() {
        let mut h = Harness::with_indent("  abc");
        h.select(5, 5);
        h.frame(0.05, vec![shift_enter()]);
        assert_eq!(h.text, "  abc\n  ");
    }

    /// 自動インデントが無効なら普通の改行
    #[test]
    fn auto_indent_off_plain_newline() {
        let mut h = Harness::new("    abc");
        h.select(7, 7);
        h.frame(0.05, vec![enter()]);
        assert_eq!(h.text, "    abc\n");
    }

    /// Ctrl+Z 1 回で改行とインデントが一緒に戻り、Ctrl+Y でやり直せる
    #[test]
    fn auto_indent_undo_redo_one_step() {
        let mut h = Harness::with_indent("    abc");
        h.select(7, 7);
        h.type_text("d");
        h.frame(2.0, vec![]); // 入力が落ち着いて履歴に区切りが入る (egui の通常の入力と同じ)
        h.frame(0.05, vec![enter()]);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "    abcd\n    ");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text, "    abcd");
        h.key(Key::Y, Modifiers::COMMAND);
        assert_eq!(h.text, "    abcd\n    ");
    }

    /// 同じフレームの入力の順序を保つ (先に来た文字の後ろで改行する)。
    /// Enter より前に本文を書き換える入力があるときは、その Enter を次のフレームへ持ち越して、
    /// 更新後の本文でインデントを求める
    #[test]
    fn auto_indent_keeps_event_order() {
        let mut h = Harness::with_indent("  a");
        h.select(3, 3);
        h.frame(0.05, vec![egui::Event::Text("b".into()), enter()]);
        assert_eq!(h.text, "  ab", "Enter は次のフレームへ持ち越される");
        assert_eq!(h.deferred.len(), 1);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "  ab\n  ");
        assert!(h.deferred.is_empty());
    }

    /// 同じフレームで、空白を入力してから Enter: 入力した空白がインデントとして引き継がれる
    #[test]
    fn auto_indent_sees_text_typed_earlier_in_the_frame() {
        let mut h = Harness::with_indent("");
        h.select(0, 0);
        h.frame(0.05, vec![egui::Event::Text("  ".into()), enter()]);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "  \n  ");
    }

    fn key_down(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// 同じフレームで、キャレットを動かす入力 (矢印キー) の後に Enter が来ても、
    /// 動いた後の行のインデントを使う (Enter とその後ろは次のフレームへ持ち越される)
    #[test]
    fn auto_indent_sees_caret_moved_earlier_in_the_frame() {
        let mut h = Harness::with_indent("  a\nb");
        h.select(5, 5); // 2 行目 "b" の末尾 (インデントなし)
        h.frame(0.05, vec![key_down(Key::ArrowUp), enter()]);
        assert_eq!(h.deferred.len(), 1, "Enter は持ち越される");
        h.frame(0.05, vec![]);
        // 1 行目 ("  a") の、キャレットの位置までのインデント (空白 1 つ) が引き継がれる
        assert_eq!(h.text, " \n  a\nb");
        assert!(h.deferred.is_empty());
        // Home → Enter も同様 (Home 後の位置は行頭 = インデントなし)
        let mut h = Harness::with_indent("  a");
        h.select(3, 3);
        h.frame(0.05, vec![key_down(Key::Home), enter()]);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "\n  a");
        // 矢印キーだけの後ろの Enter でなければ、これまでどおり同じフレームで処理される
        let mut h = Harness::with_indent("  a");
        h.select(3, 3);
        h.frame(0.05, vec![enter()]);
        assert!(h.deferred.is_empty());
        assert_eq!(h.text, "  a\n  ");
    }

    /// 複数行を貼り付けた直後の Enter は、貼り付けた最後の行のインデントを使う
    #[test]
    fn auto_indent_after_multiline_paste_uses_pasted_last_line() {
        let mut h = Harness::with_indent("    old");
        h.select(7, 7);
        h.frame(0.05, vec![egui::Event::Paste("\n\t\tx".into()), enter()]);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "    old\n\t\tx\n\t\t");
    }

    /// Enter が 2 回あるときも、2 回目は 1 回目の結果の本文で処理される (順序も保たれる)
    #[test]
    fn auto_indent_two_enters_in_one_frame() {
        let mut h = Harness::with_indent("  a");
        h.select(3, 3);
        h.frame(0.05, vec![enter(), enter(), egui::Event::Text("z".into())]);
        h.frame(0.05, vec![]);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "  a\n  \n  z");
    }

    /// インデントは本文を複製せずに、キャレットから行頭へバイト列をたどって求める
    #[test]
    fn line_indent_scans_bytes_back_from_caret() {
        assert_eq!(line_indent("  \tabc", 6), "  \t");
        assert_eq!(line_indent("a\n  b", 5), "  ");
        assert_eq!(line_indent("a\n  b", 3), " ");
        assert_eq!(line_indent("abc", 3), "");
        assert_eq!(line_indent("あ\n  い", search::char_to_byte("あ\n  い", 4)), "  ");
        assert_eq!(line_indent("  ", 100), "  ", "範囲外は末尾にそろえる");
    }

    /// Undo 後に改行すると、古い Redo は使えなくなる
    #[test]
    fn auto_indent_after_undo_clears_redo() {
        let mut h = Harness::with_indent("  a");
        h.select(3, 3);
        h.frame(2.0, vec![]);
        h.type_text("b");
        h.frame(2.0, vec![]);
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text, "  a");
        h.frame(0.05, vec![enter()]);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "  a\n  ");
        h.key(Key::Y, Modifiers::COMMAND);
        assert_eq!(h.text, "  a\n  ");
    }

    /// IME の確定 Enter (ImeGuard が取り除く) は、インデントも含め何も挿入しない
    #[test]
    fn auto_indent_ime_confirm_enter_inserts_nothing() {
        let mut h = Harness::with_indent("  ");
        let mut g = ImeGuard::default();
        h.select(2, 2);
        h.guarded(
            &mut g,
            0.05,
            vec![egui::Event::Ime(egui::ImeEvent::Preedit("あ".into()))],
        );
        h.guarded(
            &mut g,
            0.05,
            vec![
                enter(),
                egui::Event::Ime(egui::ImeEvent::Commit("あ".into())),
            ],
        );
        h.guarded(&mut g, 0.01, vec![enter()]); // 確定直後の遅れた Enter
        assert_eq!(h.text, "  あ");
        // 時間が経てば通常の Enter としてインデント付きの改行になる
        h.guarded(&mut g, 0.5, vec![enter()]);
        assert_eq!(h.text, "  あ\n  ");
    }

    /// Ctrl+Enter ではインデント処理をしない (イベントを取り除かない)
    #[test]
    fn auto_indent_ignores_ctrl_enter() {
        let mut h = Harness::with_indent("  a");
        h.select(3, 3);
        let ev = egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        };
        h.frame(0.05, vec![ev]);
        assert!(!h.text.ends_with("\n  "));
    }

    /// Shift+Enter など Enter 以外・修飾付きの扱いは変えない (変換中でなければ素通し)
    #[test]
    fn guard_passes_other_events() {
        let mut g = ImeGuard::default();
        let mut ev = vec![enter(), egui::Event::Text("a".into())];
        g.filter(1.0, &mut ev);
        assert_eq!(ev.len(), 2);
    }

    /// 変換中の範囲: Preedit で未確定文字列の範囲になり、Commit / 空 Preedit / Disabled で消える
    #[test]
    fn preedit_range_tracks_composition() {
        let mut h = Harness::new("ab");
        let mut g = ImeGuard::default();
        h.focus(ed());
        let base = editor_sel(&h.ctx, ed()).map_or(0, |s| s.1);
        let range =
            |h: &Harness, g: &ImeGuard| preedit_range(g.composing, editor_sel(&h.ctx, ed()));
        let ime = |e: egui::ImeEvent| vec![egui::Event::Ime(e)];
        assert_eq!(range(&h, &g), None, "変換前は範囲なし");
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Enabled));
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Preedit("か".into())));
        assert_eq!(range(&h, &g), Some((base, base + 1)));
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Preedit("かん".into())));
        assert_eq!(
            range(&h, &g),
            Some((base, base + 2)),
            "伸びても開始位置は同じ"
        );
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Preedit(String::new())));
        assert_eq!(range(&h, &g), None, "取り消し (空 Preedit)");
        assert!(!h.text.contains('か'));
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Preedit("字".into())));
        assert_eq!(range(&h, &g), Some((base, base + 1)));
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Commit("字".into())));
        assert_eq!(range(&h, &g), None, "確定後は範囲なし");
        assert!(h.text.contains('字'));
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Preedit("あ".into())));
        h.guarded(&mut g, 0.05, ime(egui::ImeEvent::Disabled));
        assert_eq!(range(&h, &g), None, "Disabled でも消える");
    }

    /// 改行をまたぐ未確定範囲が、行ごとの矩形に分割される
    #[test]
    fn preedit_rects_split_by_row() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {}); // フォントを初期化
        let font = egui::FontId::monospace(14.0);
        let galley = ctx.fonts(|f| {
            f.layout(
                "abcd\nefgh".into(),
                font.clone(),
                egui::Color32::WHITE,
                f32::INFINITY,
            )
        });
        assert_eq!(preedit_rects(&galley, (1, 3)).len(), 1);
        let r = preedit_rects(&galley, (3, 7)); // "d" + 改行 + "ef"
        assert_eq!(r.len(), 2);
        assert!(r[0].min.y < r[1].min.y);
        assert!(r[0].width() > 0.0 && r[1].width() > 0.0);
        assert!(
            preedit_rects(&galley, (4, 5)).is_empty(),
            "改行だけは描かない"
        );
    }
}

/// アプリ全体の動き (開く・保存・閉じる) を、画面を出さずに確かめるテスト
#[cfg(test)]
mod app_tests {
    use super::*;
    use std::path::Path;

    /// テストごとの作業フォルダ (終わると削除)
    struct Tmp(PathBuf);
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    impl Tmp {
        fn new() -> Self {
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let d =
                std::env::temp_dir().join(format!("rs_editor_app_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app() -> App {
        App::blank(egui::Context::default())
    }

    /// バックグラウンドの読み込みがすべて終わるまで待つ
    fn wait_loads(app: &mut App) {
        let ctx = app.ctx.clone();
        for _ in 0..1000 {
            app.poll_loads(&ctx);
            if app.loads.is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("読み込みが終わらない");
    }

    /// 実行中の読み込みを、スレッドの終了まで待って、結果を取り込む直前の状態にそろえる
    /// (結果は取り込まずに、次の `poll_loads` で必ず取り込まれるようにする。待ち時間に頼らない)
    fn settle_loads(app: &mut App) {
        for l in app.loads.iter_mut() {
            let r = l.rx.recv().unwrap_or_else(|_| {
                Err(fileio::LoadError::Message("読み込みが中断されました".into()))
            });
            let (tx, rx) = mpsc::channel();
            tx.send(r).unwrap();
            l.rx = rx;
        }
    }

    fn open(app: &mut App, p: &Path) {
        app.open_path(p.to_path_buf());
        wait_loads(app);
    }

    /// 文書を編集した状態にする (編集欄の変更と同じ手順)
    fn edit(app: &mut App, idx: usize, f: impl FnOnce(&mut String)) {
        let d = &mut app.docs[idx];
        f(&mut d.text);
        d.edited();
        d.update_dirty();
    }

    fn frame(
        ctx: &egui::Context,
        app: &mut App,
        t: &mut f64,
        events: Vec<egui::Event>,
        close: bool,
    ) -> egui::FullOutput {
        *t += 0.05;
        let mut raw = egui::RawInput {
            time: Some(*t),
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        if close {
            raw.viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .events
                .push(egui::ViewportEvent::Close);
        }
        ctx.run(raw, |ctx| app.update_ui(ctx, 0))
    }

    fn cancels_close(out: &egui::FullOutput) -> bool {
        out.viewport_output
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|v| {
                v.commands
                    .iter()
                    .any(|c| matches!(c, ViewportCommand::CancelClose))
            })
    }

    /// 現在の確認の画面で「保存する側」の答えを選んで実行する
    fn confirm(app: &mut App) {
        let ctx = app.ctx.clone();
        let act = match app.modal().cloned() {
            Some(Modal::Lossy {
                id,
                path,
                then,
                ok,
                ..
            }) => Action::SaveConfirmed {
                id,
                path,
                then,
                ok: Confirmed { lossy: true, ..ok },
            },
            Some(Modal::LossyLoad {
                id,
                path,
                then,
                ok,
            }) => Action::SaveConfirmed {
                id,
                path,
                then,
                ok: Confirmed {
                    lossy_load: true,
                    ..ok
                },
            },
            Some(Modal::Conflict {
                id,
                path,
                then,
                ok,
                seen,
            }) => Action::SaveConfirmed {
                id,
                path,
                then,
                ok: Confirmed {
                    conflict: Some(seen),
                    ..ok
                },
            },
            _ => panic!("確認の画面ではない"),
        };
        app.run(&ctx, act);
    }

    /// 同じファイルを開き直しても、新しいタブは増えず、既存のタブに切り替わる
    #[test]
    fn opening_same_file_activates_existing_tab() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"aaa");
        let b = t.file("b.txt", b"bbb");
        let mut app = app();
        // 読み込み中に同じファイルを 2 回頼んでも、読み込みは 1 つだけ
        app.open_path(a.clone());
        app.open_path(a.clone());
        assert_eq!(app.loads.len(), 1);
        wait_loads(&mut app);
        open(&mut app, &b);
        assert_eq!(app.docs.len(), 2);
        assert_eq!(app.active, 1);
        // 別の書き方 (./ を挟む、大文字小文字違い) でも同じファイル
        open(&mut app, &t.0.join(".").join("a.txt"));
        assert_eq!(app.docs.len(), 2);
        assert_eq!(app.active, 0);
        if cfg!(windows) {
            app.active = 1;
            open(&mut app, &t.0.join("A.TXT"));
            assert_eq!(app.docs.len(), 2);
            assert_eq!(app.active, 0);
        }
    }

    /// 開けなかったファイルは、エラーを表示する (タブは増えない)
    #[test]
    fn open_failure_shows_error() {
        let t = Tmp::new();
        let mut app = app();
        open(&mut app, &t.0.join("none.txt"));
        assert!(matches!(app.modal(), Some(Modal::Info(_))));
        assert_eq!(app.docs.len(), 1);
        assert!(app.docs[0].path.is_none());
        // UTF-32 は未対応として開かない (UTF-16 としても読める形なので、その選択肢を出す)
        let u32p = t.file("u32.txt", &[0xFF, 0xFE, 0, 0, b'a', 0, 0, 0]);
        app.modals.clear();
        open(&mut app, &u32p);
        assert!(matches!(
            app.modal(),
            Some(Modal::Utf32 { name: "UTF-32 LE", as_utf16: Some(Enc::Utf16Le), .. })
        ));
        assert!(app.docs[0].path.is_none());
        // 「UTF-16 として開く」を選ぶと、UTF-16 LE (BOM 付き) で開く
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::OpenAs(u32p.clone(), Enc::Utf16Le));
        wait_loads(&mut app);
        assert!(app.modal().is_none());
        assert_eq!(app.docs[0].path.as_deref(), Some(u32p.as_path()));
        assert_eq!(app.docs[0].enc, Enc::Utf16Le);
        assert_eq!(app.docs[0].text, "\u{0}a\u{0}");
        // UTF-32 BE は UTF-16 としては読めないので、選択肢は出さない
        let be = t.file("u32be.txt", &[0, 0, 0xFE, 0xFF, 0, 0, 0, b'a']);
        open(&mut app, &be);
        assert!(matches!(
            app.modal(),
            Some(Modal::Utf32 { name: "UTF-32 BE", as_utf16: None, .. })
        ));
    }

    /// 先頭が U+0000 の UTF-16 LE は、UTF-32 として断らずに UTF-16 で開く
    #[test]
    fn utf16_starting_with_nul_opens_as_utf16() {
        let t = Tmp::new();
        let p = t.file("nul.txt", &[0xFF, 0xFE, 0x00, 0x00, 0x41, 0x00]);
        let mut app = app();
        open(&mut app, &p);
        assert!(app.modal().is_none());
        assert_eq!(app.docs[0].enc, Enc::Utf16Le);
        assert_eq!(app.docs[0].text, "\u{0}A");
    }

    /// 大きなファイルは、読み込む前に確認する
    #[test]
    fn large_file_asks_before_loading() {
        let t = Tmp::new();
        let p = t.0.join("big.bin");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(fileio::LARGE_FILE_BYTES + 1).unwrap();
        drop(f);
        let mut app = app();
        app.open_path(p);
        assert!(matches!(app.modal(), Some(Modal::ConfirmLargeOpen { .. })));
        assert!(app.loads.is_empty());
    }

    /// 変更のない文書の上書き保存は何も書かない (デコードできなかったバイト・混在した改行を保つ)
    #[test]
    fn unmodified_save_writes_nothing() {
        let t = Tmp::new();
        let bad = [
            0xEF, 0xBB, 0xBF, b'a', 0xFF, b'b', b'\r', b'\n', b'c', b'\n', b'd', b'\r',
        ];
        let p = t.file("bad.txt", &bad);
        let mut app = app();
        open(&mut app, &p);
        assert!(app.docs[0].lossy_load);
        assert!(app.status.contains("デコード"));
        assert!(app.status.contains("混在"), "{}", app.status);
        assert!(app.status.contains("CRLF"));
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Save);
        assert!(app.status.contains("変更はありません"));
        assert!(app.modal().is_none());
        assert_eq!(std::fs::read(&p).unwrap(), bad);
    }

    /// 読み込み時にデコードできなかったバイトがあるファイルへの上書きは、確認する
    #[test]
    fn lossy_load_overwrite_asks_first() {
        let t = Tmp::new();
        let bad = [0xEF, 0xBB, 0xBF, b'a', 0xFF, b'b'];
        let p = t.file("bad.txt", &bad);
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::SetEol(Eol::Lf));
        assert!(app.docs[0].dirty);
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::LossyLoad { .. })));
        assert_eq!(std::fs::read(&p).unwrap(), bad, "確認の前には書かない");
        confirm(&mut app);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            [0xEF, 0xBB, 0xBF, b'a', 0xEF, 0xBF, 0xBD, b'b']
        );
        assert!(!app.docs[0].lossy_load);
        assert!(!app.docs[0].dirty);
        assert!(app.modal().is_none());
    }

    /// 別の名前への保存は、元のバイトを壊さないので確認しない
    #[test]
    fn lossy_load_save_as_other_path_needs_no_confirmation() {
        let t = Tmp::new();
        let p = t.file("bad.txt", &[0xEF, 0xBB, 0xBF, b'a', 0xFF, b'b']);
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        let other = t.0.join("other.txt");
        assert!(app.write_doc(&ctx, 0, other.clone(), Then::None, Confirmed::default()));
        assert!(other.exists());
        assert_eq!(std::fs::read(&p).unwrap(), [0xEF, 0xBB, 0xBF, b'a', 0xFF, b'b']);
    }

    /// 開いた後に外部で変更されたファイルへの上書きは確認し、確認後は閉じる操作も再開する
    #[test]
    fn external_change_asks_and_resumes_close() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        std::fs::write(&p, b"changed elsewhere").unwrap();
        edit(&mut app, 0, |s| s.push_str("-mine"));
        let ctx = app.ctx.clone();
        let id = app.docs[0].id;
        app.run(&ctx, Action::SaveAndClose(id));
        assert!(matches!(
            app.modal(),
            Some(Modal::Conflict {
                then: Then::Close(_),
                ..
            })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), b"changed elsewhere");
        confirm(&mut app);
        assert_eq!(std::fs::read(&p).unwrap(), b"one-mine");
        assert!(app.docs.iter().all(|d| d.id != id), "保存して閉じた");
    }

    /// 開いた後に削除されたファイルへの保存も、確認する
    #[test]
    fn deleted_file_asks_before_save() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        std::fs::remove_file(&p).unwrap();
        edit(&mut app, 0, |s| s.push('x'));
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        assert!(!p.exists());
        confirm(&mut app);
        assert!(p.exists());
        assert!(!app.docs[0].dirty);
    }

    /// 「すべて保存して終了」は、競合の確認で止まり、確認後に続きを行って終了する
    #[test]
    fn save_all_and_exit_stops_at_conflict_then_continues() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"a");
        let b = t.file("b.txt", b"b");
        let mut app = app();
        open(&mut app, &a);
        open(&mut app, &b);
        std::fs::write(&a, b"external").unwrap();
        edit(&mut app, 0, |s| s.push('1'));
        edit(&mut app, 1, |s| s.push('2'));
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::SaveAllAndExit);
        assert!(matches!(
            app.modal(),
            Some(Modal::Conflict {
                then: Then::SaveAll,
                ..
            })
        ));
        assert!(!app.allow_exit);
        assert_eq!(std::fs::read(&a).unwrap(), b"external");
        confirm(&mut app);
        assert!(app.modal().is_none());
        assert!(app.allow_exit, "確認の後に続きを行い、終了できる");
        assert!(app.docs.iter().all(|d| !d.dirty));
        assert_eq!(std::fs::read(&a).unwrap(), b"a1");
        assert_eq!(std::fs::read(&b).unwrap(), b"b2");
    }

    /// 別のタブで開いているファイルへは保存できない
    #[test]
    fn save_to_file_open_in_other_tab_is_refused() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"a");
        let b = t.file("b.txt", b"b");
        let mut app = app();
        open(&mut app, &a);
        open(&mut app, &b);
        edit(&mut app, 1, |s| s.push('!'));
        let ctx = app.ctx.clone();
        assert!(!app.write_doc(&ctx, 1, a.clone(), Then::None, Confirmed::default()));
        assert!(
            matches!(app.modal(), Some(Modal::Info(m)) if m.contains("別のタブで開いています"))
        );
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert!(app.docs[1].dirty);
    }

    /// 書き込みに失敗したら、エラーを表示して変更ありのまま (元のファイルも無事)
    #[test]
    fn write_failure_keeps_dirty_and_original() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"orig");
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm.clone()).unwrap();
        let mut app = app();
        open(&mut app, &p);
        edit(&mut app, 0, |s| s.push('x'));
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Info(m)) if m.contains("保存に失敗")));
        assert!(app.docs[0].dirty);
        assert_eq!(std::fs::read(&p).unwrap(), b"orig");
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm).unwrap();
    }

    /// Shift_JIS で別の文字になる文字 (¥) は警告し、保存後は画面の文字を保存内容にそろえる。
    /// 保存して閉じる操作も、確認の後に再開する
    #[test]
    fn lossy_save_warns_syncs_text_and_resumes() {
        let t = Tmp::new();
        let sjis = Enc::Legacy(encoding_rs::SHIFT_JIS);
        for close in [false, true] {
            let p = t.0.join(format!("sjis{close}.txt"));
            let mut app = app();
            let ctx = app.ctx.clone();
            let id = app.docs[0].id;
            app.docs[0].path = Some(p.clone());
            app.docs[0].enc = sjis;
            app.docs[0].eol = Eol::Lf;
            edit(&mut app, 0, |s| s.push_str("a\u{a5}b"));
            app.run(
                &ctx,
                if close {
                    Action::SaveAndClose(id)
                } else {
                    Action::Save
                },
            );
            match app.modal() {
                Some(Modal::Lossy { examples, then, .. }) => {
                    assert_eq!(examples, &vec!["\u{a5} → \\".to_string()]);
                    assert_eq!(*then == Then::Close(id), close);
                }
                m => panic!("警告が出ない: {}", m.is_some()),
            }
            assert!(!p.exists(), "確認の前には書かない");
            confirm(&mut app);
            assert_eq!(std::fs::read(&p).unwrap(), [b'a', 0x5C, b'b']);
            if close {
                assert!(app.docs.iter().all(|d| d.id != id), "保存して閉じる");
            } else {
                assert_eq!(app.docs[0].text, "a\\b", "画面の文字も保存内容に");
                assert!(!app.docs[0].dirty);
                // Ctrl+Z で ¥ に戻せるよう、履歴に積んである
                let state = TextEditState::load(&ctx, editor_id(id)).unwrap();
                let mut u = state.undoer();
                let cur = (CCursorRange::default(), app.docs[0].text.clone());
                assert!(u.undo(&cur).is_some_and(|(_, s)| s == "a\u{a5}b"));
            }
        }
    }

    /// 文字コードを指定した読み直し: 未保存の編集があれば確認し、ディスクから読み直す
    #[test]
    fn reload_confirms_when_dirty_and_rereads_disk() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"disk v1");
        let mut app = app();
        open(&mut app, &p);
        edit(&mut app, 0, |s| s.push_str(" + edit"));
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Reload(Enc::Utf8));
        assert!(matches!(app.modal(), Some(Modal::ConfirmReload { .. })));
        assert!(app.docs[0].dirty);
        assert!(app.loads.is_empty());
        // ディスクのファイルが変わっていたら、それを読む (メモリ上の古いバイトではなく)
        std::fs::write(&p, b"disk v2").unwrap();
        let id = app.docs[0].id;
        app.run(&ctx, Action::ReloadNow(id, Enc::Utf8));
        wait_loads(&mut app);
        assert_eq!(app.docs[0].text, "disk v2");
        assert!(!app.docs[0].dirty);
        // 読み直した後は、その状態が保存済みの基準になり、競合も起きない
        edit(&mut app, 0, |s| s.push('!'));
        app.run(&ctx, Action::Save);
        assert!(app.modal().is_none(), "{:?}", app.status);
        assert_eq!(std::fs::read(&p).unwrap(), b"disk v2!");
    }

    /// 読み直しに失敗したら、エラーを表示して文書はそのまま
    #[test]
    fn reload_failure_keeps_document() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"abc");
        let mut app = app();
        open(&mut app, &p);
        std::fs::remove_file(&p).unwrap();
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Reload(Enc::Utf8));
        wait_loads(&mut app);
        assert!(matches!(app.modal(), Some(Modal::Info(_))));
        assert_eq!(app.docs[0].text, "abc");
    }

    /// 読み直しで Undo 履歴が初期化される (読み直す前の履歴で別の内容に戻らない)
    #[test]
    fn reset_editor_state_clears_undo() {
        let ctx = egui::Context::default();
        let id = editor_id(7);
        let mut text = "abc".to_string();
        assert!(replace_text_with_undo(
            &ctx,
            id,
            &mut text,
            "xyz".into(),
            (0, 0)
        ));
        reset_editor_state(&ctx, id, text.len());
        let mut u = TextEditState::load(&ctx, id).unwrap().undoer();
        assert!(u.undo(&(CCursorRange::default(), text)).is_none());
    }

    /// 閉じる要求と最初の入力が同じ描画に届いても、未保存として確認する
    #[test]
    fn close_in_same_frame_as_first_edit_is_confirmed() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let mut t = 0.0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert!(!app.docs[0].dirty);
        let out = frame(
            &ctx,
            &mut app,
            &mut t,
            vec![egui::Event::Text("x".into())],
            true,
        );
        assert_eq!(app.docs[0].text, "x");
        assert!(app.docs[0].dirty);
        assert!(cancels_close(&out), "閉じるのを取り消す");
        assert!(matches!(app.modal(), Some(Modal::ConfirmExit)));
    }

    /// 未保存の文書がなければ、閉じる要求は取り消さない
    #[test]
    fn close_without_changes_is_not_cancelled() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let mut t = 0.0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        let out = frame(&ctx, &mut app, &mut t, vec![], true);
        assert!(!cancels_close(&out));
        assert!(app.modal().is_none());
    }

    /// 元の内容に戻したら、変更ありは解除される (Undo・改行コードの往復)
    #[test]
    fn dirty_clears_when_content_returns_to_saved() {
        let mut d = Doc::blank(1, 1);
        d.text = "hello".into();
        d.mark_saved();
        d.text.push('!');
        d.update_dirty();
        assert!(d.dirty);
        d.text.pop();
        d.update_dirty();
        assert!(!d.dirty, "Undo で保存時の内容に戻った");
        // 同じ長さで中身が違う
        d.text = "hellp".into();
        d.update_dirty();
        assert!(d.dirty);
        d.text = "hello".into();
        d.eol = Eol::Lf;
        d.update_dirty();
        assert!(d.dirty);
        d.eol = Eol::CrLf; // 初期値は CRLF
        d.update_dirty();
        assert!(!d.dirty, "CRLF → LF → CRLF で元に戻った");
        d.enc = Enc::Utf8Bom;
        d.update_dirty();
        assert!(d.dirty);
        d.enc = Enc::Utf8;
        d.update_dirty();
        assert!(!d.dirty);
    }

    /// 大きな文書は完全な比較をせず、編集したら変更ありのまま
    #[test]
    fn huge_documents_stay_dirty() {
        let mut d = Doc::blank(1, 1);
        d.text = "a".repeat(EXACT_DIRTY_LIMIT + 1);
        d.mark_saved();
        assert!(!d.saved.exact);
        d.update_dirty();
        assert!(d.dirty);
    }

    /// 文字数・行数は版が変わったときだけ数え直す
    #[test]
    fn stats_cached_per_revision() {
        let mut d = Doc::blank(1, 1);
        d.text = "あい\nう\n".into();
        assert_eq!(d.stats(), (5, 3));
        d.text.push_str("えお"); // 版を上げていないので、まだ古い値
        assert_eq!(d.stats(), (5, 3));
        d.rev += 1;
        assert_eq!(d.stats(), (7, 3));
    }

    /// 行番号用: 折り返した表示行も、論理行の番号を持つ
    #[test]
    fn row_line_numbers_follow_logical_lines() {
        assert_eq!(
            row_line_numbers([false, true, true, false, false, true].into_iter()),
            vec![1, 1, 2, 3, 3, 3]
        );
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let font = egui::FontId::monospace(14.0);
        let galley = ctx.fonts(|f| {
            f.layout(
                "aaaa bbbb cccc\nd\n\ne".into(),
                font,
                egui::Color32::WHITE,
                30.0, // 狭くして折り返させる
            )
        });
        let lines = row_line_numbers(galley.rows.iter().map(|r| r.ends_with_newline));
        assert!(galley.rows.len() > 4, "折り返している");
        assert_eq!(*lines.first().unwrap(), 1);
        assert_eq!(*lines.last().unwrap(), 4);
        // 見える範囲の先頭行を二分探索で求める
        let y0 = 100.0;
        assert_eq!(first_visible_row(&galley, y0, 0.0), 0);
        let second = &galley.rows[1].rect;
        let top = y0 + (second.min.y + second.max.y) / 2.0;
        assert_eq!(first_visible_row(&galley, y0, top), 1);
        assert_eq!(first_visible_row(&galley, y0, 1.0e6), galley.rows.len());
        // 同じ galley (Arc) なら作り直さない
        let mut cache = None;
        let p1 = row_lines_for(&mut cache, &galley).as_ptr();
        let p2 = row_lines_for(&mut cache, &galley).as_ptr();
        assert_eq!(p1, p2);
    }

    /// 貼り付け・入力の改行は LF にそろえる (単独の CR が本文に入らない)
    #[test]
    fn pasted_and_typed_text_newlines_are_normalized() {
        let mut evs = vec![
            egui::Event::Paste("a\r\nb\rc\nd".into()),
            egui::Event::Text("x\ry".into()),
            egui::Event::Text("plain".into()),
        ];
        normalize_text_events(&mut evs);
        assert_eq!(evs[0], egui::Event::Paste("a\nb\nc\nd".into()));
        assert_eq!(evs[1], egui::Event::Text("x\ny".into()));
        assert_eq!(evs[2], egui::Event::Text("plain".into()));
    }

    /// 確定の Enter を取り除くのは 1 回だけ。猶予の間でも、続けて来た Enter は本物の改行にする
    #[test]
    fn ime_guard_drops_only_one_enter_after_commit() {
        let mut g = ImeGuard::default();
        let ev = |e| egui::Event::Ime(e);
        let enter = || egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        g.filter(
            0.0,
            &mut vec![
                ev(egui::ImeEvent::Enabled),
                ev(egui::ImeEvent::Preedit("あ".into())),
            ],
        );
        let mut evs = vec![ev(egui::ImeEvent::Commit("あ".into())), enter(), enter()];
        g.filter(0.01, &mut evs);
        let enters = evs
            .iter()
            .filter(|e| matches!(e, egui::Event::Key { .. }))
            .count();
        assert_eq!(enters, 1, "1 回目だけ取り除く");
        // 次のフレームでも (猶予の間に) 来た Enter は通る
        let mut evs = vec![enter()];
        g.filter(0.02, &mut evs);
        assert_eq!(evs.len(), 1);
        // 新しい確定のたびに、また 1 回取り除ける
        g.filter(1.0, &mut vec![ev(egui::ImeEvent::Preedit("い".into()))]);
        let mut evs = vec![ev(egui::ImeEvent::Commit("い".into()))];
        g.filter(1.01, &mut evs);
        let mut evs = vec![enter()];
        g.filter(1.02, &mut evs);
        assert!(evs.is_empty());
    }

    /// タブの列は、選択が変わったときだけ選択中のタブへスクロールする
    #[test]
    fn tab_bar_scroll_target_follows_selection() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        for _ in 0..30 {
            app.new_doc();
        }
        let mut t = 0.0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert_eq!(app.tab_shown, Some(app.docs[app.active].id));
        app.active = 0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert_eq!(app.tab_shown, Some(app.docs[0].id));
    }

    /// 検索の強調付き・検索バー (折り返し付き) を開いたままでも描画でき、本文が変わると
    /// 強調と一致の一覧が新しい本文に追従する。galley は自前で使い回さず、egui のキャッシュに任せる
    #[test]
    fn highlight_follows_text_without_own_galley_cache() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        app.docs[0].text = "abc abc\nxyz abc".into();
        app.docs[0].edited();
        app.find.open = true;
        app.find.show_replace = true;
        app.find.query = "abc".into();
        let mut t = 0.0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert_eq!(app.docs[0].matches().len(), 3);
        // 入力で本文が変わると、次の描画で一致の一覧も作り直される
        frame(&ctx, &mut app, &mut t, vec![egui::Event::Text("abc".into())], false);
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert!(app.docs[0].text.contains("abc") && app.docs[0].text.len() > 15);
        assert_eq!(
            app.docs[0].matches().len(),
            search::find_all(&app.docs[0].text, "abc", false).len()
        );
        assert_eq!(app.docs[0].rev, 2);
        // 自前の galley キャッシュは持たない (メモリ・フォントアトラス作り直しの問題を避ける)
        assert!(app.docs[0].row_lines.is_some());
    }

    /// 行番号用の表は、同じ galley のときだけ使い回し、galley が変われば (フォントアトラスの
    /// 作り直しや本文の変更で egui が新しい galley を作れば) 作り直す。galley 自体は持たない
    #[test]
    fn row_lines_do_not_keep_galley_alive() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let font = egui::FontId::monospace(14.0);
        let mut cache = None;
        let lay = |text: &str| {
            ctx.fonts(|f| f.layout(text.into(), font.clone(), egui::Color32::WHITE, 100.0))
        };
        let g1 = lay("a\nb\nc");
        assert_eq!(row_lines_for(&mut cache, &g1), &[1, 2, 3]);
        // 表が持つのは Weak だけ (galley の寿命は egui のキャッシュが決める)
        assert!(cache.as_ref().is_some_and(|c| c.galley.strong_count() >= 1));
        let g2 = lay("a\nb");
        assert_eq!(row_lines_for(&mut cache, &g2), &[1, 2], "別の galley なら作り直す");
    }

    // ---- 読み直し (項目 1)

    /// 読み込み中に編集した文書へ、読み直しの結果を上書きしない
    #[test]
    fn reload_result_discarded_if_edited_while_loading() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"disk v1");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Reload(Enc::Utf8)); // 未保存の編集は無いので、そのまま始まる
        assert_eq!(app.loads.len(), 1);
        edit(&mut app, 0, |s| s.push_str(" + typing"));
        wait_loads(&mut app);
        assert_eq!(app.docs[0].text, "disk v1 + typing", "編集は消えない");
        assert!(app.docs[0].dirty);
        assert!(app.status.contains("読み込み中に変更されたため、開き直しを中止しました"));
    }

    /// 読み込み中に名前を付けて保存した文書へ、元のファイルの内容と指紋を入れない
    #[test]
    fn reload_result_discarded_if_saved_as_elsewhere_while_loading() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"from A");
        let mut app = app();
        open(&mut app, &a);
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Reload(Enc::Utf8));
        edit(&mut app, 0, |s| s.push_str("!"));
        let b = t.0.join("b.txt");
        assert!(app.write_doc(&ctx, 0, b.clone(), Then::None, Confirmed::default()));
        let fp_b = app.docs[0].fingerprint.clone();
        wait_loads(&mut app);
        assert_eq!(app.docs[0].path.as_deref(), Some(b.as_path()));
        assert_eq!(app.docs[0].text, "from A!");
        assert_eq!(app.docs[0].fingerprint, fp_b, "B の指紋のまま");
        assert!(!app.docs[0].dirty);
        assert!(app.status.contains("中止"));
    }

    /// 文字コード・改行コードを変えた場合も、読み込み中の結果は捨てる
    #[test]
    fn reload_result_discarded_if_encoding_changed_while_loading() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"abc");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Reload(Enc::Utf8));
        app.run(&ctx, Action::SetEol(Eol::Lf));
        wait_loads(&mut app);
        assert_eq!(app.docs[0].eol, Eol::Lf);
        assert!(app.docs[0].dirty);
        // 何も変わっていなければ取り込まれる
        app.run(&ctx, Action::SetEol(Eol::CrLf));
        app.run(&ctx, Action::Reload(Enc::Legacy(encoding_rs::SHIFT_JIS)));
        wait_loads(&mut app);
        assert!(app.status.contains("再読み込み"), "{}", app.status);
    }

    // ---- 外部変更の承知 (項目 3)

    /// 「上書きする」と答えた後、別の確認を出している間にファイルがもう一度変わったら、
    /// 気づかずに上書きせず、もう一度確認する
    #[test]
    fn second_external_change_after_approval_asks_again() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let sjis = Enc::Legacy(encoding_rs::SHIFT_JIS);
        app.docs[0].enc = sjis;
        app.docs[0].eol = Eol::Lf;
        edit(&mut app, 0, |s| s.push('\u{a5}'));
        std::fs::write(&p, b"first external change").unwrap();
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        confirm(&mut app); // 1 回目の変更は承知。次に文字コードの警告が出る
        assert!(matches!(app.modal(), Some(Modal::Lossy { .. })), "{:?}", app.status);
        // 警告を出している間に、ファイルが 2 回目の変更を受ける
        std::fs::write(&p, b"second, different external change").unwrap();
        confirm(&mut app);
        assert!(
            matches!(app.modal(), Some(Modal::Conflict { .. })),
            "もう一度確認する"
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"second, different external change");
        assert!(app.docs[0].dirty);
        // 新しい状態を承知して上書きすれば保存できる (文字コードの警告には答え済み)
        confirm(&mut app);
        assert!(app.modal().is_none(), "{:?}", app.status);
        assert_eq!(std::fs::read(&p).unwrap(), [b'o', b'n', b'e', 0x5C]);
    }

    /// 新しいファイルの保存先に、確認の後で同じ名前のファイルができていたら、上書きせず確認する
    #[test]
    fn late_created_file_is_not_overwritten() {
        let t = Tmp::new();
        let mut app = app();
        let ctx = app.ctx.clone();
        let target = t.0.join("new.txt");
        edit(&mut app, 0, |s| s.push_str("mine"));
        // 保存先は存在しない → そのつもりで書き始めるが、直前に他のプログラムが作る
        // (write_doc の途中は割り込めないので、期待が崩れた状況を直接 fileio で再現する)
        std::fs::write(&target, b"theirs").unwrap();
        let r = fileio::atomic_write(&target, b"mine", &fileio::Expect::Is(None));
        assert_eq!(r, Err(fileio::WriteError::Conflict));
        assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
        // アプリ経由でも、今ある状態を見て確認なしに上書きすることはない
        // (名前を付けて保存で既存ファイルを選んだときは、OS のダイアログが確認済み)
        assert!(app.write_doc(&ctx, 0, target.clone(), Then::None, Confirmed::default()));
        assert_eq!(std::fs::read(&target).unwrap(), b"mine");
    }

    // ---- フォーカス・取り込みの順序 (項目 7)

    /// 読み込み中にユーザーが別のタブへ移っていたら、開いたタブを選択しない
    #[test]
    fn finished_open_does_not_steal_active_tab() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"aaa");
        let b = t.file("b.txt", b"bbb");
        let mut app = app();
        open(&mut app, &a);
        app.open_path(b.clone());
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::New); // 読み込み中に、新しいタブへ移った
        let before = app.active;
        wait_loads(&mut app);
        assert_eq!(app.docs.len(), 3, "タブは追加される");
        assert_eq!(app.active, before, "選択は動かない");
        assert!(app.docs[app.active].path.is_none());
        assert!(app.status.contains("「b.txt」を開きました"), "{}", app.status);
        // 何も変えなければ、従来どおり開いたタブを選択する
        let c = t.file("c.txt", b"ccc");
        open(&mut app, &c);
        assert_eq!(app.docs[app.active].title(), "c.txt");
    }

    /// 読み込みが終わるのと同じフレームのキー入力は、元のタブへ届く (取り込みは編集欄の後)。
    /// そのフレームで元のタブを編集した (打ち続けている) ので、新しいタブは選択せず、
    /// 背景で開く (以降の入力が新しいタブへ入らない)
    #[test]
    fn completion_in_same_frame_does_not_retarget_input() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"aaa");
        let b = t.file("b.txt", b"bbb");
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let mut clock = 0.0;
        app.open_path(a);
        wait_loads(&mut app);
        for _ in 0..3 {
            frame(&ctx, &mut app, &mut clock, vec![], false); // 編集欄にフォーカスが付く
        }
        app.open_path(b);
        // 読み込みのスレッドが終わるまで待つ (結果の取り込みは、次のフレームの最後に行われる)
        settle_loads(&mut app);
        frame(
            &ctx,
            &mut app,
            &mut clock,
            vec![egui::Event::Text("x".into())],
            false,
        );
        let a_idx = app
            .docs
            .iter()
            .position(|d| d.title() == "a.txt")
            .expect("a が開いている");
        let b_idx = app
            .docs
            .iter()
            .position(|d| d.title() == "b.txt")
            .expect("b も取り込まれている");
        assert!(app.docs[a_idx].text.contains('x'), "入力は元のタブへ: {:?}", app.docs[a_idx].text);
        assert_eq!(app.docs[b_idx].text, "bbb", "新しいタブには入らない");
        assert_eq!(app.active, a_idx, "打っている最中なので、新しいタブは選択しない");
        assert!(app.status.contains("「b.txt」を開きました"), "{}", app.status);
    }

    // ---- 確認の待ち行列 (項目 8)

    /// 大きなファイルを 2 つ続けて開こうとしても、確認はどちらも残る
    #[test]
    fn two_large_open_confirmations_are_both_kept() {
        let t = Tmp::new();
        let mk = |name: &str| {
            let p = t.0.join(name);
            let f = std::fs::File::create(&p).unwrap();
            f.set_len(fileio::LARGE_FILE_BYTES + 1).unwrap();
            p
        };
        let (a, b) = (mk("big_a.bin"), mk("big_b.bin"));
        let mut app = app();
        app.open_path(a.clone());
        app.open_path(b.clone());
        assert_eq!(app.modals.len(), 2);
        assert!(matches!(app.modal(), Some(Modal::ConfirmLargeOpen { path, .. }) if *path == a));
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::DismissModal);
        assert!(matches!(app.modal(), Some(Modal::ConfirmLargeOpen { path, .. }) if *path == b));
    }

    /// 保存の途中の確認 (続きの操作つき) は、バックグラウンドの読み込みの失敗の通知で消えない
    #[test]
    fn background_failure_does_not_replace_save_flow_modal() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        std::fs::write(&p, b"changed elsewhere").unwrap();
        edit(&mut app, 0, |s| s.push_str("-mine"));
        let ctx = app.ctx.clone();
        let id = app.docs[0].id;
        app.run(&ctx, Action::SaveAndClose(id));
        assert!(matches!(app.modal(), Some(Modal::Conflict { then: Then::Close(_), .. })));
        // 読み込みの失敗が届く
        open(&mut app, &t.0.join("missing.txt"));
        assert_eq!(app.modals.len(), 2);
        assert!(
            matches!(app.modal(), Some(Modal::Conflict { then: Then::Close(_), .. })),
            "保存の確認が先頭のまま"
        );
        confirm(&mut app);
        assert!(app.docs.iter().all(|d| d.id != id), "続きの操作 (閉じる) が行われた");
        assert!(matches!(app.modal(), Some(Modal::Info(_))), "通知はその後で出る");
    }

    // ---- 閉じる要求と画面の操作 (項目 9)

    /// 「すべて置換」と閉じる要求が同じフレームに来ても、置換後の未保存の変更を確認する
    #[test]
    fn replace_all_and_close_in_same_frame_is_confirmed() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        app.docs[0].text = "abc abc".into();
        app.docs[0].mark_saved();
        app.docs[0].edited();
        app.find.open = true;
        app.find.show_replace = true;
        app.find.query = "abc".into();
        app.find.replacement = "x".into();
        let mut t = 0.0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert!(!app.docs[0].dirty);
        // 閉じる要求と、「すべて置換」のクリックが同じフレームに来る
        t += 0.05;
        let mut raw = egui::RawInput {
            time: Some(t),
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        raw.viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let out = ctx.run(raw, |ctx| {
            app.update_ui_with(ctx, 0, vec![Action::ReplaceAll]);
        });
        assert_eq!(app.docs[0].text, "x x");
        assert!(app.docs[0].dirty);
        assert!(cancels_close(&out), "閉じるのを取り消す");
        assert!(matches!(app.modal(), Some(Modal::ConfirmExit)));
        // 確認が重複して積まれない
        let mut raw = egui::RawInput {
            time: Some(t + 0.05),
            ..Default::default()
        };
        raw.viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(raw, |ctx| app.update_ui(ctx, 0));
        assert_eq!(app.modals.len(), 1);
    }

    // ---- Undo 履歴のメモリ (項目 10)

    /// タブを閉じると、編集欄の状態 (Undo 履歴) も egui の記憶から取り除く
    #[test]
    fn closing_tab_forgets_editor_state() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        app.new_doc();
        let id = app.docs[1].id;
        let mut text = "abc".to_string();
        assert!(replace_text_with_undo(
            &ctx,
            editor_id(id),
            &mut text,
            "xyz".into(),
            (0, 0)
        ));
        assert!(TextEditState::load(&ctx, editor_id(id)).is_some());
        assert_eq!(recorded_undo_limit(&ctx, editor_id(id)), 100);
        app.close_now(1);
        assert!(TextEditState::load(&ctx, editor_id(id)).is_none());
        // 置き換えられた (空のタブ → 開いたファイル) 文書の状態も残さない
        let t = Tmp::new();
        let p = t.file("a.txt", b"aaa");
        let blank_id = app.docs[0].id;
        let mut s = String::new();
        assert!(replace_text_with_undo(&ctx, editor_id(blank_id), &mut s, "".into(), (0, 0)));
        open(&mut app, &p);
        assert!(TextEditState::load(&ctx, editor_id(blank_id)).is_none());
    }

    /// 大きな文書ほど履歴の件数を絞る (全体で約 256 MiB に収める)
    #[test]
    fn undo_history_is_bounded_by_document_size() {
        assert_eq!(undo_limit(0), 100);
        assert_eq!(undo_limit(1024 * 1024), 100);
        assert_eq!(undo_limit(10 * 1024 * 1024), 25);
        assert_eq!(undo_limit(64 * 1024 * 1024), 4);
        assert_eq!(undo_limit(1024 * 1024 * 1024), 2, "最小 2 件");
        let ctx = egui::Context::default();
        let id = editor_id(11);
        reset_editor_state(&ctx, id, 64 * 1024 * 1024);
        assert_eq!(recorded_undo_limit(&ctx, id), 4);
        // 実際に、件数を超えた古い履歴は捨てられる
        let mut u = bounded_undoer(2);
        for i in 0..6 {
            u.add_undo(&(CCursorRange::default(), format!("state {i}")));
        }
        let mut cur = (CCursorRange::default(), "state 6".to_string());
        let mut steps = 0;
        while let Some(prev) = u.undo(&cur).cloned() {
            cur = prev;
            steps += 1;
            assert!(steps < 10);
        }
        assert!(steps <= 2, "{steps}");
        // 大きな文書の置換では、履歴を絞った新しい履歴にする
        let id2 = editor_id(12);
        let mut text = "a".repeat(8 * 1024 * 1024);
        let new_text = "b".repeat(8 * 1024 * 1024);
        assert!(replace_text_with_undo(&ctx, id2, &mut text, new_text, (0, 0)));
        assert_eq!(recorded_undo_limit(&ctx, id2), undo_limit(8 * 1024 * 1024));
        assert_eq!(text.as_bytes()[0], b'b');
    }

    // ---- 保存の安全 (項目 11)

    /// 保存に失敗したら、画面の文字も Undo 履歴も変えない (準備は書き込みの前に済ませるが、
    /// 反映は書き込みの成功の後だけ)
    #[test]
    fn failed_lossy_save_leaves_text_and_history_untouched() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"x");
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm.clone()).unwrap();
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        let id = app.docs[0].id;
        app.docs[0].enc = Enc::Legacy(encoding_rs::SHIFT_JIS);
        edit(&mut app, 0, |s| s.push('\u{a5}'));
        app.run(&ctx, Action::Save);
        confirm(&mut app); // 文字コードの警告に「それでも保存」
        assert!(matches!(app.modal(), Some(Modal::Info(m)) if m.contains("保存に失敗")));
        assert_eq!(app.docs[0].text, "x\u{a5}", "画面の文字はそのまま");
        assert!(app.docs[0].dirty);
        // Undo 履歴も触らない (開いたときの空の履歴のまま)
        let mut u = TextEditState::load(&ctx, editor_id(id)).unwrap().undoer();
        let cur = (CCursorRange::default(), app.docs[0].text.clone());
        assert!(u.undo(&cur).is_none(), "履歴も触らない");
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm).unwrap();
    }

    /// 準備した置換は、反映するまで本文も履歴も変えず、反映後は 1 回の Ctrl+Z で戻せる
    #[test]
    fn prepared_replace_applies_only_on_apply() {
        let ctx = egui::Context::default();
        let id = editor_id(21);
        let mut text = "a\u{a5}b".to_string();
        let p = prepare_replace(&ctx, id, &text, "a\\b".into(), (0, 0)).unwrap();
        assert_eq!(text, "a\u{a5}b");
        assert!(TextEditState::load(&ctx, id).is_none());
        p.apply(&ctx, id, &mut text);
        assert_eq!(text, "a\\b");
        let mut u = TextEditState::load(&ctx, id).unwrap().undoer();
        let cur = (CCursorRange::default(), text.clone());
        assert!(u.undo(&cur).is_some_and(|(_, s)| s == "a\u{a5}b"));
    }

    // ---- 検索 (項目 5 / 16)

    /// 短い検索語を大きな文書で探しても、保存する一致は上限まで。
    /// カーソルが一覧の外 (上限より後ろ) にあっても、次・前へ移動できる
    #[test]
    fn find_navigation_works_beyond_stored_matches() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let n = search::MAX_STORED_MATCHES + 50_000;
        app.docs[0].text = "a".repeat(n);
        app.docs[0].edited();
        app.find.query = "a".into();
        app.docs[0].ensure_matches("a", false);
        let c = app.docs[0].find_cache.as_ref().unwrap();
        assert_eq!(c.matches.len(), search::MAX_STORED_MATCHES);
        assert_eq!(c.byte_ranges.len(), search::MAX_STORED_MATCHES);
        assert!(c.capped);
        let id = editor_id(app.docs[0].id);
        let far = search::MAX_STORED_MATCHES + 1_000;
        set_editor_sel(&ctx, id, far, far);
        app.run(&ctx, Action::FindNext);
        assert_eq!(editor_sel(&ctx, id), Some((far, far + 1)));
        app.run(&ctx, Action::FindNext);
        assert_eq!(editor_sel(&ctx, id), Some((far + 1, far + 2)));
        app.run(&ctx, Action::FindPrev);
        assert_eq!(editor_sel(&ctx, id), Some((far, far + 1)));
        assert!(app.status.contains("選択しました"), "{}", app.status);
        // 末尾から次へ進むと、先頭へ折り返す
        set_editor_sel(&ctx, id, n, n);
        app.run(&ctx, Action::FindNext);
        assert_eq!(editor_sel(&ctx, id), Some((0, 1)));
        assert!(app.status.contains("100,000 件以上"), "{}", app.status);
    }

    /// 「すべて置換」は一致の一覧を作らずに走査し、置換後の大きさの上限は先に判定する
    #[test]
    fn replace_all_streams_without_match_list() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        app.docs[0].text = "a".repeat(500_000);
        app.docs[0].edited();
        app.find.query = "a".into();
        app.find.replacement = "bb".into();
        app.run(&ctx, Action::ReplaceAll);
        assert_eq!(app.docs[0].text.len(), 1_000_000);
        assert!(app.docs[0].text.bytes().all(|b| b == b'b'));
        assert!(app.docs[0].find_cache.is_none(), "一覧は作らない");
        assert!(app.status.contains("500000 件"), "{}", app.status);
        // Ctrl+Z 相当で元に戻せる
        let mut u = TextEditState::load(&ctx, editor_id(app.docs[0].id)).unwrap().undoer();
        let cur = (CCursorRange::default(), app.docs[0].text.clone());
        assert!(u.undo(&cur).is_some_and(|(_, s)| s.len() == 500_000));
        // 上限を超える置換は、何も変えずに断る
        let mut app = App::blank(egui::Context::default());
        app.docs[0].text = "a".repeat(2000);
        app.find.query = "a".into();
        app.find.replacement = "x".repeat(300_000);
        let ctx = app.ctx.clone();
        app.run(&ctx, Action::ReplaceAll);
        assert_eq!(app.docs[0].text.len(), 2000);
        assert!(matches!(app.modal(), Some(Modal::Info(m)) if m.contains("上限")));
    }

    /// 1 件ずつの置換は、一覧の外 (上限より後ろ) の一致も置換できる
    #[test]
    fn replace_one_works_beyond_stored_matches() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let n = search::MAX_STORED_MATCHES + 10;
        app.docs[0].text = "a".repeat(n);
        app.docs[0].edited();
        app.find.query = "a".into();
        app.find.replacement = "Z".into();
        let id = editor_id(app.docs[0].id);
        set_editor_sel(&ctx, id, n - 3, n - 2);
        app.run(&ctx, Action::ReplaceOne);
        assert_eq!(app.docs[0].text.chars().nth(n - 3), Some('Z'));
        assert_eq!(app.docs[0].text.matches('Z').count(), 1);
        // 次の一致が選択されている
        assert_eq!(editor_sel(&ctx, id), Some((n - 2, n - 1)));
    }

    /// 現在位置は 1 フレームに 1 回求め、一覧が昇順なので二分探索で引く
    #[test]
    fn find_position_is_computed_once_per_frame() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        app.docs[0].text = "ab ab ab".into();
        app.docs[0].edited();
        app.find.open = true;
        app.find.query = "ab".into();
        let id = editor_id(app.docs[0].id);
        let mut t = 0.0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        set_editor_sel(&ctx, id, 3, 5);
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert_eq!(app.find_pos.exact, Some(1));
        assert_eq!(app.find_pos.current, Some(1));
        set_editor_sel(&ctx, id, 5, 5);
        frame(&ctx, &mut app, &mut t, vec![], false);
        assert_eq!(app.find_pos.exact, None);
        assert_eq!(app.find_pos.current, Some(2), "選択の後ろの次の一致");
        assert_eq!(with_commas(100_000), "100,000");
        assert_eq!(with_commas(999), "999");
        assert_eq!(with_commas(1_234_567), "1,234,567");
    }

    // ---- 改行混在の表示 (項目 19)

    /// 改行コードの混在は、保存して統一するまで文書ごとに覚えておく
    /// (他のタブの読み込みなどで状態表示が変わっても消えない)
    #[test]
    fn mixed_eol_is_kept_per_document_until_saved() {
        let t = Tmp::new();
        let m = t.file("mixed.txt", b"a\r\nb\nc\r");
        let other = t.file("other.txt", b"plain");
        let mut app = app();
        open(&mut app, &m);
        assert!(app.docs[0].mixed_eol);
        open(&mut app, &other);
        assert!(!app.docs[1].mixed_eol);
        assert!(app.docs[0].mixed_eol, "他のタブを開いても残る");
        assert!(!app.status.contains("混在"), "状態表示からは消える");
        // 描画できる (状態表示に、タブごとの表示が出る)
        let ctx = app.ctx.clone();
        app.active = 0;
        let mut clock = 0.0;
        frame(&ctx, &mut app, &mut clock, vec![], false);
        // 保存すると解除される
        edit(&mut app, 0, |s| s.push('!'));
        app.run(&ctx, Action::Save);
        assert!(!app.docs[0].mixed_eol);
        // 読み直しで改行が混在していれば、また立つ
        std::fs::write(&m, b"x\r\ny\n").unwrap();
        app.run(&ctx, Action::Reload(Enc::Utf8));
        wait_loads(&mut app);
        assert!(app.docs[0].mixed_eol);
    }

    /// 変更の世代: 本文の編集・保存・文字コード/改行コードの変更で進む
    #[test]
    fn document_generation_tracks_every_change() {
        let t = Tmp::new();
        let p = t.file("g.txt", b"abc");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        let g0 = app.docs[0].generation;
        edit(&mut app, 0, |s| s.push('x'));
        let g1 = app.docs[0].generation;
        assert!(g1 > g0);
        app.run(&ctx, Action::SetEol(Eol::Lf));
        let g2 = app.docs[0].generation;
        assert!(g2 > g1);
        app.run(&ctx, Action::Convert(Enc::Utf8Bom));
        let g3 = app.docs[0].generation;
        assert!(g3 > g2);
        app.run(&ctx, Action::Save);
        assert!(app.docs[0].generation > g3);
    }

    // ---- 第 3 回レビュー

    /// 名前を付けて保存で、文字コードの警告を出している間に他のプログラムが保存先を作ったら、
    /// 警告に答えた後でも気づかずに上書きせず、確認する (保存先を選んだ時点の状態を持ち回る)
    #[test]
    fn save_as_expectation_survives_lossy_warning() {
        let t = Tmp::new();
        let mut app = app();
        let ctx = app.ctx.clone();
        app.docs[0].enc = Enc::Legacy(encoding_rs::SHIFT_JIS);
        edit(&mut app, 0, |s| s.push('\u{a5}'));
        let target = t.0.join("B.txt");
        // 保存先は存在しない。文字コードの警告が出る
        assert!(!app.write_doc(&ctx, 0, target.clone(), Then::None, Confirmed::default()));
        assert!(matches!(app.modal(), Some(Modal::Lossy { .. })));
        // 警告を出している間に、他のプログラムが保存先を作る
        std::fs::write(&target, b"theirs").unwrap();
        confirm(&mut app); // 「それでも保存」
        assert!(
            matches!(app.modal(), Some(Modal::Conflict { .. })),
            "確認し直す: {:?}",
            app.status
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"theirs", "上書きしない");
        assert!(app.docs[0].dirty);
        // 新しい状態を承知して上書きすれば保存できる (文字コードの警告には答え済み)
        confirm(&mut app);
        assert!(app.modal().is_none(), "{:?}", app.status);
        assert_eq!(std::fs::read(&target).unwrap(), [0x5C]);
        assert!(!app.docs[0].dirty);
    }

    /// 同じく、他のプログラムが保存先を更新した場合 (すでにある保存先を選んだとき)
    #[test]
    fn save_as_existing_target_updated_during_warning_asks_again() {
        let t = Tmp::new();
        let target = t.file("B.txt", b"old");
        let mut app = app();
        let ctx = app.ctx.clone();
        app.docs[0].enc = Enc::Legacy(encoding_rs::SHIFT_JIS);
        edit(&mut app, 0, |s| s.push('\u{a5}'));
        assert!(!app.write_doc(&ctx, 0, target.clone(), Then::None, Confirmed::default()));
        assert!(matches!(app.modal(), Some(Modal::Lossy { .. })));
        std::fs::write(&target, b"updated by another program").unwrap();
        confirm(&mut app);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        assert_eq!(std::fs::read(&target).unwrap(), b"updated by another program");
    }

    /// 保存の直後に保存先を確かめられなかったとき (`disk_unverified`) は、次の保存の前に確認する
    #[test]
    fn unverified_disk_state_asks_before_next_save() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        edit(&mut app, 0, |s| s.push('!'));
        app.docs[0].fingerprint = None;
        app.docs[0].disk_unverified = true;
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        confirm(&mut app);
        assert_eq!(std::fs::read(&p).unwrap(), b"one!");
        assert!(!app.docs[0].disk_unverified, "確認して保存すれば元に戻る");
        assert!(app.docs[0].fingerprint.is_some());
        // 通常の保存では、保存先の指紋は書いた内容そのもの (確かめられた状態)
        edit(&mut app, 0, |s| s.push('?'));
        app.run(&ctx, Action::Save);
        assert!(app.modal().is_none());
        assert_eq!(std::fs::read(&p).unwrap(), b"one!?");
        assert_eq!(app.docs[0].fingerprint, fileio::fingerprint(&p));
    }

    /// 実際の保存で、置き換えの直後に保存先が差し替わった (`Written::Unverified`) とき:
    /// 保存済みにせず (変更ありのまま)、タブを閉じる・終了する操作を止めて、確認の画面を出す。
    /// 変更がなくても、次の保存は (「変更はありません」で終わらず) 確認に進む
    #[test]
    fn unverified_save_keeps_tab_dirty_and_stops_close_and_exit() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        let id = app.docs[0].id;
        edit(&mut app, 0, |s| s.push_str("-mine"));
        fileio::test_hooks::after_replace(|t| std::fs::write(t, b"synced elsewhere").unwrap());
        app.run(&ctx, Action::SaveAndClose(id));
        assert_eq!(std::fs::read(&p).unwrap(), b"synced elsewhere");
        assert!(matches!(app.modal(), Some(Modal::Unverified { preserved: None, .. })));
        assert_eq!(app.docs.len(), 1);
        assert_eq!(app.docs[0].id, id, "保存して閉じる、は閉じない");
        assert!(app.docs[0].dirty, "保存済みにしない");
        assert!(app.docs[0].disk_unverified);
        assert_eq!(app.docs[0].text, "one-mine", "編集内容は残っている");
        // 確認の画面を閉じた後、変更がなくても次の保存は確認に進む
        app.run(&ctx, Action::DismissModal);
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        assert!(!app.status.contains("変更はありません"));
        assert_eq!(std::fs::read(&p).unwrap(), b"synced elsewhere");
        app.run(&ctx, Action::DismissModal);
        // すべて保存して終了、も止まる
        fileio::test_hooks::after_replace(|t| std::fs::write(t, b"synced again").unwrap());
        app.docs[0].disk_unverified = false;
        app.docs[0].fingerprint = fileio::fingerprint(&p);
        app.run(&ctx, Action::SaveAllAndExit);
        assert!(!app.allow_exit, "終了しない");
        assert!(matches!(app.modal(), Some(Modal::Unverified { .. })));
        assert!(app.docs[0].dirty);
    }

    /// 保存の直後に保存先を確かめられなかった文書は、保存済みの版へ Undo で戻しても、
    /// 変更ありのまま。タブを閉じる・終了する操作は、確認なしに内容を捨てない
    #[test]
    fn undo_to_old_snapshot_after_unverified_save_still_asks_before_close() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        let id = app.docs[0].id;
        edit(&mut app, 0, |s| s.push_str("-mine"));
        fileio::test_hooks::after_replace(|t| std::fs::write(t, b"synced elsewhere").unwrap());
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Unverified { .. })));
        app.run(&ctx, Action::DismissModal);
        // 古い保存済みの版 (保存前の内容) まで戻す
        edit(&mut app, 0, |s| *s = "one".into());
        assert!(app.docs[0].dirty, "Undo で戻しても変更ありのまま");
        assert!(app.docs[0].has_unsaved());
        // Ctrl+W は確認を出し、閉じない
        app.run(&ctx, Action::CloseActive);
        assert!(matches!(app.modal(), Some(Modal::ConfirmClose(i)) if *i == id));
        assert_eq!(app.docs.len(), 1);
        assert_eq!(app.docs[0].id, id);
        app.run(&ctx, Action::DismissModal);
        // ウィンドウを閉じる要求も取り消して、確認する
        let mut clock = 0.0;
        let out = frame(&ctx, &mut app, &mut clock, vec![], true);
        assert!(cancels_close(&out));
        assert!(matches!(app.modal(), Some(Modal::ConfirmExit)));
        app.run(&ctx, Action::DismissModal);
        // 保存し直す (確認して上書き) と、元に戻る
        app.run(&ctx, Action::Save);
        confirm(&mut app);
        assert!(!app.docs[0].disk_unverified);
        assert!(!app.docs[0].has_unsaved());
    }

    /// 外部の変更を承知して上書きしたときも、承知した版の内容の要約を持ち回るので、
    /// 置き換えで退避した控えは (他に何も無ければ) 残らない
    #[test]
    fn approved_conflict_overwrite_leaves_no_backup_behind() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        edit(&mut app, 0, |s| s.push('!'));
        std::fs::write(&p, b"changed by another program").unwrap();
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        let n = std::fs::read_dir(&t.0).unwrap().count();
        confirm(&mut app);
        assert!(app.modal().is_none(), "知らせる画面は出ない");
        assert_eq!(std::fs::read(&p).unwrap(), b"one!");
        assert_eq!(std::fs::read_dir(&t.0).unwrap().count(), n, "控えは残らない");
        assert!(!app.docs[0].dirty);
    }

    /// 読み込みの間に、最初の空のタブへ入力して空に戻した (読み込み前と同じ見た目の) ときは、
    /// その空のタブを (Undo 履歴ごと) 消さずに、新しいタブを追加する。
    /// 何も触っていない最初の空のタブは、従来どおり置き換える
    #[test]
    fn edited_blank_tab_is_not_replaced_by_finished_load() {
        let t = Tmp::new();
        let b = t.file("b.txt", b"bbb");
        // 触っていない: 置き換える
        let mut app = app();
        open(&mut app, &b);
        assert_eq!(app.docs.len(), 1);
        assert_eq!(app.docs[0].path.as_deref(), Some(b.as_path()));
        // 読み込みの間に入力して、空に戻した: 置き換えない
        let mut app = self::app();
        let blank_id = app.docs[0].id;
        app.open_path(b.clone());
        edit(&mut app, 0, |s| s.push('x'));
        edit(&mut app, 0, |s| s.clear());
        assert!(!app.docs[0].dirty && app.docs[0].text.is_empty());
        wait_loads(&mut app);
        assert_eq!(app.docs.len(), 2, "編集した空のタブは残る");
        assert_eq!(app.docs[0].id, blank_id);
        assert!(app.docs[0].path.is_none());
        assert_eq!(app.docs[1].path.as_deref(), Some(b.as_path()));
        assert_eq!(app.docs[app.active].id, blank_id, "選択も動かさない");
    }

    /// 置き換えの直後に、他のプログラムが同じ長さの内容で上書きして更新日時を戻しても、
    /// 「保存して閉じる」は閉じない (保存済みとは扱わない)
    #[test]
    fn same_length_overwrite_after_replace_does_not_close_tab() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        let id = app.docs[0].id;
        edit(&mut app, 0, |s| s.push_str("-mine"));
        fileio::test_hooks::after_replace(|t| {
            let mtime = std::fs::metadata(t).unwrap().modified().unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(t).unwrap();
            std::io::Write::write_all(&mut &f, b"EXT!!!!!").unwrap();
            f.set_modified(mtime).unwrap();
        });
        app.run(&ctx, Action::SaveAndClose(id));
        assert_eq!(std::fs::read(&p).unwrap(), b"EXT!!!!!");
        assert!(matches!(app.modal(), Some(Modal::Unverified { .. })));
        assert_eq!(app.docs.len(), 1, "閉じない");
        assert!(app.docs[0].dirty && app.docs[0].disk_unverified);
    }

    /// 保存の直後に確かめられなかった文書の保存し直しでは、保存先を最後まで読むのは 1 回だけ
    #[test]
    fn unverified_retry_reads_the_file_once() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        edit(&mut app, 0, |s| s.push('!'));
        app.docs[0].fingerprint = None;
        app.docs[0].disk_unverified = true;
        let reads = || fileio::test_hooks::DIGESTED_READS.with(|c| c.get());
        let before = reads();
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        assert_eq!(reads() - before, 1);
        // 外部の変更の確認の画面でも 1 回
        app.run(&ctx, Action::DismissModal);
        app.docs[0].disk_unverified = false;
        app.docs[0].fingerprint = fileio::fingerprint_digested(&p);
        std::fs::write(&p, b"changed by another program").unwrap();
        let before = reads();
        app.run(&ctx, Action::Save);
        assert!(matches!(app.modal(), Some(Modal::Conflict { .. })));
        assert_eq!(reads() - before, 1);
    }

    /// 自動インデントを切っていても、Ctrl+Tab → Enter → 文字 は切り替え後のタブへ入る
    /// (インデントは付かない)
    #[test]
    fn ctrl_tab_enter_text_goes_to_new_tab_without_auto_indent() {
        let t = Tmp::new();
        let (ctx, mut app, mut clock, _p) = indented_app(&t, "a.txt");
        app.auto_indent = false;
        let b = t.file("b.txt", b"    b");
        open(&mut app, &b);
        app.active = 0;
        app.focus_editor = true;
        for _ in 0..3 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        let (a_id, b_id) = (app.docs[0].id, app.docs[1].id);
        set_editor_sel(&ctx, editor_id(a_id), 3, 3);
        set_editor_sel(&ctx, editor_id(b_id), 5, 5);
        frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        frame_hooked(
            &ctx,
            &mut app,
            &mut clock,
            vec![ctrl_key(Key::Tab), enter_ev(), egui::Event::Text("x".into())],
        );
        for _ in 0..6 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert_eq!(app.docs[0].text, "  a");
        assert_eq!(app.active, 1);
        assert_eq!(app.docs[1].text, "    b
x");
    }

    /// Ctrl+F / Ctrl+H の直後の文字は、検索欄へ入る (自動インデントの有無によらない)
    #[test]
    fn text_after_ctrl_f_or_ctrl_h_reaches_the_query_field() {
        for key in [Key::F, Key::H] {
            for auto in [true, false] {
                let t = Tmp::new();
                let (ctx, mut app, mut clock, _p) = indented_app(&t, "a.txt");
                app.auto_indent = auto;
                frame_hooked(
                    &ctx,
                    &mut app,
                    &mut clock,
                    vec![ctrl_key(key), egui::Event::Text("x".into())],
                );
                for _ in 0..4 {
                    frame_hooked(&ctx, &mut app, &mut clock, vec![]);
                }
                assert_eq!(app.find.query, "x", "{key:?} auto={auto}");
                assert_eq!(app.docs[0].text, "  a", "{key:?} auto={auto}");
            }
        }
    }

    /// Ctrl+F / Ctrl+H → 文字 → Enter が同じフレームに届いたら、文字は検索欄へ入り、
    /// Enter は検索欄の Enter (次を検索) として働く。編集欄は変わらない
    #[test]
    fn ctrl_f_text_enter_in_one_frame_searches() {
        for key in [Key::F, Key::H] {
            for auto in [true, false] {
                let t = Tmp::new();
                let p = t.file("a.txt", b"  a x");
                let ctx = egui::Context::default();
                let mut app = App::blank(ctx.clone());
                let mut clock = 0.0;
                app.open_path(p.clone());
                wait_loads(&mut app);
                for _ in 0..3 {
                    frame(&ctx, &mut app, &mut clock, vec![], false);
                }
                app.auto_indent = auto;
                let ed = editor_id(app.docs[0].id);
                set_editor_sel(&ctx, ed, 3, 3);
                frame_hooked(&ctx, &mut app, &mut clock, vec![]);
                frame_hooked(
                    &ctx,
                    &mut app,
                    &mut clock,
                    vec![ctrl_key(key), egui::Event::Text("x".into()), enter_ev()],
                );
                for _ in 0..6 {
                    frame_hooked(&ctx, &mut app, &mut clock, vec![]);
                }
                let tag = format!("{key:?} auto={auto}");
                assert_eq!(app.find.query, "x", "{tag}");
                assert_eq!(app.docs[0].text, "  a x", "{tag}");
                assert_eq!(editor_sel(&ctx, ed), Some((4, 5)), "最初の一致を選ぶ: {tag}");
            }
        }
    }

    /// 読み込みの結果を取り込むのと同じフレームに Ctrl+F / Ctrl+H が来たら、新しいタブは
    /// 背景で開く。次のフレームの文字は検索欄だけに入り、新しいタブは変わらない
    #[test]
    fn load_completion_with_ctrl_f_does_not_leak_text_into_new_tab() {
        for key in [Key::F, Key::H] {
            let t = Tmp::new();
            let a = t.file("a.txt", b"aaa");
            let b = t.file("b.txt", b"bbb");
            let ctx = egui::Context::default();
            let mut app = App::blank(ctx.clone());
            let mut clock = 0.0;
            app.open_path(a);
            wait_loads(&mut app);
            for _ in 0..3 {
                frame_hooked(&ctx, &mut app, &mut clock, vec![]);
            }
            let a_id = app.docs[0].id;
            app.open_path(b);
            settle_loads(&mut app);
            frame_hooked(&ctx, &mut app, &mut clock, vec![ctrl_key(key)]);
            assert_eq!(app.docs.len(), 2, "B も取り込まれた");
            frame_hooked(&ctx, &mut app, &mut clock, vec![egui::Event::Text("x".into())]);
            for _ in 0..3 {
                frame_hooked(&ctx, &mut app, &mut clock, vec![]);
            }
            let bi = app.docs.iter().position(|d| d.id != a_id).unwrap();
            assert_eq!(app.find.query, "x", "{key:?}");
            assert_eq!(app.docs[app.active].id, a_id, "{key:?}");
            assert_eq!(app.docs[bi].text, "bbb", "{key:?}");
            assert!(!app.docs[bi].dirty, "{key:?}");
        }
    }

    /// 検索欄にフォーカスがあるとき、Ctrl+N → 文字 が同じフレームに届いたら、文字は新しいタブへ
    /// だけ入り、検索語は変わらない
    #[test]
    fn ctrl_n_then_text_does_not_change_the_query() {
        let t = Tmp::new();
        let (ctx, mut app, mut clock, _p) = indented_app(&t, "a.txt");
        app.run(&ctx, Action::OpenFind(false));
        app.find.query = "abc".into();
        for _ in 0..3 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert!(ctx.memory(|m| m.has_focus(find_query_id())), "検索欄にフォーカス");
        frame_hooked(
            &ctx,
            &mut app,
            &mut clock,
            vec![ctrl_key(Key::N), egui::Event::Text("x".into())],
        );
        for _ in 0..4 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert_eq!(app.find.query, "abc");
        assert_eq!(app.docs.len(), 2);
        assert_eq!(app.docs[app.active].text, "x");
    }

    /// 検索欄にフォーカスがあるとき、Esc → 文字 が同じフレームに届いたら、検索バーは閉じ、
    /// 文字は (フォーカスが戻った) 編集欄へ入る。検索語は変わらない
    #[test]
    fn esc_then_text_goes_to_the_editor() {
        let t = Tmp::new();
        let (ctx, mut app, mut clock, _p) = indented_app(&t, "a.txt");
        app.run(&ctx, Action::OpenFind(false));
        app.find.query = "abc".into();
        for _ in 0..3 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert!(ctx.memory(|m| m.has_focus(find_query_id())));
        let esc = egui::Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        frame_hooked(&ctx, &mut app, &mut clock, vec![esc, egui::Event::Text("x".into())]);
        for _ in 0..4 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert!(!app.find.open, "検索バーは閉じる");
        assert_eq!(app.find.query, "abc");
        assert!(app.docs[0].text.contains('x'), "{:?}", app.docs[0].text);
    }

    /// すでに開いているファイルをドロップして (そのタブへ切り替わる) 同じフレームに来た文字は、
    /// 切り替わったタブへ入る。検索欄にフォーカスがあっても、検索語には入らない
    #[test]
    fn drop_of_open_file_then_text_goes_to_that_tab() {
        for in_query in [false, true] {
            let t = Tmp::new();
            let a = t.file("a.txt", b"aaa");
            let b = t.file("b.txt", b"bbb");
            let ctx = egui::Context::default();
            let mut app = App::blank(ctx.clone());
            let mut clock = 0.0;
            open(&mut app, &a);
            open(&mut app, &b);
            app.active = 0;
            app.focus_editor = true;
            if in_query {
                app.run(&ctx, Action::OpenFind(false));
                app.find.query = "abc".into();
            }
            for _ in 0..4 {
                frame_hooked(&ctx, &mut app, &mut clock, vec![]);
            }
            let (a_id, b_id) = (app.docs[0].id, app.docs[1].id);
            set_editor_sel(&ctx, editor_id(b_id), 3, 3);
            clock += 0.05;
            let raw = egui::RawInput {
                time: Some(clock),
                events: vec![egui::Event::Text("x".into())],
                dropped_files: vec![egui::DroppedFile {
                    path: Some(b.clone()),
                    ..Default::default()
                }],
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(raw, |ctx| app.update_ui(ctx, 0));
            for _ in 0..3 {
                frame_hooked(&ctx, &mut app, &mut clock, vec![]);
            }
            assert_eq!(app.docs[app.active].id, b_id, "in_query={in_query}");
            assert_eq!(app.docs[1].text, "bbbx", "in_query={in_query}");
            assert_eq!(app.docs[0].text, "aaa");
            let _ = a_id;
            if in_query {
                assert_eq!(app.find.query, "abc");
            }
        }
    }

    /// 最後の確認と置き換えの間に他のプログラムが書いた版は、消さずに見つけやすい名前で残し、
    /// 知らせて、閉じる・終了する操作を止める。何も無ければ控えは残らない
    #[test]
    fn version_written_before_replace_is_preserved_and_reported() {
        let t = Tmp::new();
        let p = t.file("f.txt", b"one");
        let mut app = app();
        open(&mut app, &p);
        let ctx = app.ctx.clone();
        let id = app.docs[0].id;
        edit(&mut app, 0, |s| s.push_str("-mine"));
        fileio::test_hooks::before_replace(|t| std::fs::write(t, b"external version").unwrap());
        app.run(&ctx, Action::SaveAndClose(id));
        assert_eq!(std::fs::read(&p).unwrap(), b"one-mine");
        assert_eq!(app.docs.len(), 1, "閉じない");
        let Some(Modal::Info(msg)) = app.modal() else {
            panic!("退避を知らせる画面のはず");
        };
        let kept: Vec<_> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|f| {
                f.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("f (RsEditor 退避 ")
            })
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(std::fs::read(&kept[0]).unwrap(), b"external version");
        assert!(msg.contains(&kept[0].display().to_string()), "{msg}");
        assert!(!app.docs[0].dirty, "保存先には書けている");
        // 何も起きなければ、控えを残さずに普通に閉じる
        app.run(&ctx, Action::DismissModal);
        edit(&mut app, 0, |s| s.push('!'));
        let n = std::fs::read_dir(&t.0).unwrap().count();
        app.run(&ctx, Action::SaveAndClose(id));
        assert_eq!(std::fs::read_dir(&t.0).unwrap().count(), n, "控えは残らない");
        assert!(app.docs.iter().all(|d| d.id != id));
    }

    /// 読み込みが終わるのと同じフレームの Ctrl+S / すべて置換は、元のタブに対して行われる
    #[test]
    fn actions_in_completion_frame_target_original_tab() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"abc");
        let b = t.file("b.txt", b"abc");
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        app.open_path(a.clone());
        wait_loads(&mut app);
        edit(&mut app, 0, |s| s.push('!'));
        app.find.query = "b".into();
        app.find.replacement = "X".into();
        app.open_path(b.clone());
        // 読み込みのスレッドが終わるまで待つ (結果の取り込みは、次のフレームの最後に行われる)
        settle_loads(&mut app);
        let raw = egui::RawInput {
            time: Some(1.0),
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            app.update_ui_with(ctx, 0, vec![Action::ReplaceAll, Action::Save]);
        });
        assert_eq!(std::fs::read(&a).unwrap(), b"aXc!", "A が保存された");
        assert_eq!(std::fs::read(&b).unwrap(), b"abc", "B は変わらない");
        assert_eq!(app.docs.len(), 2, "B も取り込まれた");
        let bi = app.docs.iter().position(|d| d.title() == "b.txt").unwrap();
        assert_eq!(app.docs[bi].text, "abc");
        assert!(!app.docs[bi].dirty);
        assert!(!app.docs[1 - bi].dirty, "A は保存済み");
    }

    /// 入力を 1 フレーム持ち越す間に読み込みが終わっても、その入力は元のタブへ届く
    /// (新しいタブは追加だけして、選択しない)
    #[test]
    fn deferred_auto_indent_stays_with_its_tab() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"  a");
        let b = t.file("b.txt", b"bbb");
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let mut clock = 0.0;
        app.open_path(a);
        wait_loads(&mut app);
        for _ in 0..3 {
            frame(&ctx, &mut app, &mut clock, vec![], false);
        }
        let a_id = app.docs[0].id;
        set_editor_sel(&ctx, editor_id(a_id), 3, 3);
        frame(&ctx, &mut app, &mut clock, vec![], false);
        app.open_path(b);
        settle_loads(&mut app);
        // 文字入力の後ろの Enter は、次のフレームへ持ち越される。同じフレームで B の読み込みが終わる
        frame_hooked(
            &ctx,
            &mut app,
            &mut clock,
            vec![egui::Event::Text("b".into()), enter_ev()],
        );
        assert_eq!(app.docs.len(), 2, "B も取り込まれた");
        assert!(matches!(&app.deferred_events, Some((Some(id), _)) if *id == a_id));
        assert_eq!(app.docs[app.active].id, a_id, "持ち越し中は選択を動かさない");
        frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        let ai = app.docs.iter().position(|d| d.id == a_id).unwrap();
        assert_eq!(app.docs[ai].text, "  ab\n  ", "持ち越した Enter は A へ届く");
        let bi = 1 - ai;
        assert_eq!(app.docs[bi].text, "bbb", "B には入らない");
        // タブが切り替わっていれば、持ち越した入力は捨てる
        app.deferred_events = Some((Some(a_id), vec![enter_ev()]));
        app.active = bi;
        frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        assert!(app.deferred_events.is_none());
        assert_eq!(app.docs[bi].text, "bbb");
        assert_eq!(app.docs[ai].text, "  ab\n  ");
    }

    fn ctrl_key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: if key == Key::Tab { Modifiers::CTRL } else { Modifiers::COMMAND },
        }
    }

    /// 編集欄にフォーカスがあり、キャレットが末尾にある状態の (インデント 2 つの 1 行) アプリ
    fn indented_app(t: &Tmp, name: &str) -> (egui::Context, App, f64, PathBuf) {
        let p = t.file(name, b"  a");
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let mut clock = 0.0;
        app.open_path(p.clone());
        wait_loads(&mut app);
        for _ in 0..3 {
            frame(&ctx, &mut app, &mut clock, vec![], false);
        }
        set_editor_sel(&ctx, editor_id(app.docs[0].id), 3, 3);
        frame(&ctx, &mut app, &mut clock, vec![], false);
        (ctx, app, clock, p)
    }

    /// 文字入力 → Enter → Ctrl+S が同じフレームに届いても、保存は改行より後 (入力の順序どおり)
    #[test]
    fn text_enter_save_in_one_frame_saves_the_newline() {
        let t = Tmp::new();
        let (ctx, mut app, mut clock, p) = indented_app(&t, "a.txt");
        frame_hooked(
            &ctx,
            &mut app,
            &mut clock,
            vec![egui::Event::Text("b".into()), enter_ev(), ctrl_key(Key::S)],
        );
        for _ in 0..4 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert_eq!(app.docs[0].text, "  ab\n  ");
        assert_eq!(std::fs::read(&p).unwrap(), b"  ab\r\n  ", "改行とインデントも保存される");
        assert!(!app.docs[0].dirty);
        // Enter が先頭でも、後ろのショートカットは Enter の処理より後
        let q = t.file("c.txt", b"  c");
        open(&mut app, &q);
        for _ in 0..3 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        let ci = app.active;
        set_editor_sel(&ctx, editor_id(app.docs[ci].id), 3, 3);
        frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        frame_hooked(&ctx, &mut app, &mut clock, vec![enter_ev(), ctrl_key(Key::S)]);
        for _ in 0..4 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert_eq!(std::fs::read(&q).unwrap(), b"  c\r\n  ");
    }

    /// 文字入力 → Enter → Ctrl+Tab: 改行は元のタブへ入り、タブの切り替えはその後
    #[test]
    fn text_enter_ctrl_tab_in_one_frame_keeps_text_in_original_tab() {
        let t = Tmp::new();
        let (ctx, mut app, mut clock, _p) = indented_app(&t, "a.txt");
        let b = t.file("b.txt", b"bbb");
        open(&mut app, &b);
        app.active = 0;
        app.focus_editor = true;
        for _ in 0..3 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        let a_id = app.docs[0].id;
        set_editor_sel(&ctx, editor_id(a_id), 3, 3);
        frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        frame_hooked(
            &ctx,
            &mut app,
            &mut clock,
            vec![
                egui::Event::Text("b".into()),
                enter_ev(),
                egui::Event::Text("Z".into()),
                ctrl_key(Key::Tab),
            ],
        );
        for _ in 0..5 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert_eq!(app.docs[0].text, "  ab\n  Z", "元のタブに改行と、その後の文字が入る");
        assert_eq!(app.active, 1, "切り替えは入力の後");
        assert_eq!(app.docs[1].text, "bbb");
    }

    /// `auto_indent_split` の切り方: Enter が先頭ならその次から、後ろにあれば Enter から
    #[test]
    fn auto_indent_split_points() {
        let s = ctrl_key(Key::S);
        let txt = || egui::Event::Text("x".into());
        assert_eq!(auto_indent_split(&[enter_ev()], false), None);
        assert_eq!(auto_indent_split(&[enter_ev(), s.clone()], false), Some(1));
        assert_eq!(auto_indent_split(&[txt(), enter_ev(), s.clone()], false), Some(1));
        assert_eq!(auto_indent_split(&[txt(), s], false), None);
    }

    /// Ctrl+Tab → Enter → 文字 が同じフレームに届いても、Ctrl+Tab より後の入力は、切り替わった後の
    /// タブへ (そのタブのインデントで) 入る。切り替え前のタブは変わらない
    #[test]
    fn ctrl_tab_enter_text_in_one_frame_goes_to_the_new_tab() {
        let t = Tmp::new();
        let (ctx, mut app, mut clock, _p) = indented_app(&t, "a.txt");
        let b = t.file("b.txt", b"    b");
        open(&mut app, &b);
        app.active = 0;
        app.focus_editor = true;
        for _ in 0..3 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        let (a_id, b_id) = (app.docs[0].id, app.docs[1].id);
        set_editor_sel(&ctx, editor_id(a_id), 3, 3);
        set_editor_sel(&ctx, editor_id(b_id), 5, 5);
        frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        frame_hooked(
            &ctx,
            &mut app,
            &mut clock,
            vec![ctrl_key(Key::Tab), enter_ev(), egui::Event::Text("x".into())],
        );
        for _ in 0..6 {
            frame_hooked(&ctx, &mut app, &mut clock, vec![]);
        }
        assert_eq!(app.docs[0].text, "  a", "切り替え前のタブは変わらない");
        assert_eq!(app.active, 1);
        assert_eq!(app.docs[1].text, "    b
    x", "新しいタブのインデントで改行され、文字が入る");
    }

    /// 行き先を変えるショートカット (Ctrl+Tab など) は、持ち越しの切れ目: それ以降をすべて持ち越し、
    /// 持ち越した入力は宛先を持たない。ショートカットが先頭なら、それだけを処理する
    #[test]
    fn target_changing_shortcut_is_a_split_point() {
        let tab = ctrl_key(Key::Tab);
        let w = ctrl_key(Key::W);
        let s = ctrl_key(Key::S);
        let txt = || egui::Event::Text("x".into());
        assert_eq!(input_split(&[tab.clone(), enter_ev(), txt()], false, false), Some((1, false)));
        assert_eq!(input_split(&[txt(), tab.clone(), enter_ev()], false, false), Some((1, false)));
        assert_eq!(input_split(&[txt(), w, txt()], false, false), Some((1, false)));
        assert_eq!(input_split(&[tab.clone()], false, false), None);
        // Enter が先にあれば、従来どおり Enter の側で切る (宛先あり)
        assert_eq!(input_split(&[enter_ev(), tab.clone(), txt()], false, false), Some((1, true)));
        assert_eq!(input_split(&[txt(), enter_ev(), tab], false, false), Some((1, true)));
        // Ctrl+S は行き先を変えない
        assert_eq!(input_split(&[txt(), s], false, false), None);
    }

    /// 読み込みの間に同じタブへ入力を続けていたら、読み込みが終わっても新しいタブを
    /// 選択しない (以降の入力が新しいタブへ入らない)。背景で開く
    #[test]
    fn load_completion_does_not_steal_typing_focus() {
        let t = Tmp::new();
        let a = t.file("a.txt", b"abc");
        let b = t.file("b.txt", b"bbb");
        let mut app = app();
        open(&mut app, &a);
        let a_id = app.docs[0].id;
        app.open_path(b);
        // 読み込みの間に、同じタブへ打ち続ける
        edit(&mut app, 0, |s| s.push('x'));
        wait_loads(&mut app);
        assert_eq!(app.docs.len(), 2, "B は開く");
        assert_eq!(app.docs[app.active].id, a_id, "選択は A のまま");
        assert!(app.status.contains("「b.txt」を開きました"), "{}", app.status);
        // 打っていなければ、従来どおり新しいタブを選択する
        let c = t.file("c.txt", b"ccc");
        open(&mut app, &c);
        assert_eq!(app.docs[app.active].title(), "c.txt");
    }

    fn enter_ev() -> egui::Event {
        egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// `App::raw_input_hook` (持ち越した入力の戻しを含む) を通して 1 フレーム進める
    fn frame_hooked(
        ctx: &egui::Context,
        app: &mut App,
        t: &mut f64,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        *t += 0.05;
        let mut raw = egui::RawInput {
            time: Some(*t),
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        ctx.run(raw, |ctx| app.update_ui(ctx, 0))
    }

    /// 編集や貼り付けで文書が大きくなったら、Undo 履歴の件数の段階を下げる。
    /// 貼り付け直前までの最近の履歴は残り、貼り付けは Ctrl+Z で戻せる
    #[test]
    fn growing_document_shrinks_undo_limit_and_keeps_recent_history() {
        // テストでは予算を小さくして、小さな文書で件数が絞られる状況を作る
        TEST_UNDO_BUDGET.with(|b| b.set(Some(4500)));
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        let id = editor_id(app.docs[0].id);
        let mut t = 0.0;
        for _ in 0..3 {
            frame(&ctx, &mut app, &mut t, vec![], false);
        }
        let settle = |ctx: &egui::Context, app: &mut App, t: &mut f64| {
            for _ in 0..25 {
                frame(ctx, app, t, vec![], false);
            }
        };
        frame(&ctx, &mut app, &mut t, vec![egui::Event::Text("ab".into())], false);
        settle(&ctx, &mut app, &mut t);
        frame(&ctx, &mut app, &mut t, vec![egui::Event::Text(" more".into())], false);
        settle(&ctx, &mut app, &mut t);
        assert_eq!(app.docs[0].text, "ab more");
        assert_eq!(recorded_undo_limit(&ctx, id), 100, "小さな文書は既定の件数");
        // 大きな文章を貼り付ける
        let big = "x".repeat(1000);
        frame(&ctx, &mut app, &mut t, vec![egui::Event::Paste(big.clone())], false);
        assert_eq!(app.docs[0].text, format!("ab more{big}"));
        let want = undo_limit(app.docs[0].text.len());
        assert!(want < 100);
        assert_eq!(recorded_undo_limit(&ctx, id), want, "段階が下がった");
        // 履歴の件数は新しい上限以内。貼り付けは 1 回の Ctrl+Z で戻り、その前の状態も残っている
        let z = || egui::Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        };
        frame(&ctx, &mut app, &mut t, vec![z()], false);
        assert_eq!(app.docs[0].text, "ab more", "貼り付けが取り消される");
        frame(&ctx, &mut app, &mut t, vec![z()], false);
        assert_eq!(app.docs[0].text, "ab", "その前の履歴も残っている");
        // 履歴の長さは新しい上限を超えない (これ以上遡ると上限で打ち切られる)
        let mut steps = 2;
        loop {
            let before = app.docs[0].text.clone();
            frame(&ctx, &mut app, &mut t, vec![z()], false);
            if app.docs[0].text == before {
                break;
            }
            steps += 1;
            assert!(steps <= want, "{steps} > {want}");
        }
        TEST_UNDO_BUDGET.with(|b| b.set(None));
    }

    /// 履歴を作り直す関数そのもの: 古い状態を新しい上限の件数だけ残す
    #[test]
    fn shrink_undo_history_keeps_newest_states() {
        let ctx = egui::Context::default();
        let id = editor_id(31);
        let mut state = TextEditState::default();
        let mut u = bounded_undoer(100);
        for i in 0..10 {
            u.add_undo(&(CCursorRange::one(CCursor::new(0)), format!("state {i}")));
        }
        state.set_undoer(u);
        state.store(&ctx, id);
        shrink_undo_history(&ctx, id, "current text", 4);
        assert_eq!(recorded_undo_limit(&ctx, id), 4);
        let mut u = TextEditState::load(&ctx, id).unwrap().undoer();
        let mut cur = (CCursorRange::one(CCursor::new(0)), "current text".to_string());
        let mut seen = Vec::new();
        while let Some(prev) = u.undo(&cur).cloned() {
            seen.push(prev.1.clone());
            cur = prev;
            assert!(seen.len() < 10);
        }
        assert_eq!(seen, ["state 9", "state 8", "state 7"], "最近の 3 件 + 現在の状態");
    }

    /// 「置換」は、選択範囲の文字列そのものが検索語に一致するなら、その選択範囲を置換する。
    /// 一致の件数 (一覧の上限) には左右されない
    #[test]
    fn replace_one_semantics_do_not_depend_on_match_count() {
        for n in [6usize, search::MAX_STORED_MATCHES * 2 + 10] {
            let ctx = egui::Context::default();
            let mut app = App::blank(ctx.clone());
            // "aa" は重なって現れる ("aaa" の (1,3) は一覧の (0,2) とは別だが、選択範囲そのものが一致)
            app.docs[0].text = "a".repeat(n);
            app.docs[0].edited();
            app.find.query = "AA".into();
            app.find.replacement = "Z".into();
            app.find.case_sensitive = false; // 大文字小文字は区別しない
            let id = editor_id(app.docs[0].id);
            set_editor_sel(&ctx, id, 1, 3);
            app.run(&ctx, Action::ReplaceOne);
            assert!(app.docs[0].text.starts_with("aZ"), "n = {n}: {}", &app.docs[0].text[..4]);
            assert_eq!(app.docs[0].text.matches('Z').count(), 1);
            assert_eq!(app.docs[0].text.len(), n - 1);
            // 選択が検索語と一致しないときは、置換せずに次の一致へ移動する
            let before = app.docs[0].text.clone();
            set_editor_sel(&ctx, id, 0, 1);
            app.run(&ctx, Action::ReplaceOne);
            assert_eq!(app.docs[0].text, before, "n = {n}: 置換されない");
            assert_eq!(editor_sel(&ctx, id), Some((2, 4)), "n = {n}: 次の一致へ移動");
            // 大文字小文字を区別するときは、一致しない
            app.find.case_sensitive = true;
            set_editor_sel(&ctx, id, 2, 4);
            app.run(&ctx, Action::ReplaceOne);
            assert_eq!(app.docs[0].text, before, "n = {n}: 区別すれば一致しない");
        }
    }
}
