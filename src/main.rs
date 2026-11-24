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
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

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

/// 編集欄の表示 (galley) のキャッシュ。毎フレームの再レイアウト (全文のコピーとハッシュ) を避ける
struct LayoutCache {
    key: LayoutKey,
    galley: Arc<egui::Galley>,
}

/// レイアウト結果が変わる条件のすべて。これが同じなら、前のフレームの galley を使い回せる
#[derive(Clone, PartialEq)]
struct LayoutKey {
    rev: u64,
    /// 折り返さないときは 0
    wrap_width: u32,
    font_size: u32,
    text_color: egui::Color32,
    dark: bool,
    pixels_per_point: u32,
    /// 検索結果の強調 (検索結果キャッシュの識別子。強調なしは 0)
    find_stamp: u64,
    /// 「現在の一致」として濃く表示する一致の番号
    exact: Option<usize>,
}

/// galley の各表示行が属する論理行の番号 (1 始まり)。行番号を、見えている行からだけ描くために使う
struct RowLines {
    galley: Arc<egui::Galley>,
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

/// 検索結果のキャッシュ。検索語・検索条件・文書の版が変わったときだけ作り直す
struct FindCache {
    rev: u64,
    /// 作り直すたびに変わる識別子 (表示のキャッシュの判定に使う。0 は使わない)
    stamp: u64,
    query: String,
    case_sensitive: bool,
    /// 一致箇所 (文字位置の範囲)
    matches: Vec<(usize, usize)>,
    /// 一致箇所 (バイト位置の範囲)。表示の強調に使う
    byte_ranges: Vec<(usize, usize)>,
}

/// 検索結果の識別子の採番
static FIND_STAMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

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
    cursor_idx: usize,
    line_col: (usize, usize),
    /// 文字列を変更するたびに増やす版番号 (各種キャッシュの判定に使う)
    rev: u64,
    find_cache: Option<FindCache>,
    stats: Option<DocStats>,
    layout_cache: Option<LayoutCache>,
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
            cursor_idx: 0,
            line_col: (1, 1),
            rev: 0,
            find_cache: None,
            stats: None,
            layout_cache: None,
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
        d.fingerprint = l.fingerprint;
        d.mark_saved();
        d
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
        self.dirty = if self.enc != s.enc || self.eol != s.eol {
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
            let byte_ranges = search::find_bytes(&self.text, query, case_sensitive);
            let matches = search::bytes_to_chars(&self.text, &byte_ranges);
            self.find_cache = Some(FindCache {
                rev: self.rev,
                stamp: FIND_STAMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                query: query.to_owned(),
                case_sensitive,
                matches,
                byte_ranges,
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
    let (Some(old_copy), Some(new_copy)) = (try_clone(text), try_clone(&new_text)) else {
        return false;
    };
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let old_range = state
        .cursor
        .char_range()
        .unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));
    let new_range = CCursorRange::two(CCursor::new(new_sel.0), CCursor::new(new_sel.1));
    let mut undoer = state.undoer();
    undoer.add_undo(&(old_range, old_copy));
    *text = new_text;
    let new_state = (new_range, new_copy);
    // 新しい編集なので、Undo 後に残っている Redo を捨てる。egui 0.29 の Undoer には
    // Redo を消す API が無いが、最新の履歴と異なる状態で redo() を呼ぶと Redo が消える
    let _ = undoer.redo(&new_state);
    undoer.add_undo(&new_state);
    state.set_undoer(undoer);
    state.cursor.set_char_range(Some(new_range));
    state.store(ctx, id);
    true
}

/// 確保に失敗しても落ちない文字列の複製
fn try_clone(s: &str) -> Option<String> {
    let mut v = String::new();
    v.try_reserve_exact(s.len()).ok()?;
    v.push_str(s);
    Some(v)
}

/// 編集欄の Undo 履歴とキャレットを初期状態に戻す (文書を読み直したとき、読み直す前の履歴で
/// 別の内容に戻ってしまわないように)
fn reset_editor_state(ctx: &egui::Context, id: egui::Id) {
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    state.set_undoer(Default::default());
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(0))));
    state.store(ctx, id);
}

/// 現在行の行頭からキャレット (文字位置 `pos`) までの空白・タブ (= 引き継ぐインデント)
fn line_indent(text: &str, pos: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let pos = pos.min(chars.len());
    let line_start = chars[..pos]
        .iter()
        .rposition(|&c| c == '\n')
        .map_or(0, |p| p + 1);
    chars[line_start..pos]
        .iter()
        .take_while(|&&c| c == ' ' || c == '\t')
        .collect()
}

