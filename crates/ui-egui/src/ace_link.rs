//! Ace-link mode (`ace.open_link`): overlay single-character hint badges over all visible links
//! in the app (document link annotations, plain-text URLs, and visible UI links) and open the
//! matching link on a single key press.

use egui::{Color32, Context, CornerRadius, Event, FontId, Id, Key, LayerId, Modifiers, Order, Rect, Stroke, StrokeKind, pos2, vec2};
use pdfcraft_engine::text_links::TextLink;
use pdfcraft_render::{DocInfo, Link, LinkTarget};

use crate::LinkOrigin;
use crate::PdfCraftApp;
use crate::canvas::PageXform;

/// Single-character hint keys assigned in order (home row first, then remaining letters, digits,
/// and uppercase letters so up to 62 visible links have unique single-character overlays).
pub const HINT_CHARS: &[char] = &[
    'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p', 'z', 'x', 'c', 'v', 'b', 'n', 'm', '1', '2', '3',
    '4', '5', '6', '7', '8', '9', '0', 'A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L', 'Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P', 'Z', 'X', 'C',
    'V', 'B', 'N', 'M',
];

/// What activating an ace-link hint does.
#[derive(Clone, Debug, PartialEq)]
pub enum AceLinkTarget {
    /// An app UI link (Help/Community/Credits), opened directly via [`PdfCraftApp::open_url`].
    UiUrl(String),
    /// A web or email address found in plain page text, validated via [`PdfCraftApp::request_document_url`].
    DocumentUri(String),
    /// A PDF link annotation on a document page.
    DocumentLink { view_index: usize, page: usize, link_index: usize, target: LinkTarget },
}

/// One visible link with its assigned single-character hint and screen rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct AceLinkCandidate {
    pub hint: char,
    pub rect: Rect,
    pub target: AceLinkTarget,
}

/// State for `ace.open_link` single-char link hint mode.
#[derive(Clone, Debug, Default)]
pub struct AceLinkState {
    pub active: bool,
    pub candidates: Vec<AceLinkCandidate>,
}

impl AceLinkState {
    pub fn cancel(&mut self) {
        self.active = false;
        self.candidates.clear();
    }

    pub fn find_by_char(&self, ch: char) -> Option<&AceLinkCandidate> {
        if let Some(exact) = self.candidates.iter().find(|c| c.hint == ch) {
            return Some(exact);
        }
        let has_upper = self.candidates.iter().any(|c| c.hint.is_ascii_uppercase());
        if !has_upper {
            let lower = ch.to_ascii_lowercase();
            return self.candidates.iter().find(|c| c.hint == lower);
        }
        None
    }
}

#[derive(Clone, Debug, Default)]
struct CollectedUiLinks(Vec<(Rect, String)>);

fn ui_links_id() -> Id {
    Id::unique("pdfcraft.ace_ui_links")
}

/// Clear the per-frame UI link list at the start of UI drawing (or before a modal dialog covers
/// background UI).
pub(crate) fn begin_frame(ctx: &Context) {
    ctx.data_mut(|d| {
        d.insert_temp(ui_links_id(), CollectedUiLinks::default());
    });
}

/// Record a visible UI link widget's screen rect and URL for the current frame.
pub(crate) fn record_ui_url(ui: &egui::Ui, resp: &egui::Response, url: &str) {
    if !ui.is_rect_visible(resp.rect) {
        return;
    }
    let clipped = resp.rect.intersect(ui.clip_rect());
    if !clipped.is_positive() {
        return;
    }
    let url = url.to_string();
    ui.ctx().data_mut(|d| {
        let entry = d.get_temp_mut_or_default::<CollectedUiLinks>(ui_links_id());
        entry.0.push((clipped, url));
    });
}

impl PdfCraftApp {
    /// Toggle ace-link overlay mode (`ace.open_link`).
    pub(crate) fn toggle_ace_link(&mut self) {
        if self.ace_link.active {
            self.ace_link.cancel();
        } else {
            self.ace_link.active = true;
            self.ace_link.candidates.clear();
        }
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }

