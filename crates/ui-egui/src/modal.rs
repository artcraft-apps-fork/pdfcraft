//! Vim / Evil-style modal keybinding integration for PdfCraft (`Normal`, `Insert`, `Visual`).
//!
//! State is stored per `egui::Context` rather than inside `PdfCraftApp` so upstream rebases
//! touch only two single-line hook calls in `app.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use craft_modal::{
    AlignPos, CommandCall, ModalAction, ModalEngine, Mode, Motion, Operator, ScrollDir, ScrollUnit, Target, TextObj, VisualKind, ZoomOp,
};
use egui::{Color32, Context, CornerRadius, Id, Order, Stroke, StrokeKind, pos2, vec2};
use pdfcraft_engine::{DocId, Edit, NewAnnotation, Shape};
use pdfcraft_render::PageText;

use crate::canvas::{DocView, Fit};
use crate::comments::{CommentTool, Composer, ComposerKind};
use crate::{PdfCraftApp, QuickTool};

const SCROLL_LINE_PTS: f32 = 40.0;

/// Virtual text caret position on a PDF page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PdfCaret {
    pub page: usize,
    pub glyph: usize,
    pub preferred_x: Option<f32>,
}

/// Active or remembered Visual selection state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VisualSession {
    kind: VisualKind,
    page: usize,
    anchor: usize,
    head: usize,
}

/// Per-window modal state stored in `egui::Context` memory.
pub struct PdfModalState {
    pub engine: ModalEngine,
    carets: HashMap<DocId, PdfCaret>,
    visual: HashMap<DocId, VisualSession>,
    last_visual: HashMap<DocId, VisualSession>,
    reported_rc_warnings: bool,
}

impl Default for PdfModalState {
    fn default() -> Self {
        let mut engine = ModalEngine::new("pdfcraft");
        // Keep existing unit tests in non-modal pass-through unless explicitly enabled by a test;
        // normal desktop/web builds launch directly into modal editing.
        if cfg!(test) {
            engine.options.enabled = false;
        }
        Self { engine, carets: HashMap::new(), visual: HashMap::new(), last_visual: HashMap::new(), reported_rc_warnings: false }
    }
}

fn state_handle(ctx: &Context) -> Arc<Mutex<PdfModalState>> {
    let id = Id::unique("pdfcraft.modal_state");
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Arc<Mutex<PdfModalState>>>(id).clone())
}

/// Access or mutate the window's [`PdfModalState`].
pub fn with_state<R>(ctx: &Context, f: impl FnOnce(&mut PdfModalState) -> R) -> Option<R> {
    let handle = state_handle(ctx);
    let mut guard = handle.lock().ok()?;
    Some(f(&mut guard))
}

/// Pre-shortcut hook called from `PdfCraftApp::logic` right before `self.shortcuts(ctx)`.
pub(crate) fn intercept_keys(app: &mut PdfCraftApp, ctx: &Context) {
    let handle = state_handle(ctx);
    let Ok(mut state) = handle.lock() else { return };
    if !state.reported_rc_warnings {
        state.reported_rc_warnings = true;
        if let Some(first) = state.engine.warnings().first() {
            app.notify(format!("vimrc ({}:{}): {}", first.source, first.line, first.message));
        }
    }
    if !state.engine.options.enabled {
        return;
    }

    sync_inline_editor_mode(app, ctx, &mut state);
    let bypass = should_bypass_modal(app, ctx);
    let actions = craft_modal::egui::filter_frame(ctx, &mut state.engine, bypass);
    for action in actions {
        apply_action(app, ctx, &mut state, action);
    }
}

/// Overlay painter called at the end of `PdfCraftApp::ui_unguarded` for the virtual caret and mode badge.
pub(crate) fn paint_overlay(app: &PdfCraftApp, ctx: &Context) {
    let handle = state_handle(ctx);
    let Ok(state) = handle.lock() else { return };
    if !state.engine.options.enabled {
        return;
    }
    if state.engine.mode() != Mode::Insert {
        paint_virtual_caret(app, ctx, &state);
    }
    craft_modal::egui::paint_mode_badge(ctx, &state.engine.status());
}

fn sync_inline_editor_mode(app: &PdfCraftApp, ctx: &Context, state: &mut PdfModalState) {
    let inline_editing = ctx.text_edit_focused() || active_view_has_inline_editor(app);
    if inline_editing && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::Escape, pressed: true, .. }))) {
        state.engine.set_mode(Mode::Normal);
    }
}

fn active_view_has_inline_editor(app: &PdfCraftApp) -> bool {
    let Some(view) = app.active.and_then(|i| app.views.get(i)) else { return false };
    view.forms.focus.is_some()
        || view.line_editor.is_some()
        || view.content.draft.is_some()
        || view.comments.composer.is_some()
        || view.fill_text.is_some()
}