/// 編集欄にフォーカスがあるとき、Enter (Ctrl/Alt なし。Shift 付きも含む) の押下を、
/// その場で「改行 + 現在行のインデント」の文字入力 (`Event::Text`) に置き換える。
///
/// イベント列の中の位置はそのままなので、同じフレームの他の入力との順序が保たれる。
/// 文字入力として egui の TextEdit が処理するので、Undo/Redo の履歴も egui の通常の入力と
/// 同じに扱われる (改行とインデントは 1 つの入力として一緒に戻る)。
/// IME の確定 Enter は `ImeGuard` が先に取り除くので、ここへは届かない。
fn apply_auto_indent(ctx: &egui::Context, id: egui::Id, text: &str) {
    if !ctx.memory(|m| m.has_focus(id)) {
        return;
    }
    let is_enter = |e: &egui::Event| {
        matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. }
            if !modifiers.ctrl && !modifiers.alt && !modifiers.command && !modifiers.mac_cmd)
    };
    if !ctx.input(|i| i.events.iter().any(is_enter)) {
        return;
    }
    let Some(range) = TextEditState::load(ctx, id).and_then(|s| s.cursor.char_range()) else {
        return; // 状態がまだ無いときは egui の通常の改行に任せる
    };
    let indent = line_indent(text, range.sorted()[0].index);
    if indent.is_empty() {
        return; // インデントなし: egui の通常の改行と同じ
    }
    let newline = format!("\n{indent}");
    ctx.input_mut(|i| {
        for e in i.events.iter_mut() {
            if is_enter(e) {
                *e = egui::Event::Text(newline.clone());
            }
        }
    });
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
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Confirmed {
    /// 読み込み時にデコードできなかったバイトが失われること
    lossy_load: bool,
    /// ファイルが外部で変更されていること
    conflict: bool,
    /// 文字コードで表現できない文字が変わること
    lossy: bool,
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
    /// 読み込み後に、ファイルが他のプログラムで変更・削除された
    Conflict {
        id: u64,
        path: PathBuf,
        then: Then,
        ok: Confirmed,
    },
    /// 未保存の編集を捨てて、指定した文字コードで読み直す
    ConfirmReload { id: u64, enc: Enc },
    /// 大きなファイルを開く確認
    ConfirmLargeOpen { path: PathBuf, size: u64 },
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
    /// 直近の Commit の後に、まだ取り除ける Enter の数 (Commit のたびに 1 に戻る)。
    /// 確定の Enter は 1 回だけなので、猶予の間でも 2 回目以降の Enter は本物の改行として通す
    commit_enter_budget: u32,
    /// 最後の (空でない) Preedit の後に来た「空の Preedit / Disabled」の数。
    /// 最後の Preedit 以降に Commit が来ていれば None
    ends_since_preedit: Option<u32>,
}

impl ImeGuard {
    /// 確定の Enter が Commit の前後どちらに来ても取り除くための猶予 (秒)
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
    fn filter(&mut self, time: f64, events: &mut Vec<egui::Event>) {
        for e in events.iter_mut() {
            match e {
                // 新しい変換の開始: 前の変換の数え上げを持ち越さない
                egui::Event::Ime(egui::ImeEvent::Enabled) => {
                    self.ends_since_preedit = None;
                }
                egui::Event::Ime(egui::ImeEvent::Preedit(t)) if !t.is_empty() => {
                    self.ends_since_preedit = Some(0);
                }
                egui::Event::Ime(egui::ImeEvent::Preedit(_) | egui::ImeEvent::Disabled) => {
                    if let Some(n) = &mut self.ends_since_preedit {
                        *n += 1;
                    }
                }
                egui::Event::Ime(egui::ImeEvent::Commit(_)) => {
                    let fake = self.ends_since_preedit.is_some_and(|n| n >= 2);
                    self.ends_since_preedit = None;
                    if fake {
                        *e = egui::Event::Ime(egui::ImeEvent::Preedit(String::new()));
                    }
                }
                _ => {}
            }
        }
        events.retain(|e| match e {
            egui::Event::Ime(egui::ImeEvent::Preedit(t)) => {
                self.composing = !t.is_empty();
                self.preedit = t.clone();
                true
            }
            egui::Event::Ime(egui::ImeEvent::Commit(_)) => {
                self.composing = false;
                self.preedit.clear();
                self.last_commit = Some(time);
                self.commit_enter_budget = 1;
                true
            }
            egui::Event::Ime(egui::ImeEvent::Disabled) => {
                self.composing = false;
                self.preedit.clear();
                true
            }
            egui::Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            } => {
                if self.composing {
                    return false;
                }
                // 確定の直後 (猶予の間) の Enter は 1 回だけ取り除く。それ以降は本物の Enter
                let after_commit = self.last_commit.is_some_and(|c| time - c <= Self::GRACE);
                if after_commit && self.commit_enter_budget > 0 {
                    self.commit_enter_budget -= 1;
                    return false;
                }
                true
            }
            _ => true,
        });
    }
}