    /// Consume a single-char hint press (or Escape / pointer click to cancel) while ace-link mode
    /// is active, before modal/shortcut handlers see the key.
    pub(crate) fn ace_link_keys(&mut self, ctx: &Context) {
        if !self.ace_link.active {
            return;
        }
        if self.close_request.is_some() || self.palette_open || craft_menu::bar::is_open(ctx) {
            self.ace_link.cancel();
            return;
        }
        if ctx.input(|i| i.pointer.any_pressed()) {
            self.ace_link.cancel();
            return;
        }

        enum Outcome {
            None,
            Cancel,
            Open(AceLinkTarget),
        }

        let mut outcome = Outcome::None;
        ctx.input_mut(|input| {
            let old_events = std::mem::take(&mut input.events);
            let mut kept = Vec::with_capacity(old_events.len());
            let mut idx = 0;
            while idx < old_events.len() {
                if !matches!(outcome, Outcome::None) {
                    kept.push(old_events[idx].clone());
                    idx += 1;
                    continue;
                }
                match &old_events[idx] {
                    Event::Key { key: Key::Escape, pressed: true, .. } => {
                        outcome = Outcome::Cancel;
                        idx += 1;
                        continue;
                    }
                    Event::Key { key, pressed: true, modifiers, .. } if !has_command_mod(*modifiers) => {
                        let paired_text = old_events.get(idx + 1).and_then(|next| match next {
                            Event::Text(s) => single_printable_char(s),
                            _ => None,
                        });
                        if let Some(ch) = paired_text.or_else(|| egui_key_char(*key, modifiers.shift)) {
                            outcome = match self.ace_link.find_by_char(ch) {
                                Some(cand) => Outcome::Open(cand.target.clone()),
                                None => Outcome::Cancel,
                            };
                            idx += if paired_text.is_some() { 2 } else { 1 };
                            continue;
                        }
                    }
                    Event::Text(s) => {
                        if let Some(ch) = single_printable_char(s) {
                            outcome = match self.ace_link.find_by_char(ch) {
                                Some(cand) => Outcome::Open(cand.target.clone()),
                                None => Outcome::Cancel,
                            };
                            idx += 1;
                            continue;
                        }
                    }
                    _ => {}
                }
                kept.push(old_events[idx].clone());
                idx += 1;
            }
            input.events = kept;
        });

        match outcome {
            Outcome::None => {}
            Outcome::Cancel => self.ace_link.cancel(),
            Outcome::Open(target) => {
                self.ace_link.cancel();
                self.open_ace_target(target);
            }
        }
    }

    /// Collect all visible links for this frame and paint single-character hint overlays over them
    /// while ace-link mode is active.
    pub(crate) fn ace_link_frame(&mut self, ctx: &Context) {
        if !self.ace_link.active {
            self.ace_link.candidates.clear();
            return;
        }
        if self.palette_open || self.close_request.is_some() {
            self.ace_link.cancel();
            return;
        }

        let mut items: Vec<(Rect, AceLinkTarget)> = Vec::new();
        let ui_links = ctx.data(|d| d.get_temp::<CollectedUiLinks>(ui_links_id())).unwrap_or_default();
        for (rect, url) in ui_links.0 {
            items.push((rect, AceLinkTarget::UiUrl(url)));
        }

        let all_tools_covering = !(self.full_screen && self.active.is_some())
            && self.mode == crate::Mode::AllTools
            && self.all_tools_full
            && !self.hidden_interface().window_ui;
        if !self.covers_pages() && !self.ocr_verify.open && !all_tools_covering {
            for view_index in self.views_on_screen() {
                let Some(view) = self.views.get(view_index) else { continue };
                if view.organize {
                    continue;
                }
                let Some(doc) = self.session.get(view.id) else { continue };
                let viewport = view.viewport_rect();
                if !viewport.is_positive() {
                    continue;
                }
                for (page, xf) in view.page_xforms() {
                    let page_clip = viewport.intersect(xf.rect);
                    if !page_clip.is_positive() {
                        continue;
                    }
                    for link in doc.info.links.iter().filter(|l| l.page == page) {
                        if let Some(rect) = document_link_screen_rect(&xf, &doc.info, page, link, page_clip) {
                            items.push((rect, AceLinkTarget::DocumentLink { view_index, page, link_index: link.index, target: link.target.clone() }));
                        }
                    }
                    if self.text_links {
                        for tl in view.visible_text_links(&doc.info, page) {
                            if let Some(rect) = text_link_screen_rect(&xf, tl, page_clip) {
                                items.push((rect, AceLinkTarget::DocumentUri(tl.uri.clone())));
                            }
                        }
                    }
                }
            }
        }

        // Reading order: top-to-bottom (bucketed slightly so items on the same row sort left-to-right).
        items.sort_by(|(ra, _), (rb, _)| {
            let row_a = (ra.top() * 0.25).round() as i32;
            let row_b = (rb.top() * 0.25).round() as i32;
            row_a.cmp(&row_b).then_with(|| ra.left().total_cmp(&rb.left())).then_with(|| ra.top().total_cmp(&rb.top()))
        });

        self.ace_link.candidates =
            items.into_iter().zip(HINT_CHARS.iter().copied()).map(|((rect, target), hint)| AceLinkCandidate { hint, rect, target }).collect();

        if self.ace_link.candidates.is_empty() {
            self.ace_link.cancel();
            self.notify_tr("No visible links");
            return;
        }

        paint_overlays(ctx, &self.ace_link.candidates);
    }

    fn open_ace_target(&mut self, target: AceLinkTarget) {
        match target {
            AceLinkTarget::UiUrl(url) => self.open_url(&url),
            AceLinkTarget::DocumentUri(uri) => self.request_document_url(&uri, LinkOrigin::Link),
            AceLinkTarget::DocumentLink { view_index, page, link_index, target } => {
                if matches!(target, LinkTarget::JavaScript(_)) {
                    if let Some(doc_id) = self.views.get(view_index).map(|v| v.id) {
                        self.run_link_script(doc_id, page, link_index);
                    }
                } else if view_index < self.views.len()
                    && let Some((source, file, destination)) = crate::canvas::follow_link(self, view_index, Some(target))
                {
                    self.open_remote_link(source, file, destination);
                }
            }
        }
    }
}