fn should_bypass_modal(app: &PdfCraftApp, ctx: &Context) -> bool {
    app.close_request.is_some()
        || app.modal_open()
        || app.palette_open
        || app.ocr_verify.open
        || craft_menu::bar::is_open(ctx)
        || craft_menu::bar::is_focused(ctx)
        || ctx.text_edit_focused()
        || active_view_has_inline_editor(app)
}

fn paint_virtual_caret(app: &PdfCraftApp, ctx: &Context, state: &PdfModalState) {
    let Some(view) = app.active.and_then(|i| app.views.get(i)) else { return };
    if view.organize {
        return;
    }
    let viewport = view.viewport_rect();
    if !viewport.is_positive() {
        return;
    }
    let Some(caret) = state.carets.get(&view.id) else { return };
    let Some(rect) = view.glyph_screen_rect(caret.page, caret.glyph) else { return };
    if !viewport.intersects(rect) {
        return;
    }
    let draw_rect = rect.expand2(vec2(1.0, 1.5));
    let (fill, stroke_col) = if state.engine.mode().is_visual() {
        (Color32::from_rgba_unmultiplied(234, 88, 12, 70), Color32::from_rgb(234, 88, 12))
    } else {
        (Color32::from_rgba_unmultiplied(37, 99, 235, 85), Color32::from_rgb(37, 99, 235))
    };
    let painter = ctx.layer_painter(egui::LayerId::new(Order::Middle, Id::unique("pdfcraft.modal_caret"))).with_clip_rect(viewport);
    painter.rect(draw_rect, CornerRadius::same(2), fill, Stroke::new(1.5, stroke_col), StrokeKind::Outside);
}

fn apply_action(app: &mut PdfCraftApp, ctx: &Context, state: &mut PdfModalState, action: ModalAction) {
    match action {
        ModalAction::EnterMode { mode, .. } => handle_enter_mode(app, state, mode),
        ModalAction::ReselectVisual => handle_reselect_visual(app, state),
        ModalAction::SwapVisualAnchor => handle_swap_anchor(app, state),
        ModalAction::Motion { motion, count, explicit_count } => handle_motion(app, state, motion, count, explicit_count),
        ModalAction::Scroll { unit, dir, count } => handle_scroll(app, unit, dir, count),
        ModalAction::AlignView(pos) => handle_align_view(app, state, pos),
        ModalAction::Operator { op, target, .. } => handle_operator(app, ctx, state, op, target),
        ModalAction::SelectTextObject { obj, inner, count } => handle_select_text_object(app, state, obj, inner, count),
        ModalAction::Paste { .. } => handle_paste(app),
        ModalAction::Undo { count } => repeat_cmd(app, "edit.undo", count),
        ModalAction::Redo { count } => repeat_cmd(app, "edit.redo", count),
        ModalAction::SearchStart { .. } => {
            let _ = app.execute("edit.find");
        }
        ModalAction::SearchWord { forward } => handle_search_word(app, state, forward),
        ModalAction::SearchStep { forward, count } => handle_search_step(app, state, forward, count),
        ModalAction::DismissTransient => handle_dismiss(app, ctx, state),
        ModalAction::Zoom { op, count } => handle_zoom(app, op, count),
        ModalAction::JumpHistory { forward, count } => handle_jump_history(app, forward, count),
        ModalAction::StepTab { forward, count } => repeat_cmd(app, if forward { "view.next_tab" } else { "view.previous_tab" }, count),
        ModalAction::StepPage { forward, count } => handle_step_page(app, state, forward, count),
        ModalAction::ReloadConfig => notify_reload(app, state),
        ModalAction::AppCommand { cmd, count } => handle_app_command(app, &cmd, count),
    }
}

fn repeat_cmd(app: &mut PdfCraftApp, id: &str, count: usize) {
    for _ in 0..count.max(1) {
        if !app.execute(id) {
            break;
        }
    }
}

fn notify_reload(app: &mut PdfCraftApp, state: &PdfModalState) {
    if let Some(w) = state.engine.warnings().first() {
        app.notify(format!("vimrc reloaded with warning ({}:{}): {}", w.source, w.line, w.message));
    } else {
        app.notify("Reloaded modal configuration");
    }
}

fn handle_app_command(app: &mut PdfCraftApp, cmd: &CommandCall, count: usize) {
    for _ in 0..count.max(1) {
        app.run_command(&cmd.id);
    }
}

