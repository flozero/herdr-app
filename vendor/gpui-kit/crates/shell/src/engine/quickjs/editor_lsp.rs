use std::{
    cell::RefCell,
    ops::Range,
    rc::{Rc, Weak},
};

use anyhow::{Result, anyhow};
use gpui::{App, Entity, Subscription, Task, WeakEntity, Window};
use gpui_base::input::{
    CompletionProvider, DefinitionProvider, EditorState, HoverProvider, Rope, RopeExt,
};
use lsp_types::{
    CompletionContext, CompletionResponse, CompletionTriggerKind, GotoDefinitionResponse, Hover,
    Location, LocationLink, ShowDocumentParams, TextEdit,
};
use rquickjs::{Function, Persistent, Value};

use crate::scope::{self, ScopePhase};

use super::{ContextBinding, InputCallbackOwner, ShellRuntime, context_object, scheduler};

type Answer = async_channel::Sender<Result<serde_json::Value, String>>;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct LspOptions {
    uri: Option<String>,
    trigger_characters: Vec<String>,
    completion: bool,
    hover: bool,
    definition: bool,
    show_document: bool,
    hover_trigger: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HoverTrigger {
    Pointer,
    Selection,
}

impl LspOptions {
    pub(super) fn hover_trigger(&self) -> Result<HoverTrigger> {
        match self.hover_trigger.as_deref() {
            None | Some("pointer") => Ok(HoverTrigger::Pointer),
            Some("selection") => Ok(HoverTrigger::Selection),
            Some(other) => Err(anyhow!(
                "unknown hover_trigger `{other}`; expected \"pointer\" or \"selection\""
            )),
        }
    }
}

const SELECTED_SYMBOL_LIMIT: usize = 200;

fn empty_hover() -> Hover {
    Hover {
        contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: String::new(),
        }),
        range: None,
    }
}