/// バックグラウンドの読み込みの種類
enum LoadKind {
    /// 新しいタブで開く
    Open,
    /// 開いている文書を、文字コードを指定して読み直す
    Reload { doc_id: u64 },
}

/// バックグラウンドのスレッドで実行中の読み込み
struct PendingLoad {
    path: PathBuf,
    /// 同じファイルかどうかの判定用 (`fileio::path_key`)
    key: String,
    kind: LoadKind,
    rx: mpsc::Receiver<Result<fileio::Loaded, String>>,
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
    modal: Option<Modal>,
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
    /// 一致箇所を画面内にスクロールする要求 (文書id, 文字位置)。次の描画で処理する
    scroll_req: Option<(u64, usize)>,
    /// バックグラウンドスレッドから描画を促すために使う
    ctx: egui::Context,
    /// 実行中の読み込み
    loads: Vec<PendingLoad>,
    /// タブの列をスクロールして見せ終わった、選択中の文書 (選択が変わったときだけスクロールする)
    tab_shown: Option<u64>,
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
            modal: None,
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
            scroll_req: None,
            ctx,
            loads: Vec::new(),
            tab_shown: None,
        };
        app.new_doc();
        app
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    /// 同じファイル (`fileio::path_key` が等しい) を開いているタブ
    fn find_doc_by_key(&self, key: &str) -> Option<usize> {
        self.docs.iter().position(|d| {
            d.path
                .as_deref()
                .is_some_and(|p| fileio::path_key(p) == key)
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
        self.open_path_checked(path, false);
    }

    /// ファイルを開く。既に開いている (または読み込み中の) ファイルなら、そのタブに切り替えるだけ。
    /// 大きなファイルは `confirmed_large` でなければ、先に確認する
    fn open_path_checked(&mut self, path: PathBuf, confirmed_large: bool) {
        let key = fileio::path_key(&path);
        if let Some(i) = self.find_doc_by_key(&key) {
            self.active = i;
            self.focus_editor = true;
            return;
        }
        if self.loads.iter().any(|l| l.key == key) {
            self.status = format!("{} は読み込み中です", path.display());
            return;
        }
        if !confirmed_large {
            if let Ok(m) = std::fs::metadata(&path) {
                if m.len() > fileio::LARGE_FILE_BYTES {
                    self.modal = Some(Modal::ConfirmLargeOpen { path, size: m.len() });
                    return;
                }
            }
        }
        self.start_load(path, key, LoadKind::Open, None);
    }

    /// 読み込みをバックグラウンドのスレッドで始める (読み込み中も他のタブを操作できる)
    fn start_load(&mut self, path: PathBuf, key: String, kind: LoadKind, forced: Option<Enc>) {
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
            self.modal = Some(Modal::Info(format!(
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
        self.loads.push(PendingLoad {
            path,
            key,
            kind,
            rx,
        });
    }

    /// 終わった読み込みを取り込む (毎フレーム呼ぶ)
    fn poll_loads(&mut self, ctx: &egui::Context) {
        let mut finished = Vec::new();
        let mut i = 0;
        while i < self.loads.len() {
            match self.loads[i].rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => i += 1,
                Ok(r) => finished.push((self.loads.remove(i), r)),
                Err(mpsc::TryRecvError::Disconnected) => finished.push((
                    self.loads.remove(i),
                    Err("読み込みが中断されました".to_string()),
                )),
            }
        }
        for (l, r) in finished {
            self.finish_load(ctx, l, r);
        }
    }

    fn finish_load(
        &mut self,
        ctx: &egui::Context,
        l: PendingLoad,
        result: Result<fileio::Loaded, String>,
    ) {
        let PendingLoad { path, key, kind, .. } = l;
        match (kind, result) {
            (LoadKind::Open, Err(e)) => {
                self.modal = Some(Modal::Info(format!(
                    "ファイルを開けませんでした:\n{}\n\n{}",
                    path.display(),
                    e
                )));
            }
            (LoadKind::Reload { .. }, Err(e)) => {
                self.modal = Some(Modal::Info(format!(
                    "読み直せませんでした (文書はそのままです):\n{}\n\n{}",
                    path.display(),
                    e
                )));
            }
            (LoadKind::Open, Ok(loaded)) => {
                // 読み込み中に、別の経路 (名前を付けて保存など) で同じファイルが開かれていたら切り替えるだけ
                if let Some(i) = self.find_doc_by_key(&key) {
                    self.active = i;
                    self.focus_editor = true;
                    return;
                }
                let notes = load_notes(loaded.lossy, loaded.mixed_eol, loaded.eol);
                let id = self.alloc_id();
                let doc = Doc::from_loaded(id, path, loaded);
                self.status = format!(
                    "{} を開きました ({} / {}){}",
                    doc.title(),
                    doc.enc.label(),
                    doc.eol.label(),
                    notes
                );
                // 未編集の空タブ1つだけなら置き換える
                let replace = self.docs.len() == 1
                    && self.docs[0].path.is_none()
                    && !self.docs[0].dirty
                    && self.docs[0].text.is_empty();
                if replace {
                    self.docs[0] = doc;
                    self.active = 0;
                } else {
                    self.docs.push(doc);
                    self.active = self.docs.len() - 1;
                }
                self.focus_editor = true;
            }
            (LoadKind::Reload { doc_id }, Ok(loaded)) => {
                let Some(i) = self.index_of(doc_id) else {
                    return; // 読み込み中にタブが閉じられた
                };
                let notes = load_notes(loaded.lossy, loaded.mixed_eol, loaded.eol);
                let d = &mut self.docs[i];
                d.text = loaded.text;
                d.enc = loaded.enc;
                d.eol = loaded.eol;
                d.lossy_load = loaded.lossy;
                d.fingerprint = loaded.fingerprint;
                d.mark_saved();
                d.rev += 1;
                d.cursor_idx = 0;
                d.line_col = (1, 1);
                // 読み直す前の Undo 履歴で、別の内容に戻ってしまわないようにする
                reset_editor_state(ctx, editor_id(d.id));
                self.status = format!("{} として再読み込みしました{}", d.enc.label(), notes);
                self.focus_editor = true;
            }
        }
    }

    /// 保存。成功 (または保存する変更がない) なら true。
    /// 確認の画面を出して中断したとき、保存に失敗したとき、キャンセルしたときは false
    fn try_save(&mut self, ctx: &egui::Context, idx: usize, save_as: bool, then: Then) -> bool {
        let d = &self.docs[idx];
        // 変更のない文書の上書き保存では何も書かない (デコードできなかったバイトや、
        // 混在していた改行コードを、意図せず書き換えてしまわないように)
        if !save_as && d.path.is_some() && !d.dirty {
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
        self.modal = None;
        let mut i = 0;
        while i < self.docs.len() {
            if self.docs[i].dirty {
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
    /// 確認の画面を設定して false を返す。保存できたら true
    fn write_doc(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        path: PathBuf,
        then: Then,
        ok: Confirmed,
    ) -> bool {
        let key = fileio::path_key(&path);
        // 別のタブで開いているファイルへは保存しない (同じファイルの二重管理になるため)
        let open_elsewhere = self.docs.iter().enumerate().any(|(i, d)| {
            i != idx
                && d.path
                    .as_deref()
                    .is_some_and(|p| fileio::path_key(p) == key)
        }) || self
            .loads
            .iter()
            .any(|l| matches!(l.kind, LoadKind::Open) && l.key == key);
        if open_elsewhere {
            self.modal = Some(Modal::Info(format!(
                "そのファイルは別のタブで開いています:\n{}",
                path.display()
            )));
            return false;
        }

        let d = &self.docs[idx];
        let id = d.id;
        let same_file = d
            .path
            .as_deref()
            .is_some_and(|p| fileio::path_key(p) == key);
        // 元のファイルへの上書きでは、デコードできなかったバイトが失われる
        if same_file && d.lossy_load && !ok.lossy_load {
            self.modal = Some(Modal::LossyLoad {
                id,
                path,
                then,
                ok,
            });
            return false;
        }
        // 読み込み後に、他のプログラムがファイルを変更・削除していないか
        if same_file && !ok.conflict {
            if let Some(fp) = &d.fingerprint {
                if fileio::changed_since(fp, &path) {
                    self.modal = Some(Modal::Conflict {
                        id,
                        path,
                        then,
                        ok,
                    });
                    return false;
                }
            }
        }
        let out = encoding::apply_eol(&d.text, d.eol);
        let (bytes, lossy) = encoding::encode(&out, d.enc);
        drop(out);
        if lossy && !ok.lossy {
            self.modal = Some(Modal::Lossy {
                id,
                path,
                then,
                ok,
                examples: encoding::lossy_examples(&d.text, d.enc, 5),
            });
            return false;
        }
        if let Err(e) = fileio::atomic_write(&path, &bytes) {
            // 元のファイルはそのまま。変更ありのままにする
            self.modal = Some(Modal::Info(format!(
                "保存に失敗しました:\n{}\n\n{}",
                path.display(),
                e
            )));
            return false;
        }

        let d = &mut self.docs[idx];
        d.path = Some(path.clone());
        d.fingerprint = fileio::fingerprint(&path);
        d.lossy_load = false;
        let mut note = String::new();
        let mut in_sync = true;
        if lossy {
            // 画面の文字を、実際に書き込んだ内容 (開き直したときに見える文字) にそろえる。
            // 履歴に積むので、Ctrl+Z で元の文字に戻せる
            let (back, _) = encoding::decode(&bytes, d.enc);
            let back = encoding::normalize_eol(back);
            if back != d.text {
                let cur = d.cursor_idx.min(back.chars().count());
                if replace_text_with_undo(ctx, editor_id(d.id), &mut d.text, back, (cur, cur)) {
                    d.rev += 1;
                    d.cursor_idx = cur;
                    d.line_col = line_col(&d.text, cur);
                    note = " ※一部の文字は保存形式に合わせて変わりました (Ctrl+Z で戻せます)".into();
                } else {
                    in_sync = false;
                    note = " ※メモリが足りず、画面の文字を保存内容にそろえられませんでした".into();
                }
            }
        }
        if in_sync {
            d.mark_saved();
        } else {
            d.dirty = true;
        }
        self.status = format!(
            "{} を保存しました ({} / {}){}",
            d.title(),
            d.enc.label(),
            d.eol.label(),
            note
        );
        true
    }

    fn request_close(&mut self, idx: usize) {
        if self.docs[idx].dirty {
            self.active = idx;
            self.modal = Some(Modal::ConfirmClose(self.docs[idx].id));
        } else {
            self.close_now(idx);
        }
    }

    fn close_now(&mut self, idx: usize) {
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
        if d.dirty {
            self.modal = Some(Modal::ConfirmReload { id: d.id, enc });
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
            .any(|l| matches!(l.kind, LoadKind::Reload { doc_id } if doc_id == id));
        if pending {
            self.status = "読み込み中です".into();
            return;
        }
        let key = fileio::path_key(&path);
        self.start_load(path, key, LoadKind::Reload { doc_id: id }, Some(enc));
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

    /// 次 (forward) / 前の一致を選択し、画面内にスクロールする
    fn find_jump(&mut self, ctx: &egui::Context, forward: bool) {
        if self.find.query.is_empty() {
            self.open_find(ctx, false);
            self.status = "検索語を入力してください".into();
            return;
        }
        let cs = self.find.case_sensitive;
        let d = &mut self.docs[self.active];
        d.ensure_matches(&self.find.query, cs);
        let id = editor_id(d.id);
        let (cur_s, cur_e) = editor_sel(ctx, id).unwrap_or((d.cursor_idx, d.cursor_idx));
        let matches = d.matches();
        let picked = if forward {
            search::next_index(matches, cur_e)
        } else {
            search::prev_index(matches, cur_s)
        };
        let Some(i) = picked else {
            self.status = "見つかりません".into();
            return;
        };
        let (s, e) = matches[i];
        let total = matches.len();
        let doc_id = d.id;
        d.cursor_idx = e;
        d.line_col = line_col(&d.text, e);
        set_editor_sel(ctx, id, s, e);
        self.scroll_req = Some((doc_id, s));
        self.status = format!("{} 件中 {} 件目を選択しました", total, i + 1);
        ctx.request_repaint();
    }

    /// 選択中の一致を置換して次へ進む。選択が一致箇所でなければ、まず次の一致へ移動する
    fn replace_one(&mut self, ctx: &egui::Context) {
        if self.find.query.is_empty() {
            self.status = "検索語を入力してください".into();
            return;
        }
        let cs = self.find.case_sensitive;
        let query = self.find.query.clone();
        let repl = self.find.replacement.clone();
        let d = &mut self.docs[self.active];
        d.ensure_matches(&query, cs);
        let id = editor_id(d.id);
        let (sel_s, sel_e) = editor_sel(ctx, id).unwrap_or((d.cursor_idx, d.cursor_idx));
        if !d.matches().contains(&(sel_s, sel_e)) {
            self.find_jump(ctx, true);
            return;
        }
        let bs = search::char_to_byte(&d.text, sel_s);
        let be = search::char_to_byte(&d.text, sel_e);
        let new_text = match search::replace_ranges(
            &d.text,
            &[(bs, be)],
            &repl,
            search::MAX_REPLACE_RESULT,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.modal = Some(Modal::Info(replace_error_message(&e)));
                return;
            }
        };
        let cur = sel_s + repl.chars().count();
        // 置換後の本文で次の一致を探し、それを選択した状態にする
        let new_matches = search::find_all(&new_text, &query, cs);
        let next = search::next_index(&new_matches, cur).map(|i| new_matches[i]);
        let (ts, te) = next.unwrap_or((cur, cur));
        if !replace_text_with_undo(ctx, id, &mut d.text, new_text, (ts, te)) {
            self.modal = Some(Modal::Info(replace_error_message(
                &search::ReplaceError::OutOfMemory,
            )));
            return;
        }
        d.rev += 1;
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

    /// すべての一致を置換する
    fn replace_all_matches(&mut self, ctx: &egui::Context) {
        if self.find.query.is_empty() {
            self.status = "検索語を入力してください".into();
            return;
        }
        let cs = self.find.case_sensitive;
        let (query, repl) = (self.find.query.clone(), self.find.replacement.clone());
        let d = &mut self.docs[self.active];
        d.ensure_matches(&query, cs);
        let Some(cache) = d.find_cache.as_ref() else {
            return;
        };
        let n = cache.byte_ranges.len();
        if n == 0 {
            self.status = "見つかりません".into();
            return;
        }
        // 置換後の大きさを先に計算し、上限を超えるときは何も複製せずに断る
        let new_text = match search::replace_ranges(
            &d.text,
            &cache.byte_ranges,
            &repl,
            search::MAX_REPLACE_RESULT,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.modal = Some(Modal::Info(replace_error_message(&e)));
                return;
            }
        };
        let cur = d.cursor_idx.min(new_text.chars().count());
        if !replace_text_with_undo(ctx, editor_id(d.id), &mut d.text, new_text, (cur, cur)) {
            self.modal = Some(Modal::Info(replace_error_message(
                &search::ReplaceError::OutOfMemory,
            )));
            return;
        }
        d.rev += 1;
        d.update_dirty();
        d.cursor_idx = cur;
        d.line_col = line_col(&d.text, cur);
        self.focus_editor = true;
        self.status = format!("{} 件を置換しました (Ctrl+Z で元に戻せます)", n);
        ctx.request_repaint();
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
                self.modal = None;
                if let Some(i) = self.index_of(id) {
                    self.save_then(ctx, i, false, Then::Close(id));
                }
            }
            Action::DiscardAndClose(id) => {
                self.modal = None;
                if let Some(i) = self.index_of(id) {
                    self.close_now(i);
                }
            }
            Action::SaveAllAndExit => self.save_all_and_exit(ctx),
            Action::ExitNow => {
                self.modal = None;
                self.allow_exit = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Action::SaveConfirmed {
                id,
                path,
                then,
                ok,
            } => {
                self.modal = None;
                if let Some(i) = self.index_of(id) {
                    if self.write_doc(ctx, i, path, then, ok) {
                        self.resume(ctx, then);
                    }
                }
            }
            Action::SaveAsFor(id, then) => {
                self.modal = None;
                if let Some(i) = self.index_of(id) {
                    self.save_then(ctx, i, true, then);
                }
            }
            Action::ReloadNow(id, enc) => {
                self.modal = None;
                self.start_reload(id, enc);
            }
            Action::OpenAnyway(path) => {
                self.modal = None;
                self.open_path_checked(path, true);
            }
            Action::DismissModal => self.modal = None,
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
                    let title = if d.dirty {
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
    fn find_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Action>) {
        let qid = egui::Id::new("find_query");
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

        // 件数の表示 (文書の検索結果キャッシュを使う)
        let has_query = !self.find.query.is_empty();
        let label = if has_query {
            let cs = self.find.case_sensitive;
            let d = &mut self.docs[self.active];
            d.ensure_matches(&self.find.query, cs);
            let matches = d.matches();
            if matches.is_empty() {
                "見つかりません".to_owned()
            } else {
                let (s, e) =
                    editor_sel(ui.ctx(), editor_id(d.id)).unwrap_or((d.cursor_idx, d.cursor_idx));
                let cur = search::current_index(matches, s, e).unwrap_or(0);
                format!("{} 件中 {} 件目", matches.len(), cur + 1)
            }
        } else {
            String::new()
        };

        // 幅が狭いときは折り返して、ボタンが欄の外に切れないようにする
        ui.horizontal_wrapped(|ui| {
            ui.label("検索:");
            let r = singleline_with_preedit(ui, &mut self.find.query, Some(qid), composing, &segs);
            if std::mem::take(&mut self.find.focus_query) {
                r.request_focus();
            }
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
        let focus = std::mem::take(&mut self.focus_editor);
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
        if focus {
            ui.memory_mut(|m| m.request_focus(id));
        }
        // 自動インデント付きの改行 (egui の TextEdit が処理する前に Enter を取り除いて挿入する)
        if auto_indent && !composing {
            apply_auto_indent(ui.ctx(), id, &d.text);
        }
        if find_on {
            // 文書や検索語が変わったときだけ一致を数え直す
            d.ensure_matches(&self.find.query, cs);
        }
        // 行数は版が変わったときだけ数え直す
        let (_, line_count) = d.stats();
        let rev = d.rev;
        // 以降は欄ごとに独立して借りる (本文を TextEdit に渡しながら、キャッシュも更新するため)
        let Doc {
            text,
            find_cache,
            layout_cache,
            row_lines,
            cursor_idx,
            line_col: line_col_pos,
            id: doc_id,
            ..
        } = &mut *d;
        let doc_id = *doc_id;
        let find_cache = if find_on { find_cache.as_ref() } else { None };
        let matches: &[(usize, usize)] = find_cache.map_or(&[], |c| c.matches.as_slice());
        let byte_ranges: &[(usize, usize)] = find_cache.map_or(&[], |c| c.byte_ranges.as_slice());
        let find_stamp = find_cache.map_or(0, |c| c.stamp);
        // 選択がちょうど一致箇所と同じなら、それを「現在の一致」として濃く表示する
        let exact = editor_sel(ui.ctx(), id).and_then(|sel| matches.iter().position(|&m| m == sel));
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
                    // キャッシュを使えるのは 1 回目だけ。2 回目は本文が変わっているため、
                    // キャッシュの読み書きも検索結果 (古い位置) の強調もしない
                    let calls = std::cell::Cell::new(0u32);
                    let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
                        let first = calls.get() == 0;
                        calls.set(calls.get() + 1);
                        let text_color = ui.visuals().text_color();
                        let key = LayoutKey {
                            rev,
                            wrap_width: if wrap { wrap_width.to_bits() } else { 0 },
                            font_size: font.size.to_bits(),
                            text_color,
                            dark,
                            pixels_per_point: ui.ctx().pixels_per_point().to_bits(),
                            find_stamp,
                            exact,
                        };
                        if first {
                            if let Some(c) = layout_cache.as_ref() {
                                if c.key == key {
                                    return c.galley.clone();
                                }
                            }
                        }
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
                        let galley = ui.fonts(|f| f.layout_job(job));
                        if first {
                            *layout_cache = Some(LayoutCache {
                                key,
                                galley: galley.clone(),
                            });
                        }
                        galley
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
            d.rev += 1;
            d.update_dirty();
            ui.ctx().request_repaint();
        }
    }
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
    let fresh = matches!(cache, Some(c) if Arc::ptr_eq(&c.galley, galley));
    if !fresh {
        *cache = Some(RowLines {
            galley: galley.clone(),
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
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let time = raw_input.time.unwrap_or(0.0);
        self.ime_guard.filter(time, &mut raw_input.events);
        normalize_text_events(&mut raw_input.events);
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
        // 変換中は IME から文節 (どれが変換対象か) を取得する。失敗時は空 (全体表示)
        if self.ime_guard.composing {
            self.ime_guard.segs = ime::query(hwnd, &self.ime_guard.preedit);
        } else if !self.ime_guard.segs.is_empty() {
            self.ime_guard.segs.clear();
        }
        let mut acts = Vec::new();

        // バックグラウンドの読み込みが終わっていれば取り込む
        self.poll_loads(ctx);

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

        // ウィンドウを閉じる要求。未保存の文書があるかの判断は、この描画の編集欄の処理が
        // 終わった後で行う (同じ描画で最初の入力と閉じる要求が届くと、入力による変更ありへの
        // 切り替えが画面の処理の途中になり、ここで判断すると見逃してしまうため)
        let close_requested = ctx.input(|i| i.viewport().close_requested());

        let enabled = self.modal.is_none();
        if enabled {
            self.shortcuts(ctx, &mut acts);
        }

        // 文字数・行数 (版が変わったときだけ数え直す)
        self.docs[self.active].stats();

        // ウィンドウタイトル
        let d = &self.docs[self.active];
        let title = format!(
            "{}{} - {} v{}",
            if d.dirty { "*" } else { "" },
            d.title(),
            APP_NAME,
            APP_VERSION
        );
        if title != self.last_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

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

        // 編集欄の処理が終わったので、未保存の文書があるか確かめる。あれば閉じるのを取り消して確認する
        // (なければ、閉じる要求をそのまま通す)
        if close_requested && !self.allow_exit && self.docs.iter().any(|d| d.dirty) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.modal = Some(Modal::ConfirmExit);
        }

        if let Some(m) = self.modal.clone() {
            self.show_modal(ctx, m, &mut acts);
        }

        for a in acts {
            self.run(ctx, a);
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
                let n = self.docs.iter().filter(|d| d.dirty).count();
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
                                ok: Confirmed { lossy: true, ..ok },
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
                                    ..ok
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
            } => {
                win("ファイルが変更されています").show(ctx, |ui| {
                    ui.label(format!(
                        "このファイルは、開いた後に他のプログラムで変更または削除されています:\n{}\n\n\
                         上書きすると、その変更は失われます。",
                        path.display()
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("上書きする").clicked() {
                            acts.push(Action::SaveConfirmed {
                                id,
                                path: path.clone(),
                                then,
                                ok: Confirmed {
                                    conflict: true,
                                    ..ok
                                },
                            });
                        }
                        if ui.button("キャンセル").clicked() {
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
        fn frame(&mut self, dt: f64, events: Vec<egui::Event>) {
            self.t += dt;
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
            let _ = self.ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(fld).id(field()));
                    if ai {
                        apply_auto_indent(ctx, ed(), text);
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

    /// 同じフレームの入力の順序を保つ (先に来た文字の後ろで改行する)
    #[test]
    fn auto_indent_keeps_event_order() {
        let mut h = Harness::with_indent("  a");
        h.select(3, 3);
        h.frame(0.05, vec![egui::Event::Text("b".into()), enter()]);
        assert_eq!(h.text, "  ab\n  ");
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

    fn open(app: &mut App, p: &Path) {
        app.open_path(p.to_path_buf());
        wait_loads(app);
    }

    /// 文書を編集した状態にする (編集欄の変更と同じ手順)
    fn edit(app: &mut App, idx: usize, f: impl FnOnce(&mut String)) {
        let d = &mut app.docs[idx];
        f(&mut d.text);
        d.rev += 1;
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
        let act = match app.modal.clone() {
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
            }) => Action::SaveConfirmed {
                id,
                path,
                then,
                ok: Confirmed {
                    conflict: true,
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
        assert!(matches!(app.modal, Some(Modal::Info(_))));
        assert_eq!(app.docs.len(), 1);
        assert!(app.docs[0].path.is_none());
        // UTF-32 は未対応として開かない
        let u32p = t.file("u32.txt", &[0xFF, 0xFE, 0, 0, b'a', 0, 0, 0]);
        app.modal = None;
        open(&mut app, &u32p);
        assert!(matches!(&app.modal, Some(Modal::Info(m)) if m.contains("UTF-32")));
        assert!(app.docs[0].path.is_none());
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
        assert!(matches!(app.modal, Some(Modal::ConfirmLargeOpen { .. })));
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
        assert!(app.modal.is_none());
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
        assert!(matches!(app.modal, Some(Modal::LossyLoad { .. })));
        assert_eq!(std::fs::read(&p).unwrap(), bad, "確認の前には書かない");
        confirm(&mut app);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            [0xEF, 0xBB, 0xBF, b'a', 0xEF, 0xBF, 0xBD, b'b']
        );
        assert!(!app.docs[0].lossy_load);
        assert!(!app.docs[0].dirty);
        assert!(app.modal.is_none());
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
            app.modal,
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
        assert!(matches!(app.modal, Some(Modal::Conflict { .. })));
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
            app.modal,
            Some(Modal::Conflict {
                then: Then::SaveAll,
                ..
            })
        ));
        assert!(!app.allow_exit);
        assert_eq!(std::fs::read(&a).unwrap(), b"external");
        confirm(&mut app);
        assert!(app.modal.is_none());
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
            matches!(&app.modal, Some(Modal::Info(m)) if m.contains("別のタブで開いています"))
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
        assert!(matches!(&app.modal, Some(Modal::Info(m)) if m.contains("保存に失敗")));
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
            match &app.modal {
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
        assert!(matches!(app.modal, Some(Modal::ConfirmReload { .. })));
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
        assert!(app.modal.is_none(), "{:?}", app.status);
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
        assert!(matches!(app.modal, Some(Modal::Info(_))));
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
        reset_editor_state(&ctx, id);
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
        assert!(matches!(app.modal, Some(Modal::ConfirmExit)));
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
        assert!(app.modal.is_none());
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

    /// 検索の強調付きでも描画でき、本文が変わらない間は表示 (galley) を使い回す。
    /// 本文が変わると作り直す。検索バー (折り返し付き) を開いたままの描画も確かめる
    #[test]
    fn layout_is_cached_until_text_changes() {
        let ctx = egui::Context::default();
        let mut app = App::blank(ctx.clone());
        app.docs[0].text = "abc abc
xyz abc".into();
        app.docs[0].rev = 1;
        app.find.open = true;
        app.find.show_replace = true;
        app.find.query = "abc".into();
        let mut t = 0.0;
        frame(&ctx, &mut app, &mut t, vec![], false);
        frame(&ctx, &mut app, &mut t, vec![], false);
        let g1 = app.docs[0].layout_cache.as_ref().unwrap().galley.clone();
        assert_eq!(app.docs[0].matches().len(), 3);
        frame(&ctx, &mut app, &mut t, vec![], false);
        let g2 = app.docs[0].layout_cache.as_ref().unwrap().galley.clone();
        assert!(Arc::ptr_eq(&g1, &g2), "変わらなければ再レイアウトしない");
        // 入力で本文が変わると、次の描画で作り直される
        frame(&ctx, &mut app, &mut t, vec![egui::Event::Text("Q".into())], false);
        frame(&ctx, &mut app, &mut t, vec![], false);
        let g3 = app.docs[0].layout_cache.as_ref().unwrap().galley.clone();
        assert!(!Arc::ptr_eq(&g1, &g3));
        assert!(g3.text().contains('Q'));
        assert_eq!(app.docs[0].rev, 2);
    }
}