fn handle_enter_mode(app: &mut PdfCraftApp, state: &mut PdfModalState, mode: Mode) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    let doc_id = view.id;
    match mode {
        Mode::Normal => {
            if let Some(vis) = state.visual.remove(&doc_id) {
                state.last_visual.insert(doc_id, vis);
            }
            view.clear_selection();
        }
        Mode::Insert => {
            if let Some(vis) = state.visual.remove(&doc_id) {
                state.last_visual.insert(doc_id, vis);
            }
        }
        Mode::Visual(kind) => {
            let Some(caret) = ensure_caret(view, state) else { return };
            let (page, anchor, head) = match state
                .visual
                .get(&doc_id)
                .copied()
                .or_else(|| view.selection_run().map(|(p, a, h)| VisualSession { kind, page: p, anchor: a, head: h }))
            {
                Some(prev) if prev.page == caret.page => (prev.page, prev.anchor, caret.glyph),
                _ => (caret.page, caret.glyph, caret.glyph),
            };
            let sess = VisualSession { kind, page, anchor, head };
            state.visual.insert(doc_id, sess);
            sync_visual_selection(view, sess);
        }
    }
}

fn handle_reselect_visual(app: &mut PdfCraftApp, state: &mut PdfModalState) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    let Some(prev) = state.last_visual.get(&view.id).copied() else { return };
    state.engine.set_mode(Mode::Visual(prev.kind));
    state.visual.insert(view.id, prev);
    state.carets.insert(view.id, PdfCaret { page: prev.page, glyph: prev.head, preferred_x: None });
    sync_visual_selection(view, prev);
}

fn handle_swap_anchor(app: &mut PdfCraftApp, state: &mut PdfModalState) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    let Some(mut sess) = state.visual.get(&view.id).copied() else { return };
    std::mem::swap(&mut sess.anchor, &mut sess.head);
    state.visual.insert(view.id, sess);
    state.carets.insert(view.id, PdfCaret { page: sess.page, glyph: sess.head, preferred_x: None });
    sync_visual_selection(view, sess);
}

fn sync_visual_selection(view: &mut DocView, sess: VisualSession) {
    let Some(text) = view.texts.get(&sess.page).cloned() else { return };
    match sess.kind {
        VisualKind::Char => view.select_text(sess.page, sess.anchor, sess.head),
        VisualKind::Line => {
            let (a0, a1) = text.line_at(sess.anchor).unwrap_or((sess.anchor, sess.anchor));
            let (h0, h1) = text.line_at(sess.head).unwrap_or((sess.head, sess.head));
            let (from, to) = if sess.anchor <= sess.head { (a0, h1) } else { (a1, h0) };
            view.select_text(sess.page, from, to);
        }
        VisualKind::Block => {
            let (Some(ga), Some(gh)) = (text.glyphs.get(sess.anchor), text.glyphs.get(sess.head)) else { return };
            let rect = [ga.rect[0].min(gh.rect[0]), ga.rect[1].min(gh.rect[1]), ga.rect[2].max(gh.rect[2]), ga.rect[3].max(gh.rect[3])];
            view.select_column(sess.page, rect);
        }
    }
}

fn ensure_caret(view: &DocView, state: &mut PdfModalState) -> Option<PdfCaret> {
    if let Some((page, _, head)) = view.selection_run()
        && view.texts.get(&page).is_some_and(|t| head < t.glyphs.len())
    {
        let c = PdfCaret { page, glyph: head, preferred_x: None };
        state.carets.insert(view.id, c);
        return Some(c);
    }
    if let Some(existing) = state.carets.get(&view.id).copied()
        && view.texts.get(&existing.page).is_some_and(|t| existing.glyph < t.glyphs.len())
    {
        return Some(existing);
    }
    let page = view.current;
    let text = view.texts.get(&page)?;
    if text.glyphs.is_empty() {
        return None;
    }
    let glyph = first_visible_glyph(view, page, text).unwrap_or(0);
    let c = PdfCaret { page, glyph, preferred_x: None };
    state.carets.insert(view.id, c);
    Some(c)
}

fn first_visible_glyph(view: &DocView, page: usize, text: &PageText) -> Option<usize> {
    let vp = view.viewport_rect();
    let xf = view.page_xform(page)?;
    if !vp.is_positive() {
        return Some(0);
    }
    let top_left = pos2(vp.left() + 80.0, vp.top() + 24.0);
    let (vx, vy) = xf.screen_to_view(top_left.clamp(xf.rect.min, xf.rect.max));
    text.nearest(vx, vy)
}

fn handle_motion(app: &mut PdfCraftApp, state: &mut PdfModalState, motion: Motion, count: usize, explicit_count: bool) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    if matches!(motion, Motion::DocStart | Motion::DocEnd) {
        jump_document_bound(view, state, motion, count, explicit_count);
        return;
    }
    let Some(mut caret) = ensure_caret(view, state) else {
        fallback_scroll_motion(view, motion, count, app.scroll_speed.factor());
        return;
    };
    caret = step_caret_motion(view, caret, motion, count.max(1));
    state.carets.insert(view.id, caret);
    let scrolloff = state.engine.options.scrolloff;
    view.ensure_glyph_visible(caret.page, caret.glyph, scrolloff, None);
    update_visual_after_caret(view, state, caret);
}

