// リリースビルドではコンソールウィンドウを表示しない
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encoding;
mod ime;
mod search;

use eframe::egui;
use egui::text::{CCursor, CCursorRange};
use egui::widgets::text_edit::TextEditState;
use egui::{Key, KeyboardShortcut, Modifiers, ViewportCommand};
use encoding::{Enc, Eol};
use std::path::PathBuf;

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

/// 1つのタブ (= 1つの文書)
struct Doc {
    id: u64,
    path: Option<PathBuf>,
    untitled_no: usize,
    text: String,
    /// ディスク上のバイト列 (文字コードを指定して再読み込みするために保持)
    raw: Option<Vec<u8>>,
    enc: Enc,
    eol: Eol,
    dirty: bool,
    cursor_idx: usize,
    line_col: (usize, usize),
    /// 文字列を変更するたびに増やす版番号 (検索結果のキャッシュ判定に使う)
    rev: u64,
    find_cache: Option<FindCache>,
}

/// 検索結果のキャッシュ。検索語・検索条件・文書の版が変わったときだけ作り直す
struct FindCache {
    rev: u64,
    query: String,
    case_sensitive: bool,
    /// 一致箇所 (文字位置の範囲)
    matches: Vec<(usize, usize)>,
}

impl Doc {
    fn title(&self) -> String {
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            None => format!("無題 {}", self.untitled_no),
        }
    }

    /// 検索結果が古ければ作り直す
    fn ensure_matches(&mut self, query: &str, case_sensitive: bool) {
        let fresh = matches!(
            &self.find_cache,
            Some(c) if c.rev == self.rev && c.case_sensitive == case_sensitive && c.query == query
        );
        if !fresh {
            self.find_cache = Some(FindCache {
                rev: self.rev,
                query: query.to_owned(),
                case_sensitive,
                matches: search::find_all(&self.text, query, case_sensitive),
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
fn replace_text_with_undo(
    ctx: &egui::Context,
    id: egui::Id,
    text: &mut String,
    new_text: String,
    new_sel: (usize, usize),
) {
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let old_range = state
        .cursor
        .char_range()
        .unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));
    let new_range = CCursorRange::two(CCursor::new(new_sel.0), CCursor::new(new_sel.1));
    let mut undoer = state.undoer();
    undoer.add_undo(&(old_range, text.clone()));
    *text = new_text;
    undoer.add_undo(&(new_range, text.clone()));
    state.set_undoer(undoer);
    state.cursor.set_char_range(Some(new_range));
    state.store(ctx, id);
}

/// 自動インデント: 選択範囲 `sel` (文字位置, 昇順) を「改行 + 現在行のインデント」で置き換えた
/// 本文と、挿入後のキャレット位置 (文字位置) を返す。
///
/// インデントは現在行の行頭にある空白・タブを (キャレットより手前の分だけ) そのまま写す。
/// 本文は常に "\n" 区切り (読み込み時に正規化し、保存時に改行コードへ戻す)。
fn indent_newline(text: &str, sel: (usize, usize)) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let s = sel.0.min(chars.len());
    let e = sel.1.clamp(s, chars.len());
    let line_start = chars[..s]
        .iter()
        .rposition(|&c| c == '\n')
        .map_or(0, |p| p + 1);
    let indent: String = chars[line_start..s]
        .iter()
        .take_while(|&&c| c == ' ' || c == '\t')
        .collect();
    let mut out: String = chars[..s].iter().collect();
    out.push('\n');
    out.push_str(&indent);
    out.extend(&chars[e..]);
    (out, s + 1 + indent.chars().count())
}

/// 編集欄にフォーカスがあるとき、Enter (Ctrl/Alt なし。Shift 付きも含む) の押下を入力から取り除き、
/// 代わりに自動インデント付きの改行を挿入する。挿入した場合は true を返す。
///
/// egui 自身の改行処理に任せると Undo が「改行」と「インデント」に分かれてしまうため、
/// `replace_text_with_undo` で前後の状態を履歴に積み、Ctrl+Z 1 回で両方が戻るようにする。
/// IME の確定 Enter は `ImeGuard` が先に取り除くので、ここへは届かない。
fn apply_auto_indent(ctx: &egui::Context, id: egui::Id, text: &mut String) -> bool {
    if !ctx.memory(|m| m.has_focus(id)) {
        return false;
    }
    let is_enter = |e: &egui::Event| {
        matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. }
            if !modifiers.ctrl && !modifiers.alt && !modifiers.command && !modifiers.mac_cmd)
    };
    let count = ctx.input(|i| i.events.iter().filter(|e| is_enter(e)).count());
    if count == 0 {
        return false;
    }
    let Some(range) = TextEditState::load(ctx, id).and_then(|s| s.cursor.char_range()) else {
        return false; // 状態がまだ無いときは egui の通常の改行に任せる
    };
    ctx.input_mut(|i| i.events.retain(|e| !is_enter(e)));
    let [a, b] = range.sorted();
    let mut sel = (a.index, b.index);
    let mut new_text = text.clone();
    for _ in 0..count {
        let (t, caret) = indent_newline(&new_text, sel);
        new_text = t;
        sel = (caret, caret);
    }
    replace_text_with_undo(ctx, id, text, new_text, sel);
    true
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
) {
    use ime::Kind;
    let accent = ui.visuals().selection.stroke.color;
    let text_color = ui.visuals().text_color();
    let painter = ui.painter();
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
        paint_preedit(ui, &out.galley, out.galley_pos, r, segs, sel_fill, slot);
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
        let r = out
            .galley
            .pos_from_ccursor(CCursor::new(start))
            .translate(out.galley_pos.to_vec2());
        transform * egui::Rect::from_min_size(r.min, egui::vec2(1.0, r.height()))
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

#[derive(Clone)]
enum Modal {
    ConfirmClose(u64),
    ConfirmExit,
    Lossy { id: u64, path: PathBuf },
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
    ForceWrite(u64, PathBuf),
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
                let after_commit = self.last_commit.is_some_and(|c| time - c <= Self::GRACE);
                !(self.composing || after_commit)
            }
            _ => true,
        });
    }
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
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
        };
        app.new_doc();
        // コマンドライン引数 (「プログラムから開く」やexeへのドラッグ&ドロップ) のファイルを開く
        for arg in std::env::args_os().skip(1) {
            app.open_path(PathBuf::from(arg));
        }
        app
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    fn new_doc(&mut self) {
        self.untitled_counter += 1;
        let id = self.alloc_id();
        self.docs.push(Doc {
            id,
            path: None,
            untitled_no: self.untitled_counter,
            text: String::new(),
            raw: None,
            enc: Enc::Utf8,
            eol: Eol::CrLf,
            dirty: false,
            cursor_idx: 0,
            line_col: (1, 1),
            rev: 0,
            find_cache: None,
        });
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
        // 既に開いているファイルならそのタブに切り替える
        if let Some(i) = self
            .docs
            .iter()
            .position(|d| d.path.as_ref() == Some(&path))
        {
            self.active = i;
            self.focus_editor = true;
            return;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                self.modal = Some(Modal::Info(format!(
                    "ファイルを開けませんでした:\n{}\n\n{}",
                    path.display(),
                    e
                )));
                return;
            }
        };
        let enc = encoding::detect(&bytes);
        let (text, err) = encoding::decode(&bytes, enc);
        let eol = encoding::detect_eol(&text);
        let text = encoding::normalize_eol(text);
        let id = self.alloc_id();
        let doc = Doc {
            id,
            path: Some(path),
            untitled_no: 0,
            text,
            raw: Some(bytes),
            enc,
            eol,
            dirty: false,
            cursor_idx: 0,
            line_col: (1, 1),
            rev: 0,
            find_cache: None,
        };
        self.status = format!(
            "{} を開きました ({} / {}){}",
            doc.title(),
            enc.label(),
            eol.label(),
            if err {
                " ※デコードできないバイトがありました"
            } else {
                ""
            }
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

    /// 保存。成功したら true
    fn save(&mut self, idx: usize, save_as: bool) -> bool {
        let path = match (&self.docs[idx].path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let d = &self.docs[idx];
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
        self.write(idx, path, false)
    }

    fn write(&mut self, idx: usize, path: PathBuf, force: bool) -> bool {
        let d = &self.docs[idx];
        let out = encoding::apply_eol(&d.text, d.eol);
        let (bytes, lossy) = encoding::encode(&out, d.enc);
        if lossy && !force {
            self.modal = Some(Modal::Lossy { id: d.id, path });
            return false;
        }
        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                let d = &mut self.docs[idx];
                d.path = Some(path);
                d.raw = Some(bytes);
                d.dirty = false;
                self.status = format!(
                    "{} を保存しました ({} / {})",
                    d.title(),
                    d.enc.label(),
                    d.eol.label()
                );
                true
            }
            Err(e) => {
                self.modal = Some(Modal::Info(format!(
                    "保存に失敗しました:\n{}\n\n{}",
                    path.display(),
                    e
                )));
                false
            }
        }
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

    /// 文字コードを指定してディスクから読み直す (文字化けの修正用)
    fn reload_as(&mut self, enc: Enc) {
        let d = &mut self.docs[self.active];
        let Some(raw) = &d.raw else {
            self.status = "保存されていない文書は再読み込みできません".into();
            return;
        };
        let (text, err) = encoding::decode(raw, enc);
        d.eol = encoding::detect_eol(&text);
        d.text = encoding::normalize_eol(text);
        d.enc = enc;
        d.dirty = false;
        d.rev += 1;
        d.cursor_idx = usize::MAX;
        self.status = format!(
            "{} として再読み込みしました{}",
            enc.label(),
            if err {
                " ※デコードできないバイトがありました"
            } else {
                ""
            }
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
        d.dirty = true;
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
            d.dirty = true;
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
        let mut new_text = d.text.clone();
        new_text.replace_range(bs..be, &repl);
        let cur = sel_s + repl.chars().count();
        // 置換後の本文で次の一致を探し、それを選択した状態にする
        let new_matches = search::find_all(&new_text, &query, cs);
        let next = search::next_index(&new_matches, cur).map(|i| new_matches[i]);
        let (ts, te) = next.unwrap_or((cur, cur));
        replace_text_with_undo(ctx, id, &mut d.text, new_text, (ts, te));
        d.dirty = true;
        d.rev += 1;
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
        let (new_text, n) = search::replace_all(&d.text, &query, &repl, cs);
        if n == 0 {
            self.status = "見つかりません".into();
            return;
        }
        let cur = d.cursor_idx.min(new_text.chars().count());
        replace_text_with_undo(ctx, editor_id(d.id), &mut d.text, new_text, (cur, cur));
        d.dirty = true;
        d.rev += 1;
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
                self.save(self.active, false);
            }
            Action::SaveAs => {
                self.save(self.active, true);
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
            Action::Reload(e) => self.reload_as(e),
            Action::Convert(e) => self.convert_to(e),
            Action::SetEol(e) => self.set_eol(e),
            Action::SaveAndClose(id) => {
                self.modal = None;
                if let Some(i) = self.index_of(id) {
                    if self.save(i, false) {
                        self.close_now(i);
                    }
                }
            }
            Action::DiscardAndClose(id) => {
                self.modal = None;
                if let Some(i) = self.index_of(id) {
                    self.close_now(i);
                }
            }
            Action::SaveAllAndExit => {
                self.modal = None;
                let mut i = 0;
                while i < self.docs.len() {
                    if self.docs[i].dirty {
                        self.active = i;
                        if !self.save(i, false) {
                            return; // キャンセルまたは失敗したら終了を中止
                        }
                    }
                    i += 1;
                }
                self.allow_exit = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Action::ExitNow => {
                self.modal = None;
                self.allow_exit = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Action::ForceWrite(id, path) => {
                self.modal = None;
                if let Some(i) = self.index_of(id) {
                    self.write(i, path, true);
                }
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
            let has_raw = self.docs[self.active].raw.is_some();
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
                ui.add_enabled_ui(has_raw, |ui| {
                    ui.menu_button("指定した文字コードで開き直す", |ui| {
                        ui.label("文字化けしたときに使います (未保存の編集は破棄されます)");
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
                    frame.show(ui, |ui| {
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

        ui.horizontal(|ui| {
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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("×").on_hover_text("閉じる (Esc)").clicked() {
                    acts.push(Action::CloseFind);
                }
            });
        });

        if self.find.show_replace {
            ui.horizontal(|ui| {
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
        ui.horizontal(|ui| {
            ui.label(&self.status);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("{} v{}", APP_NAME, APP_VERSION));
                ui.separator();
                ui.label(d.enc.label());
                ui.separator();
                ui.label(d.eol.label());
                ui.separator();
                ui.label(format!("{} 文字", d.text.chars().count()));
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
        let indented = auto_indent && !composing && apply_auto_indent(ui.ctx(), id, &mut d.text);
        if indented {
            d.dirty = true;
            d.rev += 1;
        }
        if find_on {
            // 文書や検索語が変わったときだけ一致を数え直す
            d.ensure_matches(&self.find.query, cs);
        }
        let matches: &[(usize, usize)] = if find_on {
            d.find_cache
                .as_ref()
                .map(|c| c.matches.as_slice())
                .unwrap_or(&[])
        } else {
            &[]
        };
        // 選択がちょうど一致箇所と同じなら、それを「現在の一致」として濃く表示する
        let exact = editor_sel(ui.ctx(), id).and_then(|sel| matches.iter().position(|&m| m == sel));
        let colors = match_colors(dark);

        // 行番号の桁数 (行数は改行の数から求める)
        let line_count = d.text.bytes().filter(|&b| b == b'\n').count() + 1;
        let digits = line_count.to_string().len();
        let cur_line = d.line_col.0;

        let scroll = if wrap {
            egui::ScrollArea::vertical()
        } else {
            egui::ScrollArea::both()
        };
        ui.horizontal_top(|ui| {
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
                .id_salt(("scroll", d.id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
                        let mut job = egui::text::LayoutJob::simple(
                            s.to_owned(),
                            font.clone(),
                            ui.visuals().text_color(),
                            if wrap { wrap_width } else { f32::INFINITY },
                        );
                        if !matches.is_empty() {
                            let ranges = search::to_byte_ranges(s, matches);
                            let sections = highlight_sections(
                                s,
                                &font,
                                ui.visuals().text_color(),
                                &ranges,
                                exact,
                                colors,
                            );
                            if !sections.is_empty() {
                                job.sections = sections;
                            }
                        }
                        ui.fonts(|f| f.layout_job(job))
                    };
                    let old_fill = hide_selection_fill(ui, composing);
                    let fill_slot = reserve_preedit_slot(ui);
                    let out = egui::TextEdit::multiline(&mut d.text)
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
                    if out.response.changed() {
                        d.dirty = true;
                        d.rev += 1;
                    }
                    if let Some(cr) = out.cursor_range {
                        let idx = cr.primary.ccursor.index;
                        if idx != d.cursor_idx || out.response.changed() || indented {
                            d.cursor_idx = idx;
                            d.line_col = line_col(&d.text, idx);
                        }
                        // 通常の Enter と同様にキャレットが見えるようにスクロールする
                        if indented {
                            let rect = out
                                .galley
                                .pos_from_cursor(&cr.primary)
                                .translate(out.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect, None);
                        }
                    }

                    // 一致箇所を画面内に表示する (折り返し後の位置を含めて galley から求める)
                    if let Some((doc_id, idx)) = scroll_req {
                        if doc_id == d.id {
                            let rect = out
                                .galley
                                .pos_from_ccursor(CCursor::new(idx))
                                .translate(out.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect, Some(egui::Align::Center));
                        }
                    }

                    (out.galley.clone(), out.galley_pos)
                });
            let (galley, galley_pos) = shown.inner;

            // 行番号: 論理行の先頭の行 (折り返しの 2 行目以降は番号を付けない) にだけ描く
            if let Some(g) = gutter {
                let painter = ui.painter_at(g);
                painter.rect_filled(g, 0.0, ui.visuals().faint_bg_color);
                let weak = ui.visuals().weak_text_color();
                let strong = ui.visuals().strong_text_color();
                let mut line_no = 1;
                let mut line_start = true;
                for row in &galley.rows {
                    let y = galley_pos.y + row.rect.min.y;
                    if y > g.bottom() {
                        break; // 以降は画面の下 (描く必要なし)
                    }
                    if line_start && y + row.rect.height() >= g.top() {
                        let color = if line_no == cur_line { strong } else { weak };
                        painter.text(
                            egui::pos2(g.right() - GUTTER_PAD / 2.0, y),
                            egui::Align2::RIGHT_TOP,
                            line_no.to_string(),
                            font.clone(),
                            color,
                        );
                    }
                    line_start = row.ends_with_newline;
                    if row.ends_with_newline {
                        line_no += 1;
                    }
                }
            }
        });
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
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 変換中は IME から文節 (どれが変換対象か) を取得する。失敗時は空 (全体表示)
        if self.ime_guard.composing {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let hwnd = match frame.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                _ => 0,
            };
            self.ime_guard.segs = ime::query(hwnd, &self.ime_guard.preedit);
        } else if !self.ime_guard.segs.is_empty() {
            self.ime_guard.segs.clear();
        }
        let mut acts = Vec::new();

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

        // 未保存の文書がある状態でウィンドウを閉じようとしたら確認
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_exit
            && self.docs.iter().any(|d| d.dirty)
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.modal = Some(Modal::ConfirmExit);
        }

        let enabled = self.modal.is_none();
        if enabled {
            self.shortcuts(ctx, &mut acts);
        }

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

        if let Some(m) = self.modal.clone() {
            self.show_modal(ctx, m, &mut acts);
        }

        for a in acts {
            self.run(ctx, a);
        }
    }
}

impl App {
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
            Modal::Lossy { id, path } => {
                let enc = self
                    .index_of(id)
                    .map(|i| self.docs[i].enc.label())
                    .unwrap_or_default();
                win("文字コードの警告").show(ctx, |ui| {
                    ui.label(format!(
                        "{} では表現できない文字が含まれています。\nそのまま保存すると、それらの文字は「&#数値;」形式に置き換えられます。",
                        enc
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("それでも保存").clicked() {
                            acts.push(Action::ForceWrite(id, path.clone()));
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
        let new_text = search::replace_all(&h.text, "abc", "Z", true).0;
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
        h.type_text("d"); // 直前の入力と履歴が混ざらないこと
        h.frame(0.05, vec![enter()]);
        h.frame(0.05, vec![]);
        assert_eq!(h.text, "    abcd\n    ");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text, "    abcd");
        h.key(Key::Y, Modifiers::COMMAND);
        assert_eq!(h.text, "    abcd\n    ");
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
