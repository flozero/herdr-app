//! A native web view placed over its element's bounds.

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Refineable as _, Style, StyleRefinement, Window,
};

use crate::spec::WebViewSpec;

pub(in crate::materialize) fn webview(
    id: ElementId,
    spec: WebViewSpec,
    style: StyleRefinement,
) -> AnyElement {
    NativeWebView { id, spec, style }.into_any_element()
}

struct NativeWebView {
    id: ElementId,
    spec: WebViewSpec,
    style: StyleRefinement,
}

impl IntoElement for NativeWebView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for NativeWebView {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let clipped = bounds.intersect(&window.content_mask().bounds);
        let visible = if crate::web_view::covered(clipped) { Bounds::default() } else { clipped };
        #[cfg(target_os = "macos")]
        if let Some(id) = id {
            native::show(id, &self.spec, visible, window);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (id, visible, window);
            unsupported();
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| tracing::warn!("webview: only macOS shows web views; this one stays empty"));
}

#[cfg(target_os = "macos")]
mod native {
    use std::{cell::RefCell, collections::HashMap, mem::ManuallyDrop};

    use gpui::{Bounds, GlobalElementId, Pixels, Window, WindowId};
    use wry::{
        Rect, WebView, WebViewBuilder,
        dpi::{LogicalPosition, LogicalSize},
    };

    use crate::spec::WebViewSpec;

    type Key = (WindowId, GlobalElementId);

    struct Shown {
        view: WebView,
        url: String,
        bounds: Bounds<Pixels>,
    }

    thread_local! {
        static VIEWS: ManuallyDrop<RefCell<HashMap<Key, Shown>>> = ManuallyDrop::new(RefCell::new(HashMap::new()));
    }

    struct Painted {
        key: Key,
        keep: bool,
    }

    impl Drop for Painted {
        fn drop(&mut self) {
            let _ = VIEWS.try_with(|views| {
                let Ok(mut views) = views.try_borrow_mut() else {
                    return;
                };
                if self.keep {
                    if let Some(shown) = views.get(&self.key) {
                        hide(&shown.view);
                    }
                } else if let Some(shown) = views.remove(&self.key) {
                    hide(&shown.view);
                }
            });
        }
    }

    fn hide(view: &WebView) {
        let _ = view.set_visible(false);
        let _ = view.focus_parent();
    }

    fn rect(bounds: Bounds<Pixels>) -> Rect {
        Rect {
            position: LogicalPosition::new(f64::from(bounds.origin.x), f64::from(bounds.origin.y))
                .into(),
            size: LogicalSize::new(
                f64::from(bounds.size.width),
                f64::from(bounds.size.height),
            )
            .into(),
        }
    }

    pub(super) fn show(id: &GlobalElementId, spec: &WebViewSpec, bounds: Bounds<Pixels>, window: &mut Window) {
        let key = (window.window_handle().window_id(), id.clone());
        window.with_element_state::<Painted, _>(id, |painted, window| {
            let mut painted = painted.unwrap_or_else(|| Painted { key: key.clone(), keep: spec.keep });
            painted.keep = spec.keep;
            VIEWS.with(|views| {
                let mut views = views.borrow_mut();
                if let Some(shown) = views.get_mut(&key) {
                    if shown.url != spec.url {
                        let _ = shown.view.load_url(&spec.url);
                        shown.url = spec.url.clone();
                    }
                    if shown.bounds != bounds {
                        let _ = shown.view.set_bounds(rect(bounds));
                        shown.bounds = bounds;
                    }
                    if bounds.is_empty() {
                        hide(&shown.view);
                    } else {
                        let _ = shown.view.set_visible(true);
                    }
                    return;
                }
                match WebViewBuilder::new()
                    .with_url(&spec.url)
                    .with_bounds(rect(bounds))
                    .with_visible(!bounds.is_empty())
                    .with_devtools(cfg!(debug_assertions))
                    .build_as_child(&*window)
                {
                    Ok(view) => {
                        views.insert(
                            key.clone(),
                            Shown {
                                view,
                                url: spec.url.clone(),
                                bounds,
                            },
                        );
                    }
                    Err(error) => tracing::warn!("webview: {error}"),
                }
            });
            ((), painted)
        });
    }
}
