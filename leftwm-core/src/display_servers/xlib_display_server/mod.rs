use crate::config::Config;
use crate::display_action::DisplayAction;
use crate::models::Mode;
use crate::models::Screen;
use crate::models::Window;
use crate::models::WindowHandle;
use crate::models::WindowState;
use crate::models::Workspace;
use crate::state::State;
use crate::utils;
use crate::DisplayEvent;
use crate::DisplayServer;
use futures::prelude::*;
use std::pin::Pin;
use x11_dl::xlib;

mod event_translate;
mod event_translate_client_message;
mod event_translate_property_notify;
mod xatom;
mod xwrap;
pub use xwrap::XWrap;

use event_translate::XEvent;
mod xcursor;

pub struct XlibDisplayServer {
    xw: XWrap,
    root: xlib::Window,
    initial_events: Option<Vec<DisplayEvent>>,
}

impl DisplayServer for XlibDisplayServer {
    fn new(config: &impl Config) -> Self {
        let mut wrap = XWrap::new();

        wrap.init(config); //setup events masks

        let root = wrap.get_default_root();
        let instance = Self {
            xw: wrap,
            root,
            initial_events: None,
        };
        let initial_events = instance.initial_events(config);

        Self {
            initial_events: Some(initial_events),
            ..instance
        }
    }

    fn load_config(&mut self, config: &impl Config) {
        self.xw.load_config(config);
    }

    fn update_windows(
        &self,
        windows: Vec<&Window>,
        focused_window: Option<&Window>,
        state: &State,
    ) {
        let max_tag_index = state
            .workspaces
            .iter()
            .flat_map(|w| &w.tags)
            .max()
            .map_or(0, std::borrow::ToOwned::to_owned);

        let to_the_right = state
            .screens
            .iter()
            .map(|s| s.bbox.width + s.bbox.x + 100)
            .max();
        let max_screen_width = state.screens.iter().map(|s| s.bbox.width).max();

        for window in &windows {
            let is_focused = match focused_window {
                Some(f) => f.handle == window.handle,
                None => false,
            };

            let hide_offset = right_offset(max_tag_index, to_the_right, window)
                .unwrap_or_else(|| left_offset(max_screen_width, window));

            self.xw.update_window(window, is_focused, hide_offset);
        }
    }

    fn update_workspaces(&self, _workspaces: Vec<&Workspace>, focused: Option<&Workspace>) {
        if let Some(focused) = focused {
            self.xw.set_current_desktop(&focused.tags);
        }
    }

    fn get_next_events(&mut self) -> Vec<DisplayEvent> {
        let mut events = vec![];

        if let Some(initial_events) = self.initial_events.take() {
            for e in initial_events {
                (&mut events).push(e);
            }
        }

        let event_in_queue = self.xw.queue_len();

        for _ in 0..event_in_queue {
            let xlib_event = self.xw.get_next_event();
            let event = XEvent(&mut self.xw, xlib_event).into();
            if let Some(e) = event {
                log::trace!("DisplayEvent: {:?}", e);
                events.push(e);
            }
        }

        for event in &events {
            if let DisplayEvent::WindowDestroy(WindowHandle::XlibHandle(w)) = event {
                self.xw.force_unmapped(*w);
            }
        }

        events
    }