fn jump_document_bound(view: &mut DocView, state: &mut PdfModalState, motion: Motion, count: usize, explicit_count: bool) {
    if explicit_count {
        let target_page = count.saturating_sub(1).min(view.page_count().saturating_sub(1));
        view.go_to_page(target_page);
        if let Some(t) = view.texts.get(&target_page)
            && !t.glyphs.is_empty()
        {
            let c = PdfCaret { page: target_page, glyph: 0, preferred_x: None };
            state.carets.insert(view.id, c);
            update_visual_after_caret(view, state, c);
        }
        return;
    }
    if motion == Motion::DocStart {
        view.go_to_page(0);
        if let Some(t) = view.texts.get(&0)
            && !t.glyphs.is_empty()
        {
            let c = PdfCaret { page: 0, glyph: 0, preferred_x: None };
            state.carets.insert(view.id, c);
            update_visual_after_caret(view, state, c);
        }
    } else {
        view.go_to_end();
        let last_page = view.page_count().saturating_sub(1);
        if let Some(t) = view.texts.get(&last_page)
            && let Some(last_glyph) = t.glyphs.len().checked_sub(1)
        {
            let c = PdfCaret { page: last_page, glyph: last_glyph, preferred_x: None };
            state.carets.insert(view.id, c);
            update_visual_after_caret(view, state, c);
        }
    }
}

fn update_visual_after_caret(view: &mut DocView, state: &mut PdfModalState, caret: PdfCaret) {
    if !state.engine.mode().is_visual() {
        return;
    }
    if let Some(mut sess) = state.visual.get(&view.id).copied() {
        if sess.page != caret.page {
            sess.page = caret.page;
            sess.anchor = 0;
        }
        sess.head = caret.glyph;
        state.visual.insert(view.id, sess);
        sync_visual_selection(view, sess);
    }
}

fn fallback_scroll_motion(view: &mut DocView, motion: Motion, count: usize, speed_factor: f32) {
    let step = SCROLL_LINE_PTS * speed_factor * (count.max(1) as f32);
    match motion {
        Motion::Down => view.key_scroll += step,
        Motion::Up => view.key_scroll -= step,
        Motion::Right => view.key_side += step,
        Motion::Left => view.key_side -= step,
        _ => {}
    }
}

fn step_caret_motion(view: &DocView, mut caret: PdfCaret, motion: Motion, count: usize) -> PdfCaret {
    match motion {
        Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => return screen_anchor_caret(view, caret, motion),
        _ => {}
    }
    for _ in 0..count {
        let Some(text) = view.texts.get(&caret.page) else { break };
        if text.glyphs.is_empty() {
            break;
        }
        caret = single_caret_step(view, text, caret, motion);
    }
    caret
}

fn screen_anchor_caret(view: &DocView, caret: PdfCaret, motion: Motion) -> PdfCaret {
    let vp = view.viewport_rect();
    let Some(xf) = view.page_xform(caret.page) else { return caret };
    let Some(text) = view.texts.get(&caret.page) else { return caret };
    if !vp.is_positive() {
        return caret;
    }
    let frac = match motion {
        Motion::ScreenTop => 0.12,
        Motion::ScreenMiddle => 0.50,
        _ => 0.88,
    };
    let y = vp.top() + vp.height() * frac;
    let pt = pos2(xf.rect.center().x, y).clamp(xf.rect.min, xf.rect.max);
    let (vx, vy) = xf.screen_to_view(pt);
    match text.nearest(vx, vy) {
        Some(glyph) => PdfCaret { page: caret.page, glyph, preferred_x: None },
        None => caret,
    }
}

fn single_caret_step(view: &DocView, text: &PageText, caret: PdfCaret, motion: Motion) -> PdfCaret {
    let last = text.glyphs.len().saturating_sub(1);
    match motion {
        Motion::Left => PdfCaret { page: caret.page, glyph: caret.glyph.saturating_sub(1), preferred_x: None },
        Motion::Right => PdfCaret { page: caret.page, glyph: (caret.glyph + 1).min(last), preferred_x: None },
        Motion::LineStart => {
            let g = text.line_at(caret.glyph).map_or(caret.glyph, |(s, _)| s);
            PdfCaret { page: caret.page, glyph: g, preferred_x: None }
        }
        Motion::FirstNonBlank => {
            let (s, e) = text.line_at(caret.glyph).unwrap_or((caret.glyph, caret.glyph));
            let g = (s..=e).find(|&k| !text.glyphs[k].text.trim().is_empty()).unwrap_or(s);
            PdfCaret { page: caret.page, glyph: g, preferred_x: None }
        }
        Motion::LineEnd => {
            let g = text.line_at(caret.glyph).map_or(caret.glyph, |(_, e)| e);
            PdfCaret { page: caret.page, glyph: g, preferred_x: None }
        }
        Motion::Down => step_vertical_line(view, text, caret, true),
        Motion::Up => step_vertical_line(view, text, caret, false),
        Motion::WordForward | Motion::BigWordForward => step_word_forward(view, text, caret),
        Motion::WordBack | Motion::BigWordBack => step_word_back(view, text, caret),
        Motion::WordEnd | Motion::BigWordEnd => step_word_end(text, caret),
        Motion::ParagraphNext => step_paragraph(text, caret, true),
        Motion::ParagraphPrev => step_paragraph(text, caret, false),
        Motion::FindChar { ch, forward, till } => step_find_char(text, caret, ch, forward, till),
        _ => caret,
    }
}