fn document_link_screen_rect(xf: &PageXform, info: &DocInfo, page: usize, link: &Link, page_clip: Rect) -> Option<Rect> {
    if !link.quads.is_empty() {
        for q in &link.quads {
            let min_x = q[0].min(q[2]).min(q[4]).min(q[6]);
            let min_y = q[1].min(q[3]).min(q[5]).min(q[7]);
            let max_x = q[0].max(q[2]).max(q[4]).max(q[6]);
            let max_y = q[1].max(q[3]).max(q[5]).max(q[7]);
            let r = xf.user_rect(info, page, [min_x, min_y, max_x, max_y]);
            if r.is_finite() && page_clip.intersects(r) {
                return Some(r.intersect(page_clip));
            }
        }
        return None;
    }
    let r = xf.user_rect(info, page, link.rect);
    (r.is_finite() && page_clip.intersects(r)).then(|| r.intersect(page_clip))
}

fn text_link_screen_rect(xf: &PageXform, link: &TextLink, page_clip: Rect) -> Option<Rect> {
    for &rect in &link.rects {
        let r = xf.view_rect(rect);
        if r.is_finite() && page_clip.intersects(r) {
            return Some(r.intersect(page_clip));
        }
    }
    None
}

fn paint_overlays(ctx: &Context, candidates: &[AceLinkCandidate]) {
    let screen = ctx.content_rect();
    if !screen.is_positive() {
        return;
    }
    let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::unique("pdfcraft.ace_link_overlay")));
    let bg = Color32::from_rgb(253, 224, 71);
    let fg = Color32::from_rgb(17, 24, 39);
    let badge_stroke = Stroke::new(1.0, Color32::from_rgb(161, 98, 7));
    let link_stroke = Stroke::new(1.5, Color32::from_rgb(217, 119, 6));
    let link_fill = Color32::from_rgba_unmultiplied(253, 224, 71, 32);
    let font = FontId::monospace(12.0);

    for cand in candidates {
        painter.rect(cand.rect.expand(1.0), CornerRadius::same(3), link_fill, link_stroke, StrokeKind::Outside);
        let mut label_buf = [0u8; 4];
        let label = cand.hint.encode_utf8(&mut label_buf);
        let galley = ctx.fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font.clone(), fg));
        let pad = vec2(5.0, 2.0);
        let size = vec2((galley.size().x + pad.x * 2.0).max(18.0), (galley.size().y + pad.y * 2.0).max(18.0));
        let max_x = (screen.right() - size.x - 2.0).max(screen.left() + 2.0);
        let max_y = (screen.bottom() - size.y - 2.0).max(screen.top() + 2.0);
        let top_left = pos2(cand.rect.left().clamp(screen.left() + 2.0, max_x), cand.rect.top().clamp(screen.top() + 2.0, max_y));
        let badge_rect = Rect::from_min_size(top_left, size);
        painter.rect(badge_rect, CornerRadius::same(4), bg, badge_stroke, StrokeKind::Inside);
        painter.galley(badge_rect.center() - galley.size() * 0.5, galley, fg);
    }
}

fn has_command_mod(m: Modifiers) -> bool {
    m.command || m.ctrl || m.mac_cmd || m.alt
}

fn single_printable_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || ch.is_control() {
        return None;
    }
    Some(ch)
}

fn egui_key_char(key: Key, shifted: bool) -> Option<char> {
    let base = match key {
        Key::A => 'a',
        Key::B => 'b',
        Key::C => 'c',
        Key::D => 'd',
        Key::E => 'e',
        Key::F => 'f',
        Key::G => 'g',
        Key::H => 'h',
        Key::I => 'i',
        Key::J => 'j',
        Key::K => 'k',
        Key::L => 'l',
        Key::M => 'm',
        Key::N => 'n',
        Key::O => 'o',
        Key::P => 'p',
        Key::Q => 'q',
        Key::R => 'r',
        Key::S => 's',
        Key::T => 't',
        Key::U => 'u',
        Key::V => 'v',
        Key::W => 'w',
        Key::X => 'x',
        Key::Y => 'y',
        Key::Z => 'z',
        Key::Num0 => return Some('0'),
        Key::Num1 => return Some('1'),
        Key::Num2 => return Some('2'),
        Key::Num3 => return Some('3'),
        Key::Num4 => return Some('4'),
        Key::Num5 => return Some('5'),
        Key::Num6 => return Some('6'),
        Key::Num7 => return Some('7'),
        Key::Num8 => return Some('8'),
        Key::Num9 => return Some('9'),
        _ => return None,
    };
    Some(if shifted { base.to_ascii_uppercase() } else { base })
}
