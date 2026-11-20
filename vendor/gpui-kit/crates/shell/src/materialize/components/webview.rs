//! A native web view placed over its element's bounds.

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Refineable as _, Style, StyleRefinement, Window,
};

pub(in crate::materialize) fn webview(
    id: ElementId,
    url: String,
    style: StyleRefinement,
) -> AnyElement {
    NativeWebView { id, url, style }.into_any_element()
}

struct NativeWebView {
    id: ElementId,
    url: String,
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
        #[cfg(target_os = "macos")]
        if let Some(id) = id {
            native::show(id, &self.url, bounds, window);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (id, bounds, window);
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::{cell::RefCell, collections::HashMap, mem::ManuallyDrop};

    use gpui::{Bounds, GlobalElementId, Pixels, Window};
    use wry::{
        Rect, WebView, WebViewBuilder,
        dpi::{LogicalPosition, LogicalSize},
    };

    struct Shown {
        view: WebView,
        url: String,
        bounds: Bounds<Pixels>,
    }

    thread_local! {
        static VIEWS: ManuallyDrop<RefCell<HashMap<GlobalElementId, Shown>>> = ManuallyDrop::new(RefCell::new(HashMap::new()));
    }

    struct Painted(GlobalElementId);

    impl Drop for Painted {
        fn drop(&mut self) {
            let _ = VIEWS.try_with(|views| {
                if let Some(shown) = views.borrow().get(&self.0) {
                    let _ = shown.view.set_visible(false);
                    let _ = shown.view.focus_parent();
                }
            });
        }
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

    pub(super) fn show(id: &GlobalElementId, url: &str, bounds: Bounds<Pixels>, window: &mut Window) {
        window.with_element_state::<Painted, _>(id, |painted, window| {
            let painted = painted.unwrap_or_else(|| Painted(id.clone()));
            VIEWS.with(|views| {
                let mut views = views.borrow_mut();
                if let Some(shown) = views.get_mut(id) {
                    if shown.url != url {
                        let _ = shown.view.load_url(url);
                        shown.url = url.to_owned();
                    }
                    if shown.bounds != bounds {
                        let _ = shown.view.set_bounds(rect(bounds));
                        shown.bounds = bounds;
                    }
                    let _ = shown.view.set_visible(true);
                    return;
                }
                match WebViewBuilder::new()
                    .with_url(url)
                    .with_bounds(rect(bounds))
                    .with_devtools(true)
                    .build_as_child(&*window)
                {
                    Ok(view) => {
                        views.insert(
                            id.clone(),
                            Shown {
                                view,
                                url: url.to_owned(),
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
