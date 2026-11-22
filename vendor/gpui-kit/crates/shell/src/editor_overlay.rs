use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

use gpui::{
    Action, AnyElement, App, Context, Entity, Hsla, InteractiveElement as _, IntoElement as _,
    MouseButton, ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window,
    anchored, deferred, div, point, prelude::FluentBuilder as _, px,
};
use gpui_base::{
    TextView,
    input::{DiagnosticSeverity, EditorState, Enter, Escape, InputOverlayKind, MoveDown, MoveUp},
};
use lsp_types::{CompletionItem, Hover, HoverContents, MarkedString};

use crate::entities::EntityHandle;

const VISIBLE_COMPLETIONS: usize = 10;
const MAX_COMPLETIONS: usize = 200;
const EDGE: gpui::Pixels = px(8.);
const HOVER_ROOM: gpui::Pixels = px(160.);
const HOVER_HEIGHT: gpui::Pixels = px(360.);

#[derive(Default)]
pub(crate) struct EditorOverlay {
    items: RefCell<Vec<CompletionItem>>,
    shown: RefCell<Option<(u64, String)>>,
    selected: Cell<usize>,
    top: Cell<usize>,
}

impl EditorOverlay {
    pub(crate) fn install(self: &Rc<Self>, state: &Entity<EditorState>, cx: &mut App) {
        let overlay = self.clone();
        state.update(cx, |state, _| {
            state.set_overlay_action_handler(move |kind, action, window, cx| {
                overlay.handle(kind, action, window, cx)
            });
        });
    }

    fn handle(
        &self,
        kind: InputOverlayKind,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) -> bool {
        let count = self.items.borrow().len();
        if kind != InputOverlayKind::Completion || count == 0 {
            return false;
        }
        if action.as_any().is::<MoveUp>() {
            self.select((self.selected.get() + count - 1) % count);
            cx.notify();
            return true;
        }
        if action.as_any().is::<MoveDown>() {
            self.select((self.selected.get() + 1) % count);
            cx.notify();
            return true;
        }
        if action.partial_eq(&Escape) {
            return true;
        }
        if Enter::is_primary(&*action) {
            let item = self.items.borrow()[self.selected.get()].clone();
            cx.defer_in(window, move |state, window, cx| {
                insert(state, &item, window, cx);
            });
            return true;
        }
        false
    }

    fn select(&self, index: usize) {
        self.selected.set(index);
        if index < self.top.get() {
            self.top.set(index);
        } else if index >= self.top.get() + VISIBLE_COMPLETIONS {
            self.top.set(index + 1 - VISIBLE_COMPLETIONS);
        }
    }

    fn refresh(&self, revision: u64, query: &str, items: &[CompletionItem]) {
        let current = Some((revision, query.to_owned()));
        if *self.shown.borrow() == current {
            return;
        }
        *self.items.borrow_mut() = filtered(items, query);
        *self.shown.borrow_mut() = current;
        self.selected.set(0);
        self.top.set(0);
    }

    fn clear(&self) {
        self.items.borrow_mut().clear();
        self.shown.borrow_mut().take();
    }
}

fn insert(
    state: &mut EditorState,
    item: &CompletionItem,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let end = state.cursor();
    let start = state
        .completion_menu_state()
        .trigger_start_offset
        .unwrap_or(end)
        .min(end);
    state.insert_completion(item, start..end, window, cx);
    state.dismiss_completion_overlay(cx);
}

fn filtered(items: &[CompletionItem], query: &str) -> Vec<CompletionItem> {
    let query = query.to_lowercase();
    let mut ranked: Vec<(u8, &CompletionItem)> = items
        .iter()
        .filter_map(|item| {
            let text = item
                .filter_text
                .as_deref()
                .unwrap_or(&item.label)
                .to_lowercase();
            if text.starts_with(&query) {
                Some((0, item))
            } else if is_subsequence(&query, &text) {
                Some((1, item))
            } else {
                None
            }
        })
        .collect();
    ranked.sort_by(|(a_rank, a), (b_rank, b)| {
        a_rank
            .cmp(b_rank)
            .then_with(|| sort_key(a).cmp(sort_key(b)))
    });
    ranked
        .into_iter()
        .take(MAX_COMPLETIONS)
        .map(|(_, item)| item.clone())
        .collect()
}

fn sort_key(item: &CompletionItem) -> &str {
    item.sort_text.as_deref().unwrap_or(&item.label)
}

fn is_subsequence(query: &str, text: &str) -> bool {
    let mut rest = text.chars();
    query.chars().all(|wanted| rest.any(|ch| ch == wanted))
}

fn markdown_of(hover: &Hover) -> String {
    let marked = |marked: &MarkedString| match marked {
        MarkedString::String(text) => text.clone(),
        MarkedString::LanguageString(code) => format!("```{}\n{}\n```", code.language, code.value),
    };
    match &hover.contents {
        HoverContents::Scalar(scalar) => marked(scalar),
        HoverContents::Array(parts) => parts.iter().map(marked).collect::<Vec<_>>().join("\n\n"),
        HoverContents::Markup(markup) => markup.value.clone(),
    }
}

fn severity_color(severity: DiagnosticSeverity, error: Hsla) -> Hsla {
    match severity {
        DiagnosticSeverity::Error => error,
        DiagnosticSeverity::Warning => gpui::hsla(38. / 360., 0.92, 0.5, 1.),
        DiagnosticSeverity::Info => gpui::hsla(212. / 360., 0.8, 0.55, 1.),
        DiagnosticSeverity::Hint => gpui::hsla(0., 0., 0.55, 1.),
    }
}