fn step_vertical_line(view: &DocView, text: &PageText, caret: PdfCaret, down: bool) -> PdfCaret {
    let cur_line = text.line_of.get(caret.glyph).copied().unwrap_or(0);
    let pref_x = caret.preferred_x.unwrap_or_else(|| glyph_center_x(text, caret.glyph));
    let target_line =
        if down { text.line_of.iter().copied().find(|&l| l > cur_line) } else { text.line_of.iter().rev().copied().find(|&l| l < cur_line) };
    if let Some(line) = target_line {
        let best = text
            .line_of
            .iter()
            .enumerate()
            .filter(|(_, l)| **l == line)
            .min_by(|(ia, _), (ib, _)| (glyph_center_x(text, *ia) - pref_x).abs().total_cmp(&(glyph_center_x(text, *ib) - pref_x).abs()))
            .map(|(i, _)| i)
            .unwrap_or(caret.glyph);
        return PdfCaret { page: caret.page, glyph: best, preferred_x: Some(pref_x) };
    }
    // Cross page boundary when adjacent page's text is available.
    let next_page = if down { caret.page.checked_add(1) } else { caret.page.checked_sub(1) };
    if let Some(p) = next_page
        && let Some(other) = view.texts.get(&p)
        && !other.glyphs.is_empty()
    {
        let glyph = if down { 0 } else { other.glyphs.len() - 1 };
        return PdfCaret { page: p, glyph, preferred_x: Some(pref_x) };
    }
    caret
}

fn glyph_center_x(text: &PageText, idx: usize) -> f32 {
    text.glyphs.get(idx).map_or(0.0, |g| (g.rect[0] + g.rect[2]) * 0.5)
}

fn is_word_break(text: &PageText, idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    text.space_before.get(idx).copied().unwrap_or(false)
        || text.line_of.get(idx) != text.line_of.get(idx - 1)
        || text.glyphs.get(idx - 1).is_some_and(|g| g.text.trim().is_empty())
}

fn step_word_forward(view: &DocView, text: &PageText, caret: PdfCaret) -> PdfCaret {
    let len = text.glyphs.len();
    for k in (caret.glyph + 1)..len {
        if is_word_break(text, k) && !text.glyphs[k].text.trim().is_empty() {
            return PdfCaret { page: caret.page, glyph: k, preferred_x: None };
        }
    }
    if let Some(next) = view.texts.get(&(caret.page + 1))
        && !next.glyphs.is_empty()
    {
        return PdfCaret { page: caret.page + 1, glyph: 0, preferred_x: None };
    }
    PdfCaret { page: caret.page, glyph: len.saturating_sub(1), preferred_x: None }
}

fn step_word_back(view: &DocView, text: &PageText, caret: PdfCaret) -> PdfCaret {
    if caret.glyph > 0 {
        let mut k = caret.glyph - 1;
        while k > 0 && text.glyphs[k].text.trim().is_empty() {
            k -= 1;
        }
        while k > 0 && !is_word_break(text, k) {
            k -= 1;
        }
        return PdfCaret { page: caret.page, glyph: k, preferred_x: None };
    }
    if let Some(prev_page) = caret.page.checked_sub(1)
        && let Some(prev) = view.texts.get(&prev_page)
        && !prev.glyphs.is_empty()
    {
        return PdfCaret { page: prev_page, glyph: prev.glyphs.len() - 1, preferred_x: None };
    }
    caret
}

fn step_word_end(text: &PageText, caret: PdfCaret) -> PdfCaret {
    let last = text.glyphs.len().saturating_sub(1);
    let mut k = (caret.glyph + 1).min(last);
    while k < last && text.glyphs[k].text.trim().is_empty() {
        k += 1;
    }
    while k < last && !is_word_break(text, k + 1) && !text.glyphs[k + 1].text.trim().is_empty() {
        k += 1;
    }
    PdfCaret { page: caret.page, glyph: k, preferred_x: None }
}