fn hover_on_selection(
    state: &Entity<EditorState>,
    lsp: Rc<ScriptLsp>,
    window: &mut Window,
    cx: &mut App,
) -> Subscription {
    let last: RefCell<Option<Range<usize>>> = RefCell::new(None);
    let pending = RefCell::new(Task::ready(()));
    window.observe(state, cx, move |state, window, cx| {
        let (range, text, shown) = {
            let editor = state.read(cx);
            (
                editor.selected_range(),
                editor.text().clone(),
                editor.hover_popover().is_some(),
            )
        };
        if last.borrow().as_ref() == Some(&range) {
            return;
        }
        *last.borrow_mut() = Some(range.clone());
        *pending.borrow_mut() = Task::ready(());
        if shown {
            state.update(cx, |state, cx| state.clear_hover_state(cx));
        }
        let symbol = !range.is_empty()
            && range.len() <= SELECTED_SYMBOL_LIMIT
            && text.offset_to_position(range.start).line == text.offset_to_position(range.end).line;
        if !symbol {
            return;
        }
        let flagged = state
            .read(cx)
            .diagnostics()
            .is_some_and(|set| set.range(range.clone()).next().is_some());
        let answer = lsp.hover(&text, range.start, window, cx);
        let target = state.downgrade();
        *pending.borrow_mut() = cx.spawn(async move |cx| {
            let hover = answer
                .await
                .ok()
                .flatten()
                .or_else(|| flagged.then(empty_hover));
            if let Some(hover) = hover {
                let _ = target.update(cx, |state, cx| {
                    if state.selected_range() == range {
                        state.present_hover(range, hover, cx);
                    }
                });
            }
        });
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HighlightSpec {
    range: lsp_types::Range,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    style: Option<String>,
}

pub(super) fn highlights(
    state: &Entity<EditorState>,
    specs: Vec<HighlightSpec>,
    cx: &App,
) -> Result<Vec<gpui_base::input::RangeDecoration>> {
    use gpui_base::input::{RangeDecoration, RangeDecorationStyle};
    let text = state.read(cx).text().clone();
    specs
        .into_iter()
        .map(|spec| {
            let start = text.position_to_offset(&spec.range.start);
            let end = text.position_to_offset(&spec.range.end);
            let style = match spec.style.as_deref() {
                None | Some("fill") => RangeDecorationStyle::Fill,
                Some("frame") => RangeDecorationStyle::Frame,
                Some("gutter") => RangeDecorationStyle::Gutter,
                Some(other) => {
                    return Err(anyhow!(
                        "unknown style `{other}`; expected \"fill\", \"frame\" or \"gutter\""
                    ));
                }
            };
            let decoration = RangeDecoration::new(start..end).with_style(style);
            Ok(match spec.color {
                Some(color) => decoration.with_color(
                    crate::value::Bridged::Str(color)
                        .as_color()
                        .map_err(|error| anyhow!(error.to_string()))?,
                ),
                None => decoration,
            })
        })
        .collect()
}

pub(super) struct ScriptLsp {
    this: Weak<ScriptLsp>,
    runtime: Weak<ShellRuntime>,
    dispatch: Weak<Persistent<Function<'static>>>,
    owner: InputCallbackOwner,
    trigger_characters: Vec<String>,
}

impl ScriptLsp {
    pub(super) fn install(
        state: &Entity<EditorState>,
        options: LspOptions,
        runtime: Weak<ShellRuntime>,
        dispatch: Weak<Persistent<Function<'static>>>,
        trigger: HoverTrigger,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Subscription> {
        let lsp = Rc::new_cyclic(|this| Self {
            this: this.clone(),
            runtime,
            dispatch,
            owner: InputCallbackOwner::current(),
            trigger_characters: options.trigger_characters,
        });
        let show_document = options.show_document.then(|| {
            let lsp = lsp.clone();
            let own_uri = options.uri;
            let handler: gpui_base::input::ShowDocumentHandler =
                Rc::new(move |params: &ShowDocumentParams, window, cx| {
                    if params.external == Some(true)
                        || own_uri.as_deref() == Some(params.uri.as_str())
                    {
                        return false;
                    }
                    let request = serde_json::to_value(params).unwrap_or_default();
                    lsp.send("show_document", request, None, window, cx);
                    true
                });
            handler
        });
        state.update(cx, |state, cx| {
            let providers = state.lsp_mut();
            providers.completion_provider = options
                .completion
                .then(|| lsp.clone() as Rc<dyn CompletionProvider>);
            providers.hover_provider = (options.hover && trigger == HoverTrigger::Pointer)
                .then(|| lsp.clone() as Rc<dyn HoverProvider>);
            providers.definition_provider = options
                .definition
                .then(|| lsp.clone() as Rc<dyn DefinitionProvider>);
            providers.show_document = show_document;
            cx.notify();
        });
        (options.hover && trigger == HoverTrigger::Selection)
            .then(|| hover_on_selection(state, lsp, window, cx))
    }

    fn rc(&self) -> Option<Rc<Self>> {
        self.this.upgrade()
    }

    fn request(
        self: &Rc<Self>,
        kind: &'static str,
        request: serde_json::Value,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<serde_json::Value>> {
        let (answer, answered) = async_channel::bounded(1);
        self.send(kind, request, Some(answer), window, cx);
        cx.foreground_executor().spawn(async move {
            answered
                .recv()
                .await
                .map_err(|_| anyhow!("the script ended the {kind} request without an answer"))?
                .map_err(anyhow::Error::msg)
        })
    }

    fn send(
        self: &Rc<Self>,
        kind: &'static str,
        request: serde_json::Value,
        answer: Option<Answer>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let lsp = self.clone();
        window.defer(cx, move |window, cx| {
            if let (Some(runtime), Some(dispatch)) = (lsp.runtime.upgrade(), lsp.dispatch.upgrade())
            {
                runtime
                    .dispatch_editor_lsp(&dispatch, &lsp.owner, kind, &request, answer, window, cx);
            }
        });
    }

    fn trigger_ending(&self, text: &str) -> Option<&String> {
        self.trigger_characters
            .iter()
            .find(|trigger| !trigger.is_empty() && text.ends_with(trigger.as_str()))
    }
}

fn position_request(text: &Rope, offset: usize) -> serde_json::Value {
    serde_json::json!({
        "position": text.offset_to_position(offset),
        "offset": text.offset_to_offset_utf16(offset),
    })
}

fn decode<T: serde::de::DeserializeOwned + Default>(value: serde_json::Value) -> Result<T> {
    if value.is_null() {
        return Ok(T::default());
    }
    Ok(serde_json::from_value(value)?)
}

fn location_link(location: Location) -> LocationLink {
    LocationLink {
        origin_selection_range: None,
        target_uri: location.uri,
        target_range: location.range,
        target_selection_range: location.range,
    }
}

impl CompletionProvider for ScriptLsp {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let Some(lsp) = self.rc() else {
            return Task::ready(Ok(CompletionResponse::Array(vec![])));
        };
        let typed = trigger.trigger_character.unwrap_or_default();
        let context = match self.trigger_ending(&typed) {
            Some(character) => CompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(character.clone()),
            },
            None => CompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
        };
        let mut request = position_request(text, offset);
        request["context"] = serde_json::to_value(context).unwrap_or_default();
        let answer = lsp.request("completion", request, window, cx);
        cx.foreground_executor().spawn(async move {
            let response: Option<CompletionResponse> = decode(answer.await?)?;
            Ok(response.unwrap_or(CompletionResponse::Array(vec![])))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text
            .chars()
            .last()
            .is_some_and(|last| last.is_alphanumeric() || last == '_')
            || self.trigger_ending(new_text).is_some()
    }
}

impl HoverProvider for ScriptLsp {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let Some(lsp) = self.rc() else {
            return Task::ready(Ok(None));
        };
        let answer = lsp.request("hover", position_request(text, offset), window, cx);
        cx.foreground_executor()
            .spawn(async move { decode(answer.await?) })
    }
}

impl DefinitionProvider for ScriptLsp {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        let Some(lsp) = self.rc() else {
            return Task::ready(Ok(vec![]));
        };
        let answer = lsp.request("definition", position_request(text, offset), window, cx);
        cx.foreground_executor().spawn(async move {
            let response: Option<GotoDefinitionResponse> = decode(answer.await?)?;
            Ok(match response {
                None => vec![],
                Some(GotoDefinitionResponse::Scalar(location)) => vec![location_link(location)],
                Some(GotoDefinitionResponse::Array(locations)) => {
                    locations.into_iter().map(location_link).collect()
                }
                Some(GotoDefinitionResponse::Link(links)) => links,
            })
        })
    }
}

pub(super) fn set_diagnostics(
    state: &Entity<EditorState>,
    diagnostics: Vec<lsp_types::Diagnostic>,
    cx: &mut App,
) {
    state.update(cx, |state, cx| {
        let text = state.text().clone();
        if let Some(set) = state.diagnostics_mut() {
            set.reset(&text);
            set.extend(diagnostics);
        }
        cx.notify();
    });
}

pub(super) fn apply_edits(
    state: &Entity<EditorState>,
    mut edits: Vec<TextEdit>,
    window: &mut Window,
    cx: &mut App,
) {
    edits.sort_by(|a, b| {
        (b.range.start.line, b.range.start.character)
            .cmp(&(a.range.start.line, a.range.start.character))
    });
    state.update(cx, |state, cx| state.apply_lsp_edits(&edits, window, cx));
}

impl InputCallbackOwner {
    fn current() -> Self {
        Self {
            policy: scope::policy(),
            application: scope::current_application_generation(),
            view: scope::current_view().map(|view| view.downgrade()),
        }
    }
}

impl ShellRuntime {
    #[allow(clippy::too_many_arguments)]
    fn dispatch_editor_lsp(
        self: &Rc<Self>,
        dispatch: &Persistent<Function<'static>>,
        owner: &InputCallbackOwner,
        kind: &str,
        request: &serde_json::Value,
        answer: Option<Answer>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if owner
            .application
            .as_ref()
            .is_some_and(|application| !application.is_active())
        {
            return;
        }
        let view = owner.view.as_ref().and_then(WeakEntity::upgrade);
        let (_guard, generation) = scope::enter_with_application(
            self,
            window,
            cx,
            ScopePhase::Event,
            view,
            owner.policy.clone(),
            owner.application.clone(),
        );
        let request = request.to_string();
        let result = self.with_js(|ctx| {
            let dispatch = dispatch.clone().restore(ctx)?;
            let settle = match answer {
                Some(answer) => Function::new(
                    ctx.clone(),
                    move |value: Option<String>, error: Option<String>| {
                        let settled = match error {
                            Some(error) => Err(error),
                            None => serde_json::from_str(value.as_deref().unwrap_or("null"))
                                .map_err(|error| error.to_string()),
                        };
                        let _ = answer.try_send(settled);
                    },
                )?
                .into_value(),
                None => Value::new_null(ctx.clone()),
            };
            dispatch.call::<_, ()>((
                kind,
                ctx.json_parse(request)?,
                settle,
                context_object(ctx, ContextBinding::Call(generation))?,
            ))
        });
        if let Err(error) = result {
            tracing::error!("error in editor {kind} provider: {error}");
        }
        scheduler::drain_runtime_jobs(self, window, cx);
    }
}