pub(crate) fn overlays(
    handle: EntityHandle,
    state: &Entity<EditorState>,
    overlay: &Rc<EditorOverlay>,
    cx: &App,
) -> Vec<AnyElement> {
    let mut elements = Vec::new();
    if let Some(menu) = completion_menu(handle, state, overlay, cx) {
        elements.push(menu);
    }
    if let Some(popover) = hover_popover(handle, state, cx) {
        elements.push(popover);
    }
    elements
}

fn surface(cx: &App) -> gpui::Div {
    let colors = &gpui_base::Theme::global(cx).tokens.colors;
    div()
        .occlude()
        .bg(colors.surface)
        .text_color(colors.surface_foreground)
        .border_1()
        .border_color(colors.border)
        .rounded(px(6.))
}

fn completion_menu(
    handle: EntityHandle,
    state: &Entity<EditorState>,
    overlay: &Rc<EditorOverlay>,
    cx: &App,
) -> Option<AnyElement> {
    let editor = state.read(cx);
    let menu = editor.completion_menu_state();
    if !menu.open {
        overlay.clear();
        return None;
    }
    overlay.refresh(menu.revision(), &menu.query, &menu.items);
    let items = overlay.items.borrow();
    if items.is_empty() {
        return None;
    }
    let cursor = editor.cursor();
    let caret = editor.range_to_bounds(&(cursor..cursor))?;
    let colors = gpui_base::Theme::global(cx).tokens.colors;
    let top = overlay.top.get().min(items.len().saturating_sub(1));
    let rows = items
        .iter()
        .enumerate()
        .skip(top)
        .take(VISIBLE_COMPLETIONS)
        .map(|(index, item)| {
            let chosen = item.clone();
            let target = state.clone();
            let selected = index == overlay.selected.get();
            div()
                .id(("gpui-shell-editor-completion", index))
                .flex()
                .gap(px(12.))
                .px(px(8.))
                .py(px(2.))
                .rounded(px(4.))
                .when(selected, |row| row.bg(colors.selection))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    target.update(cx, |state, cx| insert(state, &chosen, window, cx));
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(item.label.clone()),
                )
                .when_some(item.detail.clone(), |row, detail| {
                    row.child(
                        div()
                            .max_w(px(220.))
                            .truncate()
                            .text_color(colors.muted_foreground)
                            .child(detail),
                    )
                })
                .into_any_element()
        })
        .collect::<Vec<_>>();
    let menu = surface(cx)
        .id(("gpui-shell-editor-completions", handle))
        .debug_selector(|| "editor-completions".into())
        .w(px(420.))
        .p(px(4.))
        .flex()
        .flex_col()
        .children(rows);
    Some(
        deferred(
            anchored()
                .position(point(caret.left(), caret.bottom()))
                .snap_to_window_with_margin(px(8.))
                .child(menu),
        )
        .with_priority(1)
        .into_any_element(),
    )
}

fn hover_popover(
    handle: EntityHandle,
    state: &Entity<EditorState>,
    cx: &App,
) -> Option<AnyElement> {
    let editor = state.read(cx);
    if editor.completion_menu_state().open {
        return None;
    }
    let hover = editor.hover_popover().cloned();
    let diagnostic = editor.diagnostic_popover();
    let range: Range<usize> = hover
        .as_ref()
        .map(|hover| hover.symbol_range.clone())
        .or_else(|| diagnostic.as_ref().map(|entry| entry.range.clone()))?;
    let bounds = editor.range_to_bounds(&range)?;
    let room_above = bounds.top() - EDGE;
    let above = room_above >= HOVER_ROOM;
    let colors = gpui_base::Theme::global(cx).tokens.colors;
    let content = surface(cx)
        .id(("gpui-shell-editor-hover", handle))
        .debug_selector(|| "editor-hover".into())
        .max_w(px(560.))
        .max_h(if above {
            room_above.min(HOVER_HEIGHT)
        } else {
            HOVER_HEIGHT
        })
        .overflow_y_scroll()
        .p(px(8.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .when_some(diagnostic, |popover, entry| {
            popover.child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(3.))
                            .flex_none()
                            .rounded(px(2.))
                            .bg(severity_color(entry.severity, colors.destructive)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(entry.message.clone()),
                    )
                    .when_some(entry.source.clone(), |row, source| {
                        row.child(
                            div()
                                .flex_none()
                                .text_color(colors.muted_foreground)
                                .child(source),
                        )
                    }),
            )
        })
        .when_some(hover, |popover, hover| {
            popover.child(TextView::markdown(
                ("gpui-shell-editor-hover-text", handle),
                markdown_of(&hover.hover),
            ))
        });
    Some(
        deferred(
            anchored()
                .position(if above {
                    bounds.origin
                } else {
                    point(bounds.left(), bounds.bottom())
                })
                .anchor(if above {
                    gpui::Anchor::BottomLeft
                } else {
                    gpui::Anchor::TopLeft
                })
                .snap_to_window_with_margin(EDGE)
                .child(
                    div()
                        .when(above, |gap| gap.pb(px(4.)))
                        .when(!above, |gap| gap.pt(px(4.)))
                        .child(content),
                ),
        )
        .with_priority(1)
        .into_any_element(),
    )
}
