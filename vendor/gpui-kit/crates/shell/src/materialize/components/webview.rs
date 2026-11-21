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
        cx: &mut App,
    ) {
        let clipped = bounds.intersect(&window.content_mask().bounds);
        let visible = if crate::web_view::covered(clipped) { Bounds::default() } else { clipped };
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(id) = id {
            native::show(id, &self.spec, visible, window, cx);
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (id, visible, window, cx);
            unsupported();
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unsupported() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| tracing::warn!("webview: only macOS and Linux (X11) show web views; this one stays empty"));
}

/// Linux: a web view is a WebKitGTK child of the app's X11 window. wry only embeds in X11
/// windows (`build_as_child`), and GTK, which WebKitGTK needs, has to be initialised and run
/// beside GPUI's own loop.
#[cfg(target_os = "linux")]
mod linux {
    use std::{cell::Cell, os::raw::c_ulong, sync::Once, time::Duration};

    use gpui::{App, Window};
    use raw_window_handle::{
        HandleError, HasWindowHandle, RawWindowHandle, WindowHandle, XlibWindowHandle,
    };

    /// How often GTK's pending events (WebKit's painting, input, loading) are run.
    const PUMP_EVERY: Duration = Duration::from_millis(8);

    /// The app's X11 window, as the Xlib handle wry asks for. GPUI's X11 windows give an Xcb
    /// handle: the same window id, another name for it.
    pub(super) struct Parent(c_ulong);