fn step_paragraph(text: &PageText, caret: PdfCaret, forward: bool) -> PdfCaret {
    let line_starts = collect_line_starts(text);
    if line_starts.is_empty() {
        return caret;
    }
    let cur_pos = line_starts.partition_point(|&g| g <= caret.glyph).saturating_sub(1);
    if forward {
        for idx in (cur_pos + 1)..line_starts.len() {
            if is_paragraph_gap(text, line_starts[idx - 1], line_starts[idx]) {
                return PdfCaret { page: caret.page, glyph: line_starts[idx], preferred_x: None };
            }
        }
        PdfCaret { page: caret.page, glyph: text.glyphs.len().saturating_sub(1), preferred_x: None }
    } else {
        for idx in (1..=cur_pos).rev() {
            if is_paragraph_gap(text, line_starts[idx - 1], line_starts[idx]) && line_starts[idx] < caret.glyph {
                return PdfCaret { page: caret.page, glyph: line_starts[idx], preferred_x: None };
            }
        }
        PdfCaret { page: caret.page, glyph: 0, preferred_x: None }
    }
}

fn collect_line_starts(text: &PageText) -> Vec<usize> {
    let mut out = Vec::new();
    let mut prev_line = None;
    for (idx, &line) in text.line_of.iter().enumerate() {
        if prev_line != Some(line) {
            out.push(idx);
            prev_line = Some(line);
        }
    }
    out
}

fn is_paragraph_gap(text: &PageText, prev_start: usize, cur_start: usize) -> bool {
    let (Some(a), Some(b)) = (text.glyphs.get(prev_start), text.glyphs.get(cur_start)) else { return false };
    let line_h = (a.rect[3] - a.rect[1]).abs().max(8.0);
    let dy = (b.rect[1] - a.rect[3]).max(0.0);
    dy > line_h * 0.45
}

fn step_find_char(text: &PageText, caret: PdfCaret, target_ch: char, forward: bool, till: bool) -> PdfCaret {
    let Some((line_start, line_end)) = text.line_at(caret.glyph) else { return caret };
    if forward {
        for k in (caret.glyph + 1)..=line_end {
            if text.glyphs[k].text.contains(target_ch) {
                let g = if till { k.saturating_sub(1).max(caret.glyph) } else { k };
                return PdfCaret { page: caret.page, glyph: g, preferred_x: None };
            }
        }
    } else {
        for k in (line_start..caret.glyph).rev() {
            if text.glyphs[k].text.contains(target_ch) {
                let g = if till { (k + 1).min(caret.glyph) } else { k };
                return PdfCaret { page: caret.page, glyph: g, preferred_x: None };
            }
        }
    }
    caret
}

fn handle_scroll(app: &mut PdfCraftApp, unit: ScrollUnit, dir: ScrollDir, count: usize) {
    let Some(i) = app.active else { return };
    let speed = app.scroll_speed.factor();
    let view = &mut app.views[i];
    let vh = view.viewport_rect().height().max(240.0);
    let n = count.max(1) as f32;
    let amount = match unit {
        ScrollUnit::Line => SCROLL_LINE_PTS * speed * n,
        ScrollUnit::HalfPage => (vh * 0.5) * n,
        ScrollUnit::FullPage => {
            if !view.continuous {
                for _ in 0..count.max(1) {
                    view.step_page(matches!(dir, ScrollDir::Down | ScrollDir::Right));
                }
                return;
            }
            (vh - SCROLL_LINE_PTS).max(SCROLL_LINE_PTS) * n
        }
    };
    match dir {
        ScrollDir::Down => view.key_scroll += amount,
        ScrollDir::Up => view.key_scroll -= amount,
        ScrollDir::Right => view.key_side += amount,
        ScrollDir::Left => view.key_side -= amount,
    }
}

fn handle_align_view(app: &mut PdfCraftApp, state: &mut PdfModalState, pos: AlignPos) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    let Some(caret) = ensure_caret(view, state) else { return };
    let frac = match pos {
        AlignPos::Top => 0.10,
        AlignPos::Center => 0.50,
        AlignPos::Bottom => 0.90,
    };
    view.ensure_glyph_visible(caret.page, caret.glyph, 0, Some(frac));
}

fn handle_select_text_object(app: &mut PdfCraftApp, state: &mut PdfModalState, obj: TextObj, inner: bool, _count: usize) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    let Some(caret) = ensure_caret(view, state) else { return };
    let Some((start, end)) = resolve_text_object_bounds(view, caret, obj, inner) else { return };
    let sess = VisualSession { kind: VisualKind::Char, page: caret.page, anchor: start, head: end };
    state.visual.insert(view.id, sess);
    state.carets.insert(view.id, PdfCaret { page: caret.page, glyph: end, preferred_x: None });
    sync_visual_selection(view, sess);
}

fn resolve_text_object_bounds(view: &DocView, caret: PdfCaret, obj: TextObj, _inner: bool) -> Option<(usize, usize)> {
    let text = view.texts.get(&caret.page)?;
    match obj {
        TextObj::Word | TextObj::BigWord => text.word_at(caret.glyph),
        TextObj::Line => text.line_at(caret.glyph),
        TextObj::Paragraph => paragraph_bounds_at(text, caret.glyph),
    }
}

