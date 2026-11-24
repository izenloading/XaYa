//! Terminal window context.

use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::mem;
use std::rc::Rc;
use std::time::Instant;

use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
use log::error;
use serde_json as json;
use winit::event::{ElementState, Event as WinitEvent, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;

use alacritty_terminal::event::Event as TerminalEvent;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
#[cfg(unix)]
use alacritty_terminal::shell_integration::ShellMark;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::test::TermSize;

use crate::agent_panel::AgentPanel;
use crate::cli::{ParsedOptions, TerminalOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
#[cfg(unix)]
use crate::daemon::foreground_process_path;
use crate::display::color::DIM_FACTOR;
use crate::display::window::{ImeInhibitor, Window};
use crate::display::{Display, PaneFrame, SizeInfo};
#[cfg(unix)]
use crate::event::WorktreeRequest;
use crate::event::{ActionContext, Event, EventType, Mouse, TouchPurpose};
use crate::layout::{CellRect, Layout, PaneId, SplitDirection};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::{Message, MessageBuffer, MessageType};
use crate::pane::Pane;
use crate::program_status::{Attention, ProgramActivity};
use crate::renderer::metrics::{self, FrameMetrics};
use crate::renderer::rects::RenderRect;
use crate::scheduler::Scheduler;
#[cfg(unix)]
use crate::session::{self, AgentDirs, AgentSession, SessionWindow};
#[cfg(unix)]
use crate::window_status::TurnChanges;
use crate::window_status::WindowStatus;
use crate::{input, renderer};

/// How long a window opened again keeps reporting the session it resumes, while its agent starts.
#[cfg(unix)]
const RESTORE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Event context for one individual Alacritty window.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    pub display: Display,
    pub dirty: bool,
    #[cfg(unix)]
    pub managed_bridge: Option<crate::managed_panel::Bridge>,
    render_metrics: Option<FrameMetrics>,
    /// Events to handle, with the pane they're for when it's decided already.
    event_queue: Vec<(Option<PaneId>, WinitEvent<Event>)>,
    /// The window's terminals; the layout orders them.
    panes: Vec<Pane>,
    /// How the panes split the window.
    layout: Layout,
    /// The pane keyboard input goes to.
    focused: PaneId,
    /// The pane mouse buttons were pressed in, which gets the mouse until they're released.
    mouse_pane: Option<PaneId>,
    /// Lines between panes.
    dividers: Vec<RenderRect>,
    /// Whether the window has the keyboard focus.
    window_focused: bool,
    proxy: EventLoopProxy<Event>,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    prev_notification: Option<Instant>,
    modifiers: Modifiers,
    mouse: Mouse,
    touch: TouchPurpose,
    occluded: bool,
    preserve_title: bool,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
}
impl WindowContext {
    /// Create initial window context that does bootstrapping the graphics API we're going to use.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        #[cfg(target_os = "macos")]
        if config.debug.renderer() == Some(crate::config::debug::RendererPreference::Metal) {
            let mut identity = config.window.identity.clone();
            options.window_identity.override_identity_config(&mut identity);
            let window = Window::new(event_loop, &config, &identity, &mut options)?;
            let display = Display::new(window, None, &config, false)?;
            return Self::new(display, config, options, proxy);
        }

        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Windows has different order of GL platform initialization compared to any other platform;
        // it requires the window first.
        #[cfg(windows)]
        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        #[cfg(windows)]
        let raw_window_handle = Some(window.raw_window_handle());

        #[cfg(not(windows))]
        let raw_window_handle = None;

        let gl_display = renderer::platform::create_gl_display(
            raw_display_handle,
            raw_window_handle,
            config.debug.prefer_egl,
        )?;
        let gl_config = renderer::platform::pick_gl_config(&gl_display, raw_window_handle)?;

        #[cfg(not(windows))]
        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)?;

        let display = Display::new(window, Some(gl_context), &config, false)?;

        Self::new(display, config, options, proxy)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: Option<&GlutinConfig>,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Check if new window will be opened as a tab.
        // This must be done before `Window::new()`, which unsets `window_tabbing_id`.
        #[cfg(target_os = "macos")]
        let tabbed = options.window_tabbing_id.is_some();
        #[cfg(not(target_os = "macos"))]
        let tabbed = false;

        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.and_then(|config| config.x11_visual()),
        )?;

        // A missing GL config indicates that the initial window uses Metal.
        let gl_context = gl_config
            .map(|gl_config| {
                renderer::platform::create_gl_context(
                    &gl_config.display(),
                    gl_config,
                    Some(window.raw_window_handle()),
                )
            })
            .transpose()?;

        let display = Display::new(window, gl_context, &config, tabbed)?;

        let mut window_context = Self::new(display, config, options, proxy)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.window_config = config_overrides;

        Ok(window_context)
    }

    /// Create a new terminal window context.
    #[cfg_attr(not(unix), allow(unused_mut))]
    fn new(
        mut display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let preserve_title = options.window_identity.title.is_some();
        let window_id = display.window.id();

        let id = PaneId::next();
        let size_info = display.size_info;
        let mut pane =
            Pane::new(id, &config, &options.terminal_options, size_info, proxy.clone(), window_id)?;

        // The session that ran in the window before is resumed through its input.
        #[cfg(unix)]
        if let Some(start) = options.start.clone().map(|start| *start) {
            pane.restored =
                start.restore.filter(|window| window.agent.is_some()).map(|w| (w, Instant::now()));
            pane.pending_input = start.input;
            display.window.set_label(start.label);
        }

        // Start cursor blinking, in case `Focused` isn't sent on startup.
        if config.cursor.style().blinking {
            let event = TerminalEvent::CursorBlinkingChange.into();
            let _ = proxy.send_event(Event::for_pane(event, window_id, id));
        }

        // Create context for the Alacritty window.
        Ok(WindowContext {
            preserve_title,
            render_metrics: metrics::enabled().then(FrameMetrics::default),
            display,
            config,
            panes: vec![pane],
            layout: Layout::Pane(id),
            focused: id,
            mouse_pane: None,
            dividers: Vec::new(),
            window_focused: false,
            proxy,
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            prev_notification: Default::default(),
            message_buffer: Default::default(),
            window_config: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            occluded: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            dirty: Default::default(),
            #[cfg(unix)]
            managed_bridge: None,
        })
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());

        self.display.update_config(&self.config);
        for pane in &self.panes {
            pane.terminal.lock().set_options(self.config.term_options());
        }

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - self.config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != self.config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            // Do not update font size if it has been changed at runtime.
            if self.display.font_size == old_config.font.size().scale(scale_factor) {
                self.display.font_size = self.config.font.size().scale(scale_factor);
            }

            let font = self.config.font.clone().with_size(self.display.font_size);
            self.display.pending_update.set_font(font);
        }

        // Always reload the theme to account for auto-theme switching.
        self.display.window.set_theme(self.config.window.theme());

        // Update display if either padding options or resize increments were changed.
        let window_config = &old_config.window;
        if window_config.padding(1.) != self.config.window.padding(1.)
            || window_config.dynamic_padding != self.config.window.dynamic_padding
            || window_config.resize_increments != self.config.window.resize_increments
        {
            self.display.pending_update.dirty = true;
        }

        // Update title on config reload according to the following table.
        //
        // │cli │ dynamic_title │ current_title == old_config ││ set_title │
        // │ Y  │       _       │              _              ││     N     │
        // │ N  │       Y       │              Y              ││     Y     │
        // │ N  │       Y       │              N              ││     N     │
        // │ N  │       N       │              _              ││     Y     │
        if !self.preserve_title
            && (!self.config.window.dynamic_title
                || self.display.window.title() == old_config.window.identity.title)
        {
            self.display.window.set_title(self.config.window.identity.title.clone());
        }

        let opaque = self.config.window_opacity() >= 1.;

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);

        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(self.config.window.option_as_alt());

        // Change opacity and blur state.
        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(self.config.window.blur);

        // Update hint keys.
        self.display.hint_state.update_alphabet(self.config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push((None, event.into()));

        self.dirty = true;
    }

    /// Get reference to the window's configuration.
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.clear();

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.extend_from_slice(options);

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) {
        self.display.window.requested_redraw = false;

        if self.occluded {
            if let Some(metrics) = &mut self.render_metrics {
                metrics.discard_input();
            }
            return;
        }

        self.dirty = false;
        let frame_start = self.render_metrics.as_ref().map(|_| Instant::now());

        // Force the display to process any pending display update.
        self.display.process_renderer_update();

        // Request immediate re-draw if visual bell animation is not finished yet.
        if !self.display.visual_bell.completed() {
            // We can get an OS redraw which bypasses alacritty's frame throttling, thus
            // marking the window as dirty when we don't have frame yet.
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        // Redraw the window.
        let focused = self.focused;
        let frames = self
            .panes
            .iter_mut()
            .map(|pane| PaneFrame {
                terminal: pane.terminal.lock(),
                search_state: &mut pane.search_state,
                size_info: pane.size_info,
                focused: pane.id == focused,
            })
            .collect();
        let dividers = self.dividers.clone();
        let result =
            self.display.draw(frames, dividers, scheduler, &self.message_buffer, &self.config);
        let rendered = result.is_ok();
        if let Err(err) = result {
            log::error!("Unable to render terminal window: {err}");
            self.display.window.hold = false;
            self.focused_pane().terminal.lock().exit();
        }
        self.dirty |= self.display.retry_frame;
        if rendered
            && !self.display.retry_frame
            && let (Some(metrics), Some(start)) = (&mut self.render_metrics, frame_start)
        {
            metrics.submitted(start.elapsed());
        }
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        if let Some(metrics) = &mut self.render_metrics {
            use winit::event::Ime;
            let input = match &event {
                WinitEvent::WindowEvent {
                    event: WindowEvent::KeyboardInput { event, .. }, ..
                } => {
                    event.state == ElementState::Pressed
                        && event.text.as_ref().is_some_and(|text| !text.is_empty())
                },
                WinitEvent::WindowEvent {
                    event: WindowEvent::Ime(Ime::Commit(_) | Ime::Preedit(..)),
                    ..
                } => true,
                _ => false,
            };
            if input {
                metrics.input();
            }
        }

        if let WinitEvent::WindowEvent { event: WindowEvent::Focused(focused), .. } = &event {
            self.window_focused = *focused;
        }

        if let WinitEvent::UserEvent(event) = &event {
            // Type a pane's startup input once its shell is ready for it.
            #[cfg(unix)]
            if matches!(
                event.payload(),
                EventType::Terminal(TerminalEvent::ShellMark(ShellMark::InputStart))
                    | EventType::StartupInput
            ) {
                let target = event.pane();
                for pane in &mut self.panes {
                    if target.is_none_or(|target| target == pane.id)
                        && let Some(input) = pane.pending_input.take()
                    {
                        use alacritty_terminal::event::Notify;
                        pane.notifier.notify(format!("{input}\r").into_bytes());
                    }
                }
            }

            // Managing panes is the window's.
            match event.payload() {
                EventType::SplitPane { direction, input } => {
                    self.split(*direction, input.clone());
                    return;
                },
                EventType::FocusPane(delta) => {
                    let panes = self.layout.panes();
                    if let Some(index) = panes.iter().position(|id| *id == self.focused) {
                        let index = (index as isize + delta).rem_euclid(panes.len() as isize);
                        self.focus_pane(panes[index as usize]);
                    }
                    return;
                },
                _ => (),
            }
        }

        match event {
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            event => {
                self.event_queue.push((None, event));
                return;
            },
        }

        // Hand each pane its events, in the order they came.
        let focused_was_searching = self.focused_pane().search_state.history_index.is_some();
        let mut batches: Vec<(PaneId, Vec<WinitEvent<Event>>)> = Vec::new();
        for (target, event) in mem::take(&mut self.event_queue) {
            let pane = target.unwrap_or_else(|| self.event_pane(&event));
            match batches.iter_mut().find(|(id, _)| *id == pane) {
                Some((_, events)) => events.push(event),
                None => batches.push((pane, vec![event])),
            }
        }
        for (pane, events) in batches {
            self.handle_pane_events(
                pane,
                events,
                #[cfg(target_os = "macos")]
                event_loop,
                event_proxy,
                clipboard,
                scheduler,
            );
        }

        // Process DisplayUpdate events.
        if self.display.pending_update.dirty {
            self.submit_display_update(focused_was_searching);
            self.dirty = true;
        }

        if self.dirty || self.mouse.hint_highlight_dirty {
            // Hints are for the focused pane, under the mouse.
            let pane = self.panes.iter().find(|pane| pane.id == self.focused).unwrap();
            if pane.contains(self.mouse.x, self.mouse.y) || self.display.highlighted_hint.is_none()
            {
                let terminal = pane.terminal.lock();
                self.dirty |= self.display.update_highlighted_hints(
                    &terminal,
                    &self.config,
                    &self.mouse,
                    self.modifiers.state(),
                );
            } else {
                self.display.highlighted_hint = None;
                self.dirty = true;
            }
            self.mouse.hint_highlight_dirty = false;
        }

        if (!self.dirty || self.occluded)
            && let Some(metrics) = &mut self.render_metrics
        {
            metrics.discard_input();
        }

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if self.dirty
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }
    }

    /// The pane an event without one goes to: the mouse's for mouse events, otherwise the
    /// focused one. Pressing a mouse button in a pane focuses it.
    fn event_pane(&mut self, event: &WinitEvent<Event>) -> PaneId {
        let event = match event {
            WinitEvent::UserEvent(event) => {
                return event
                    .pane()
                    .filter(|id| self.panes.iter().any(|pane| pane.id == *id))
                    .unwrap_or(self.focused);
            },
            WinitEvent::WindowEvent { event, .. } => event,
            _ => return self.focused,
        };

        let under = |panes: &[Pane], x: usize, y: usize| {
            panes.iter().find(|pane| pane.contains(x, y)).map(|pane| pane.id)
        };
        match event {
            WindowEvent::CursorMoved { position, .. } => self.mouse_pane.unwrap_or_else(|| {
                under(&self.panes, position.x as usize, position.y as usize).unwrap_or(self.focused)
            }),
            WindowEvent::MouseInput { state, .. } => {
                let pane = self
                    .mouse_pane
                    .or_else(|| under(&self.panes, self.mouse.x, self.mouse.y))
                    .unwrap_or(self.focused);
                match state {
                    ElementState::Pressed => {
                        self.mouse_pane = Some(pane);
                        self.focus_pane(pane);
                    },
                    ElementState::Released => self.mouse_pane = None,
                }
                pane
            },
            WindowEvent::MouseWheel { .. } | WindowEvent::Touch(_) => {
                under(&self.panes, self.mouse.x, self.mouse.y).unwrap_or(self.focused)
            },
            _ => self.focused,
        }
    }

    /// Handle `events` with `pane`'s terminal.
    fn handle_pane_events(
        &mut self,
        pane_id: PaneId,
        events: Vec<WinitEvent<Event>>,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) {
        let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == pane_id) else { return };
        let focused = pane_id == self.focused;

        // The display works in the pane's geometry, and its title shows the focused pane's.
        self.display.size_info = pane.size_info;
        let window_title = self.display.window.title().to_owned();

        let mut terminal = pane.terminal.lock();
        let context = ActionContext {
            cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
            prev_bell_cmd: &mut self.prev_bell_cmd,
            prev_notification: &mut self.prev_notification,
            program_activity: &mut pane.program_activity,
            #[cfg(unix)]
            turn_tracker: &mut pane.turn_tracker,
            message_buffer: &mut self.message_buffer,
            inline_search_state: &mut pane.inline_search_state,
            search_state: &mut pane.search_state,
            modifiers: &mut self.modifiers,
            notifier: &mut pane.notifier,
            display: &mut self.display,
            mouse: &mut self.mouse,
            touch: &mut self.touch,
            dirty: &mut self.dirty,
            occluded: &mut self.occluded,
            terminal: &mut terminal,
            #[cfg(not(windows))]
            master_fd: pane.master_fd,
            #[cfg(not(windows))]
            shell_pid: pane.shell_pid,
            preserve_title: self.preserve_title,
            config: &self.config,
            event_proxy,
            #[cfg(target_os = "macos")]
            event_loop,
            clipboard,
            scheduler,
            pane: pane_id,
        };
        let mut processor = input::Processor::new(context);
        for event in events {
            processor.handle_event(event);
        }
        drop(terminal);

        // A title set in a pane in the background waits until it's focused.
        if !focused && self.display.window.title() != window_title {
            pane.title = Some(self.display.window.title().to_owned());
            self.display.window.set_title(window_title);
        }

        self.display.size_info = self.focused_pane().size_info;
        self.show_status();
    }

    /// Show the status of the pane that needs the user most in the title.
    fn show_status(&mut self) {
        let pane = self
            .panes
            .iter()
            .filter(|pane| pane.program_activity.urgency().is_some())
            .min_by_key(|pane| pane.program_activity.urgency());
        let badge = pane.and_then(|pane| pane.program_activity.badge());
        self.display.window.set_status_badge(badge);
    }

    /// Split the focused pane in `direction`, typing `input` into the new pane's shell.
    #[cfg_attr(not(unix), allow(unused_mut))]
    fn split(&mut self, direction: SplitDirection, input: Option<String>) {
        let area = self.grid_area();
        let id = PaneId::next();
        if !self.layout.split(area, self.focused, id, direction) {
            let text = "There's no room to split this pane; enlarge the window first".into();
            self.show_message(Message::new(text, MessageType::Warning));
            return;
        }

        // The new pane starts where the focused one is.
        let mut options = TerminalOptions::default();
        #[cfg(unix)]
        {
            let pane = self.focused_pane();
            options.working_directory =
                foreground_process_path(pane.master_fd, pane.shell_pid).ok();
        }
        let size_info = self.focused_pane().size_info;
        let window_id = self.id();
        match Pane::new(id, &self.config, &options, size_info, self.proxy.clone(), window_id) {
            Ok(mut pane) => {
                #[cfg(unix)]
                {
                    pane.pending_input = input;
                }
                #[cfg(not(unix))]
                let _ = input;
                self.panes.push(pane);
                self.layout_panes();
                self.focus_pane(id);
            },
            Err(err) => {
                self.layout.remove(id);
                error!("Unable to open a pane: {err}");
                let text = format!("Unable to open a pane: {err}");
                self.show_message(Message::new(text, MessageType::Error));
            },
        }
    }

    /// Close pane `id`, returning whether it was, unless it's the window's last one.
    pub fn close_pane(&mut self, id: PaneId) -> bool {
        if self.panes.len() < 2 || !self.layout.remove(id) {
            return false;
        }

        // Focus the pane before it, which takes its space.
        if self.focused == id {
            let order: Vec<_> = self.panes.iter().map(|pane| pane.id).collect();
            let index = order.iter().position(|pane| *pane == id).unwrap_or(0);
            let next = self.layout.panes();
            let fallback = order[..index].iter().rev().find(|pane| next.contains(pane));
            let next = fallback.copied().unwrap_or(next[0]);
            self.focus_pane(next);
        }
        self.panes.retain(|pane| pane.id != id);
        self.mouse_pane = self.mouse_pane.filter(|pane| *pane != id);
        self.layout_panes();
        self.show_status();
        true
    }

    /// Give pane `id` the keyboard, telling both panes' programs focus moved.
    fn focus_pane(&mut self, id: PaneId) {
        if self.focused == id || !self.panes.iter().any(|pane| pane.id == id) {
            return;
        }
        let previous = mem::replace(&mut self.focused, id);

        // Keep the title of the pane leaving, and show the one of the pane coming.
        let title = self.display.window.title().to_owned();
        if let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == previous) {
            pane.title = Some(title);
        }
        let title = self.focused_pane().title.clone();
        let title = title.unwrap_or_else(|| self.config.window.identity.title.clone());
        self.display.window.set_title(title);
        self.display.size_info = self.focused_pane().size_info;

        // Hints and the mouse's highlight belonged to the other pane.
        self.display.highlighted_hint = None;
        self.display.vi_highlighted_hint = None;

        if self.window_focused {
            let window_id = self.id();
            let focus = |focused| WinitEvent::WindowEvent {
                window_id,
                event: WindowEvent::Focused(focused),
            };
            self.event_queue.insert(0, (Some(id), focus(true)));
            self.event_queue.insert(0, (Some(previous), focus(false)));
        }
        self.dirty = true;
    }

    /// The grid of the window, in cells, that panes split.
    fn grid_area(&self) -> CellRect {
        let window = &self.display.window_size;
        CellRect { column: 0, line: 0, columns: window.columns(), lines: window.screen_lines() }
    }

    /// Move each pane where the layout puts it in the window's grid.
    fn layout_panes(&mut self) {
        let window = self.display.window_size;
        let area = self.grid_area();
        let (cell_width, cell_height) = (window.cell_width(), window.cell_height());
        let cell_x = |column: usize| window.padding_x() + column as f32 * cell_width;
        let cell_y = |line: usize| window.padding_y() + line as f32 * cell_height;

        let geometries: Vec<(PaneId, SizeInfo)> = if self.panes.len() == 1 {
            vec![(self.panes[0].id, window)]
        } else {
            let rects = self.layout.rects(area).into_iter();
            rects
                .map(|(id, rect)| {
                    let size = window.pane(
                        cell_x(rect.column),
                        cell_y(rect.line),
                        rect.columns,
                        rect.lines,
                    );
                    (id, size)
                })
                .collect()
        };
        for (id, mut size_info) in geometries {
            if let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == id) {
                // A pane being searched shows its search bar on its last line.
                if pane.search_state.history_index.is_some() {
                    size_info.reserve_lines(1);
                }
                pane.resize(size_info);
            }
        }

        // Thin lines through the middle of the cells between panes.
        let color = self.config.colors.primary.foreground;
        let thickness = self.display.window.scale_factor.round().max(1.) as f32;
        self.dividers = self
            .layout
            .dividers(area)
            .into_iter()
            .map(|divider| {
                let rect = divider.rect;
                let (x, y) = (cell_x(rect.column), cell_y(rect.line));
                let (width, height) =
                    (rect.columns as f32 * cell_width, rect.lines as f32 * cell_height);
                match divider.direction {
                    SplitDirection::Right => RenderRect::new(
                        x + (width - thickness) / 2.,
                        y,
                        thickness,
                        height,
                        color,
                        1. - DIM_FACTOR,
                    ),
                    SplitDirection::Down => RenderRect::new(
                        x,
                        y + (height - thickness) / 2.,
                        width,
                        thickness,
                        color,
                        1. - DIM_FACTOR,
                    ),
                }
            })
            .collect();

        self.display.size_info = self.focused_pane().size_info;
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.dirty = true;
    }

    /// The pane keyboard input goes to.
    fn focused_pane(&self) -> &Pane {
        self.panes.iter().find(|pane| pane.id == self.focused).unwrap_or(&self.panes[0])
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// The program status of the pane that needs the user most.
    fn urgent_activity(&self) -> &ProgramActivity {
        &self.urgent_pane().program_activity
    }

    /// The pane whose program needs the user most, or the focused one.
    fn urgent_pane(&self) -> &Pane {
        let pane = self
            .panes
            .iter()
            .filter(|pane| pane.program_activity.urgency().is_some())
            .min_by_key(|pane| pane.program_activity.urgency());
        pane.unwrap_or_else(|| self.focused_pane())
    }

    /// What the last turn of an agent in this window changed: the most urgent pane's, or else
    /// the latest of any pane's.
    #[cfg(unix)]
    fn turn_changes(&self) -> Option<Box<TurnChanges>> {
        let changes = self.urgent_pane().turn_tracker.last().or_else(|| {
            self.panes.iter().filter_map(|pane| pane.turn_tracker.last()).max_by_key(|(_, at)| *at)
        });
        changes.map(|(changes, _)| Box::new(changes.clone()))
    }

    /// Whether a program in this window waits on the user, see [`ProgramActivity`].
    pub fn needs_attention(&self) -> bool {
        self.panes.iter().any(|pane| pane.program_activity.needs_attention())
    }

    /// How much this window's programs need the user, see [`ProgramActivity::attention`].
    pub fn program_attention(&self) -> Option<Attention> {
        self.panes.iter().filter_map(|pane| pane.program_activity.attention()).min()
    }

    /// What this window's programs report, for `msg status`.
    #[cfg(unix)]
    pub fn program_status(&self, id: u64, now: Instant) -> WindowStatus {
        self.window_status(id, self.is_focused(), now)
    }

    /// Whether this window has focus.
    pub fn is_focused(&self) -> bool {
        self.focused_pane().terminal.lock().is_focused
    }

    /// The focused pane's text in `range`, see [`terminal_text::text`].
    #[cfg(unix)]
    pub fn text(&self, range: crate::terminal_text::TextRange) -> Option<String> {
        crate::terminal_text::text(&self.focused_pane().terminal.lock(), range)
    }

    /// A worktree tab to open from this window, on `branch` and starting `command`; by default
    /// a new branch, and the configured command or the focused pane's agent.
    #[cfg(unix)]
    pub fn worktree_request(
        &self,
        branch: Option<String>,
        command: Option<String>,
    ) -> WorktreeRequest {
        let pane = self.focused_pane();
        let cwd = foreground_process_path(pane.master_fd, pane.shell_pid).ok();
        let app = pane.program_activity.app();
        WorktreeRequest::new(&self.display.window, &self.config, cwd, app, branch, command)
    }

    /// Show `message` in the message bar, unless it's already there.
    pub fn show_message(&mut self, message: Message) {
        if self.message_buffer.is_queued(&message) {
            return;
        }
        self.message_buffer.push(message);
        self.display.pending_update.dirty = true;
        self.dirty = true;
        if self.display.window.has_frame {
            self.display.window.request_redraw();
        }
    }

    /// The checkouts agents in this window's panes work in.
    #[cfg(unix)]
    pub fn agent_checkouts(&self) -> Vec<std::path::PathBuf> {
        self.panes
            .iter()
            .filter(|pane| pane.program_activity.app().is_some())
            .filter_map(|pane| foreground_process_path(pane.master_fd, pane.shell_pid).ok())
            .filter_map(|cwd| {
                crate::worktree::checkout_root(&cwd).map(std::path::Path::to_path_buf)
            })
            .collect()
    }

    /// What runs in each of this window's panes, to open them again on the next start.
    ///
    /// A pane opened again keeps the session it resumed while its agent starts.
    #[cfg(unix)]
    pub fn session_windows(&self, dirs: Option<&AgentDirs>) -> Vec<SessionWindow> {
        let label = self.display.window.label().map(String::from);
        let order = self.layout.panes();
        let panes = order.iter().filter_map(|id| self.panes.iter().find(|pane| pane.id == *id));
        panes
            .map(|pane| {
                // SAFETY: `master_fd` is this pane's PTY, open for the pane's lifetime.
                let pgrp = unsafe { libc::tcgetpgrp(pane.master_fd) };
                let cwd = foreground_process_path(pane.master_fd, pane.shell_pid).ok();

                let in_program = pgrp >= 0 && pgrp as u32 != pane.shell_pid;
                let agent = dirs.filter(|_| in_program).and_then(|dirs| {
                    if let Some(claude) = dirs.claude_session(pgrp, session::process_group) {
                        return Some(claude);
                    }
                    let codex = session::process_name(pgrp).as_deref() == Some("codex")
                        || pane.program_activity.app() == Some("codex");
                    let session_id = cwd.as_deref().and_then(|cwd| dirs.codex_session(cwd));
                    codex.then_some(AgentSession::Codex { session_id })
                });

                match &pane.restored {
                    Some((restored, since))
                        if agent.is_none() && since.elapsed() < RESTORE_GRACE =>
                    {
                        restored.clone()
                    },
                    _ => SessionWindow { cwd, agent, label: label.clone() },
                }
            })
            .collect()
    }

    /// What this window's programs report, as window `id`.
    pub fn window_status(&self, id: u64, focused: bool, now: Instant) -> WindowStatus {
        let title = self.display.window.title();
        let title = match self.display.window.label() {
            Some(label) => format!("{label} · {title}"),
            None => title.to_owned(),
        };
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut status = WindowStatus::new(id, title, focused, self.urgent_activity(), now);
        #[cfg(unix)]
        {
            status.changes = self.turn_changes();
        }
        status
    }

    /// Show `windows` in this window's agent panel, opening it if it isn't yet.
    pub fn refresh_agent_panel(&mut self, windows: Vec<WindowStatus>) {
        let changed = match &mut self.display.agent_panel {
            Some(panel) => panel.update(windows),
            None => {
                let current = u64::from(self.id());
                self.display.agent_panel = Some(AgentPanel::new(current, windows));
                // The panel's keys are single letters; an input method would compose them into
                // text instead of handing them over.
                self.display.window.set_ime_inhibitor(ImeInhibitor::PANEL, true);
                true
            },
        };
        #[cfg(unix)]
        if let Some(panel) = &self.display.agent_panel
            && panel.managed_mode
            && !(panel.managed.preview
                && panel.managed.view == crate::managed_panel::DetailView::Project)
        {
            let inspect = panel.managed.inspect_id();
            let window_id = self.display.window.id();
            self.managed_bridge
                .get_or_insert_with(|| {
                    crate::managed_panel::Bridge::new(self.proxy.clone(), window_id)
                })
                .refresh(inspect, panel.managed.view);
        }
        if changed {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    #[cfg(unix)]
    pub fn managed_update(&mut self, update: crate::managed_panel::ManagedUpdate) {
        if let Some(panel) = &mut self.display.agent_panel {
            panel.managed.apply(update);
            self.dirty = true;
            self.display.window.request_redraw();
        }
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        // Dump grid state.
        let mut grid = self.focused_pane().terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let size_info = &self.display.size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }

    /// Submit the pending changes to the `Display`, and lay out the panes again.
    fn submit_display_update(&mut self, focused_was_searching: bool) {
        // Compute the focused pane's cursor positions before resize.
        let (cursor_at_bottom, origin_at_bottom) = {
            let pane = self.focused_pane();
            let terminal = pane.terminal.lock();
            let num_lines = terminal.screen_lines();
            let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
            let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
                terminal.vi_mode_cursor.point.line == num_lines - 1
            } else {
                pane.search_state.direction == Direction::Left
            };
            (cursor_at_bottom, origin_at_bottom)
        };

        self.display.handle_update(&self.message_buffer, &self.config);
        self.layout_panes();

        let pane = self.focused_pane();
        let is_searching = pane.search_state.history_index.is_some();
        if !focused_was_searching && is_searching {
            // Scroll on search start to make sure origin is visible with minimal viewport motion.
            let mut terminal = pane.terminal.lock();
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }
    }
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        if let Some(metrics) = &self.render_metrics {
            metrics.log(self.id().into());
        }
        // Each pane shuts its PTY down as it's dropped.
    }
}