    // TODO: Split function up.
    #[allow(clippy::too_many_lines)]
    fn execute_action(&mut self, act: DisplayAction) -> Option<DisplayEvent> {
        log::trace!("DisplayAction: {:?}", act);
        let event: Option<DisplayEvent> = match act {
            DisplayAction::KillWindow(w) => {
                self.xw.kill_window(&w);
                None
            }
            DisplayAction::AddedWindow(w, follow_mouse) => {
                self.xw.setup_managed_window(w, follow_mouse)
            }
            DisplayAction::MoveMouseOver(handle) => {
                if let WindowHandle::XlibHandle(win) = handle {
                    let _ = self.xw.move_cursor_to_window(win);
                }
                None
            }
            DisplayAction::MoveMouseOverPoint(point) => {
                let _ = self.xw.move_cursor_to_point(point);
                None
            }
            DisplayAction::DestroyedWindow(w) => {
                self.xw.teardown_managed_window(&w);
                None
            }
            DisplayAction::WindowTakeFocus(w) => {
                self.xw.window_take_focus(&w);
                None
            }
            DisplayAction::Unfocus => {
                self.xw.unfocus();
                None
            }
            DisplayAction::SetState(h, toggle_to, window_state) => {
                // TODO: impl from for windowstate and xlib::Atom
                let state = match window_state {
                    WindowState::Modal => self.xw.atoms.NetWMStateModal,
                    WindowState::Sticky => self.xw.atoms.NetWMStateSticky,
                    WindowState::MaximizedVert => self.xw.atoms.NetWMStateMaximizedVert,
                    WindowState::MaximizedHorz => self.xw.atoms.NetWMStateMaximizedHorz,
                    WindowState::Shaded => self.xw.atoms.NetWMStateShaded,
                    WindowState::SkipTaskbar => self.xw.atoms.NetWMStateSkipTaskbar,
                    WindowState::SkipPager => self.xw.atoms.NetWMStateSkipPager,
                    WindowState::Hidden => self.xw.atoms.NetWMStateHidden,
                    WindowState::Fullscreen => self.xw.atoms.NetWMStateFullscreen,
                    WindowState::Above => self.xw.atoms.NetWMStateAbove,
                    WindowState::Below => self.xw.atoms.NetWMStateBelow,
                };
                self.xw.set_state(h, toggle_to, state);
                None
            }
            DisplayAction::SetWindowOrder(windows) => {
                // The windows we are managing should be behind unmanaged windows. Unless they are
                // fullscreen, or their children.
                let (fullscreen_windows, other): (Vec<&Window>, Vec<&Window>) =
                    windows.iter().partition(|w| w.is_fullscreen());
                // Fullscreen windows.
                let level2: Vec<WindowHandle> =
                    fullscreen_windows.iter().map(|w| w.handle).collect();
                let (fullscreen_children, other): (Vec<&Window>, Vec<&Window>) =
                    other.iter().partition(|w| {
                        level2.contains(&w.transient.unwrap_or(WindowHandle::XlibHandle(0)))
                    });
                // Fullscreen windows children.
                let level1: Vec<WindowHandle> =
                    fullscreen_children.iter().map(|w| w.handle).collect();
                // Left over managed windows.
                let level4: Vec<WindowHandle> = other.iter().map(|w| w.handle).collect();
                // Unmanaged windows.
                let level3: Vec<WindowHandle> = self
                    .xw
                    .get_all_windows()
                    .unwrap_or_default()
                    .iter()
                    .filter(|&w| *w != self.root)
                    .map(|w| WindowHandle::XlibHandle(*w))
                    .filter(|&h| !windows.iter().any(|w| w.handle == h))
                    .collect();
                let all: Vec<WindowHandle> = level1
                    .iter()
                    .chain(level2.iter())
                    .chain(level3.iter())
                    .chain(level4.iter())
                    .copied()
                    .collect();
                self.xw.restack(all);
                None
            }
            DisplayAction::MoveToTop(handle) => {
                self.xw.move_to_top(&handle);
                None
            }
            DisplayAction::FocusWindowUnderCursor => {
                let point = self.xw.get_cursor_point().ok()?;
                let evt = DisplayEvent::MoveFocusTo(point.0, point.1);
                Some(evt)
            }
            DisplayAction::StartMovingWindow(w) => {
                self.xw.set_mode(Mode::MovingWindow(w));
                None
            }
            DisplayAction::StartResizingWindow(w) => {
                self.xw.set_mode(Mode::ResizingWindow(w));
                None
            }
            DisplayAction::NormalMode => {
                self.xw.set_mode(Mode::Normal);
                None
            }
            DisplayAction::SetCurrentTags(tags) => {
                self.xw.set_current_desktop(&tags);
                None
            }
            DisplayAction::SetWindowTags(handle, tag) => {
                if let WindowHandle::XlibHandle(window) = handle {
                    self.xw.set_window_desktop(window, &tag);
                }
                None
            }
            DisplayAction::ReloadKeyGrabs(keybinds) => {
                self.xw.reset_grabs(&keybinds);
                None
            }
        };
        if event.is_some() {
            log::trace!("DisplayEvent: {:?}", event);
        }
        event
    }

    fn wait_readable(&self) -> Pin<Box<dyn Future<Output = ()>>> {
        let task_notify = self.xw.task_notify.clone();
        Box::pin(async move {
            task_notify.notified().await;
        })
    }

    fn flush(&self) {
        self.xw.flush();
    }

    /// Creates a verify focus event for the cursors current window.
    fn generate_verify_focus_event(&self) -> Option<DisplayEvent> {
        let handle = self.xw.get_cursor_window().ok()?;
        Some(DisplayEvent::VerifyFocusedAt(handle))
    }
}

impl XlibDisplayServer {
    /// Return a vec of events for setting up state of WM.
    fn initial_events(&self, config: &impl Config) -> Vec<DisplayEvent> {
        let mut events = vec![];
        if let Some(workspaces) = config.workspaces() {
            if workspaces.is_empty() {
                // tell manager about existing screens
                self.xw.get_screens().into_iter().for_each(|screen| {
                    let e = DisplayEvent::ScreenCreate(screen);
                    events.push(e);
                });
            } else {
                for wsc in &workspaces {
                    let mut screen = Screen::from(wsc);
                    screen.root = WindowHandle::XlibHandle(self.root);
                    let e = DisplayEvent::ScreenCreate(screen);
                    events.push(e);
                }
            }
        }

        // tell manager about existing windows
        self.find_all_windows().into_iter().for_each(|w| {
            let cursor = self.xw.get_cursor_point().ok().unwrap_or_default();
            let e = DisplayEvent::WindowCreate(w, cursor.0, cursor.1);
            events.push(e);
        });

        events
    }

    fn find_all_windows(&self) -> Vec<Window> {
        let mut all: Vec<Window> = Vec::new();
        match self.xw.get_all_windows() {
            Ok(handles) => handles.into_iter().for_each(|handle| {
                let attrs = match self.xw.get_window_attrs(handle) {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let managed = match self.xw.get_transient_for(handle) {
                    Some(_) => attrs.map_state == 2,
                    None => attrs.override_redirect <= 0 && attrs.map_state == 2,
                };
                if managed {
                    let name = self.xw.get_window_name(handle);
                    let pid = self.xw.get_window_pid(handle);
                    let w = Window::new(WindowHandle::XlibHandle(handle), name, pid);
                    all.push(w);
                }
            }),
            Err(err) => {
                println!("ERROR: {}", err);
            }
        }
        all
    }
}

//return an offset to hide the window in the right, if it should be hidden on the right
fn right_offset(
    max_tag_index: usize,
    max_right_screen: Option<i32>,
    window: &Window,
) -> Option<i32> {
    let max_right_screen = max_right_screen?;
    for tag in &window.tags {
        if tag > &max_tag_index {
            return Some(max_right_screen + window.x());
        }
    }
    None
}

//return an offset to hide the window on the left
fn left_offset(max_screen_width: Option<i32>, window: &Window) -> i32 {
    let mut left = -(window.width());
    if let Some(screen_width) = max_screen_width {
        let best_left = window.x() - screen_width;
        left = std::cmp::min(best_left, left);
    }
    left
}