    impl HasWindowHandle for Parent {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let raw = RawWindowHandle::Xlib(XlibWindowHandle::new(self.0));
            // SAFETY: the handle only names the window; wry reads its id and nothing else.
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    /// The window web views go in, or `None` when the app isn't on X11 (Wayland has no way to
    /// embed a foreign window: start Herdr without `WAYLAND_DISPLAY` to run it on XWayland).
    pub(super) fn parent(window: &Window) -> Option<Parent> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Xcb(handle) => Some(Parent(c_ulong::from(handle.window.get()))),
            RawWindowHandle::Xlib(handle) => Some(Parent(handle.window)),
            _ => {
                static WARNED: Once = Once::new();
                WARNED.call_once(|| {
                    tracing::warn!(
                        "webview: web views need an X11 window and this one isn't. \
                         Start Herdr with WAYLAND_DISPLAY unset to run it on XWayland; this web view stays empty"
                    );
                });
                None
            }
        }
    }

    thread_local! {
        static GTK: Cell<Option<bool>> = const { Cell::new(None) };
        static PUMPING: Cell<bool> = const { Cell::new(false) };
    }

    /// Starts GTK on the X11 backend, once. False when it can't start.
    pub(super) fn gtk_ready() -> bool {
        GTK.with(|state| {
            if let Some(ready) = state.get() {
                return ready;
            }
            // The web views are X11 children: GTK must not pick Wayland just because the
            // session has it.
            gtk::gdk::set_allowed_backends("x11");
            let ready = match gtk::init() {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!("webview: GTK didn't start: {error}");
                    false
                }
            };
            state.set(Some(ready));
            ready
        })
    }

    /// Runs GTK's events on GPUI's thread for as long as `alive` says a web view exists.
    pub(super) fn pump(cx: &mut App, alive: fn() -> bool) {
        if PUMPING.replace(true) {
            return;
        }
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(PUMP_EVERY).await;
                while gtk::events_pending() {
                    gtk::main_iteration_do(false);
                }
                if !alive() {
                    break;
                }
            }
            PUMPING.set(false);
        })
        .detach();
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use std::{cell::RefCell, collections::HashMap, mem::ManuallyDrop};

    use gpui::{Bounds, GlobalElementId, Pixels, Window, WindowId};
    #[cfg(target_os = "macos")]
    use wry::dpi::{LogicalPosition, LogicalSize};
    #[cfg(target_os = "linux")]
    use wry::dpi::{PhysicalPosition, PhysicalSize};
    use wry::{Rect, WebView, WebViewBuilder};

    use crate::spec::WebViewSpec;

    type Key = (WindowId, GlobalElementId);

    struct Shown {
        view: WebView,
        url: String,
        bounds: Bounds<Pixels>,
        visible: bool,
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
                    if let Some(shown) = views.get_mut(&self.key) {
                        hide(&shown.view);
                        shown.visible = false;
                    }
                } else if let Some(shown) = views.remove(&self.key) {
                    hide(&shown.view);
                }
            });
        }
    }

    fn hide(view: &WebView) {
        let _ = view.set_visible(false);
        // X hands the keyboard back to the app's window when the focused child is unmapped.
        #[cfg(target_os = "macos")]
        let _ = view.focus_parent();
    }

    #[cfg(target_os = "linux")]
    fn views_alive() -> bool {
        VIEWS.try_with(|views| views.try_borrow().map_or(true, |views| !views.is_empty())).unwrap_or(false)
    }

    #[cfg(target_os = "macos")]
    fn rect(bounds: Bounds<Pixels>, _scale: f32) -> Rect {
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

    /// In X pixels: wry turns logical sizes into pixels with a factor of its own, taken from the
    /// screen's reported size, which isn't the one GPUI laid the window out with.
    #[cfg(target_os = "linux")]
    fn rect(bounds: Bounds<Pixels>, scale: f32) -> Rect {
        let pixels = |value: Pixels| (f32::from(value) * scale).round();
        Rect {
            position: PhysicalPosition::new(pixels(bounds.origin.x) as i32, pixels(bounds.origin.y) as i32).into(),
            size: PhysicalSize::new(pixels(bounds.size.width) as u32, pixels(bounds.size.height) as u32).into(),
        }
    }

    pub(super) fn show(
        id: &GlobalElementId,
        spec: &WebViewSpec,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        #[cfg(target_os = "linux")]
        let Some(parent) = super::linux::parent(window) else {
            return;
        };
        #[cfg(target_os = "linux")]
        if !super::linux::gtk_ready() {
            return;
        }
        #[cfg(target_os = "linux")]
        super::linux::pump(cx, views_alive);
        #[cfg(not(target_os = "linux"))]
        let _ = cx;
        let key = (window.window_handle().window_id(), id.clone());
        let scale = window.scale_factor();
        window.with_element_state::<Painted, _>(id, |painted, window| {
            #[cfg(target_os = "linux")]
            let _ = &window;
            let mut painted = painted.unwrap_or_else(|| Painted { key: key.clone(), keep: spec.keep });
            painted.keep = spec.keep;
            VIEWS.with(|views| {
                let mut views = views.borrow_mut();
                if let Some(shown) = views.get_mut(&key) {
                    if shown.url != spec.url {
                        let _ = shown.view.load_url(&spec.url);
                        shown.url = spec.url.clone();
                    }
                    let showing = !bounds.is_empty();
                    // A web view moved or resized while hidden keeps its old size once shown:
                    // show it first, then place it.
                    let reshown = showing && !shown.visible;
                    if reshown {
                        let _ = shown.view.set_visible(true);
                    }
                    if reshown || shown.bounds != bounds {
                        let _ = shown.view.set_bounds(rect(bounds, scale));
                        shown.bounds = bounds;
                    }
                    if showing {
                        let _ = shown.view.set_visible(true);
                    } else if shown.visible || cfg!(target_os = "macos") {
                        hide(&shown.view);
                    }
                    shown.visible = showing;
                    return;
                }
                let builder = WebViewBuilder::new()
                    .with_url(&spec.url)
                    .with_bounds(rect(bounds, scale))
                    .with_visible(!bounds.is_empty())
                    .with_devtools(cfg!(debug_assertions));
                #[cfg(target_os = "macos")]
                let built = builder.build_as_child(&*window);
                #[cfg(target_os = "linux")]
                let built = builder.build_as_child(&parent);
                match built {
                    Ok(view) => {
                        views.insert(
                            key.clone(),
                            Shown {
                                view,
                                url: spec.url.clone(),
                                bounds,
                                visible: !bounds.is_empty(),
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