fn paragraph_bounds_at(text: &PageText, glyph: usize) -> Option<(usize, usize)> {
    text.glyphs.get(glyph)?;
    let starts = collect_line_starts(text);
    if starts.is_empty() {
        return None;
    }
    let cur = starts.partition_point(|&g| g <= glyph).saturating_sub(1);
    let mut first_line = cur;
    while first_line > 0 && !is_paragraph_gap(text, starts[first_line - 1], starts[first_line]) {
        first_line -= 1;
    }
    let mut last_line = cur;
    while last_line + 1 < starts.len() && !is_paragraph_gap(text, starts[last_line], starts[last_line + 1]) {
        last_line += 1;
    }
    let start_glyph = starts[first_line];
    let end_glyph = text.line_at(starts[last_line]).map_or(starts[last_line], |(_, e)| e);
    Some((start_glyph, end_glyph))
}

fn handle_operator(app: &mut PdfCraftApp, ctx: &Context, state: &mut PdfModalState, op: Operator, target: Target) {
    let Some(i) = app.active else { return };
    if matches!(op, Operator::Delete) && matches!(target, Target::Linewise { .. }) && app.views[i].comments.selected.is_some() {
        delete_selected_comment(app, i);
        return;
    }
    if !prepare_target_selection(app, i, state, target) {
        return;
    }
    let doc_id = app.views[i].id;
    if let Some(vis) = state.visual.remove(&doc_id) {
        state.last_visual.insert(doc_id, vis);
    }
    execute_operator_on_selection(app, ctx, i, op);
}

fn delete_selected_comment(app: &mut PdfCraftApp, view_idx: usize) {
    if let Some((page, annot_idx)) = app.views[view_idx].comments.selected.take() {
        let _ = app.apply_edit(Edit::DeleteAnnotation { page, index: annot_idx });
    }
}

fn prepare_target_selection(app: &mut PdfCraftApp, view_idx: usize, state: &mut PdfModalState, target: Target) -> bool {
    let view = &mut app.views[view_idx];
    match target {
        Target::Selection => view.selected_text().is_some_and(|t| !t.is_empty()),
        Target::Linewise { count } => {
            let Some(caret) = ensure_caret(view, state) else { return false };
            let Some(text) = view.texts.get(&caret.page).cloned() else { return false };
            let Some((start, mut end)) = text.line_at(caret.glyph) else { return false };
            for _ in 1..count.max(1) {
                if let Some((_, next_end)) = text.line_at(end + 1) {
                    end = next_end;
                }
            }
            view.select_text(caret.page, start, end);
            true
        }
        Target::TextObject { obj, inner, .. } => {
            let Some(caret) = ensure_caret(view, state) else { return false };
            let Some((start, end)) = resolve_text_object_bounds(view, caret, obj, inner) else { return false };
            view.select_text(caret.page, start, end);
            true
        }
        Target::Motion { motion, count, .. } => {
            let Some(start) = ensure_caret(view, state) else { return false };
            let end = step_caret_motion(view, start, motion, count.max(1));
            if end.page != start.page {
                return false;
            }
            view.select_text(start.page, start.glyph, end.glyph);
            true
        }
    }
}

fn execute_operator_on_selection(app: &mut PdfCraftApp, ctx: &Context, view_idx: usize, op: Operator) {
    match op {
        Operator::Yank => {
            let _ = app.copy_selected_text(view_idx, ctx);
            app.views[view_idx].clear_selection();
        }
        Operator::Highlight => apply_markup_to_selection(app, view_idx, CommentTool::Highlight),
        Operator::Underline => apply_markup_to_selection(app, view_idx, CommentTool::Underline),
        Operator::StrikeOut => apply_markup_to_selection(app, view_idx, CommentTool::StrikeOut),
        Operator::Squiggly => apply_markup_to_selection(app, view_idx, CommentTool::Squiggly),
        Operator::Note => open_note_on_selection(app, view_idx),
        Operator::Delete | Operator::Change => {
            let _ = app.copy_selected_text(view_idx, ctx);
            app.views[view_idx].clear_selection();
            if op == Operator::Change {
                let _ = app.execute("comment.replace");
            }
        }
    }
}

fn apply_markup_to_selection(app: &mut PdfCraftApp, view_idx: usize, tool: CommentTool) {
    let Some(kind) = tool.markup() else { return };
    let doc_id = app.views[view_idx].id;
    let Some(doc) = app.session.get(doc_id) else { return };
    if !doc.allows_annotation() {
        app.notify("The document's security settings don't allow adding annotations");
        return;
    }
    let marks = app.views[view_idx].selection_quads(&doc.info);
    if marks.is_empty() {
        return;
    }
    let head = app.views[view_idx].selection_head_page().or_else(|| marks.last().map(|(p, _)| *p)).unwrap_or(0);
    app.views[view_idx].clear_selection();
    let author = app.comment_prefs.author.clone();
    let edits = marks
        .into_iter()
        .map(|(page, quads)| {
            Edit::AddAnnotation(NewAnnotation {
                page,
                shape: Shape::TextMarkup { kind, quads },
                style: app.comment_prefs.style(tool),
                contents: String::new(),
                author: author.clone(),
            })
        })
        .collect();
    if let Some(edit) = app.views[view_idx].selection_marks_edit(edits, head) {
        let prev_tool = app.quick_tool;
        let _ = app.apply_edit(edit);
        if prev_tool == QuickTool::Select {
            app.quick_tool = QuickTool::Select;
        }
    }
}

fn open_note_on_selection(app: &mut PdfCraftApp, view_idx: usize) {
    let doc_id = app.views[view_idx].id;
    let Some(doc) = app.session.get(doc_id) else { return };
    if !doc.allows_annotation() {
        app.notify("The document's security settings don't allow adding annotations");
        return;
    }
    let Ok([(page, quads)]) = <[_; 1]>::try_from(app.views[view_idx].selection_quads(&doc.info)) else { return };
    let last = quads.last().copied().unwrap_or_default();
    let at = [last[2].max(last[6]), last[1].max(last[3])];
    app.views[view_idx].clear_selection();
    app.views[view_idx].comments.composer = Some(Composer { page, at, kind: ComposerKind::TextNote { quads }, text: String::new(), focus: true });
}

fn handle_paste(app: &mut PdfCraftApp) {
    let Some(i) = app.active else { return };
    if app.paste_comments(i, None, None) {
        return;
    }
    let _ = app.execute("comment.paste");
}

fn handle_search_word(app: &mut PdfCraftApp, state: &mut PdfModalState, forward: bool) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    let Some(caret) = ensure_caret(view, state) else { return };
    let Some(text) = view.texts.get(&caret.page).cloned() else { return };
    let Some((w0, w1)) = text.word_at(caret.glyph) else { return };
    let word = text.text_of(w0..w1 + 1);
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return;
    }
    let smart_case = state.engine.options.smart_case && trimmed.chars().any(char::is_uppercase);
    let case_sensitive = !state.engine.options.ignore_case || smart_case;
    let f = view.find.get_or_insert_with(Default::default);
    f.query = trimmed.to_string();
    f.case_sensitive = case_sensitive;
    f.focus = false;
    view.rerun_find();
    view.find_step(forward);
    sync_caret_to_current_match(view, state);
}

fn handle_search_step(app: &mut PdfCraftApp, state: &mut PdfModalState, forward: bool, count: usize) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    for _ in 0..count.max(1) {
        view.find_step(forward);
    }
    sync_caret_to_current_match(view, state);
}

fn sync_caret_to_current_match(view: &DocView, state: &mut PdfModalState) {
    if let Some(f) = view.find.as_ref()
        && let Some(cur) = f.current
        && let Some((page, range)) = f.matches.get(cur)
    {
        state.carets.insert(view.id, PdfCaret { page: *page, glyph: range.start, preferred_x: None });
    }
}

fn handle_dismiss(app: &mut PdfCraftApp, ctx: &Context, state: &mut PdfModalState) {
    if app.full_screen {
        app.set_full_screen(ctx, false);
        return;
    }
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    if let Some(vis) = state.visual.remove(&view.id) {
        state.last_visual.insert(view.id, vis);
    }
    if view.selected_text().is_some() || view.all_text {
        view.clear_selection();
        view.all_text = false;
    } else if view.find.is_some() {
        view.find = None;
    } else if app.quick_tool != QuickTool::Select {
        app.quick_tool = QuickTool::Select;
    }
}

fn handle_zoom(app: &mut PdfCraftApp, op: ZoomOp, count: usize) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    match op {
        ZoomOp::In => {
            for _ in 0..count.max(1) {
                view.zoom_step(true);
            }
        }
        ZoomOp::Out => {
            for _ in 0..count.max(1) {
                view.zoom_step(false);
            }
        }
        ZoomOp::Reset => view.set_zoom(1.0),
        ZoomOp::FitWidth => view.set_fit(Fit::Width),
        ZoomOp::FitPage => view.set_fit(Fit::Page),
    }
}

fn handle_jump_history(app: &mut PdfCraftApp, forward: bool, count: usize) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    for _ in 0..count.max(1) {
        if !view.view_history(forward) {
            break;
        }
    }
}

fn handle_step_page(app: &mut PdfCraftApp, state: &mut PdfModalState, forward: bool, count: usize) {
    let Some(i) = app.active else { return };
    let view = &mut app.views[i];
    for _ in 0..count.max(1) {
        view.step_page(forward);
    }
    let page = view.current;
    if view.texts.get(&page).is_some_and(|t| !t.glyphs.is_empty()) {
        state.carets.insert(view.id, PdfCaret { page, glyph: 0, preferred_x: None });
    }
}
