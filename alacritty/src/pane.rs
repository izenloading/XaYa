//! A terminal in a window: its grid, the shell's PTY and what the shell's programs report.
//!
//! A window shows one pane, or several side by side or stacked, see [`crate::layout`].

use std::error::Error;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Instant;

use log::info;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use alacritty_terminal::event::OnResize;
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;

use crate::cli::TerminalOptions;
use crate::config::UiConfig;
use crate::display::SizeInfo;
use crate::event::{Event, EventProxy, InlineSearchState, SearchState};
use crate::layout::PaneId;
use crate::program_status::ProgramActivity;
#[cfg(unix)]
use crate::session::SessionWindow;
#[cfg(unix)]
use crate::turn_changes::TurnTracker;

/// One terminal of a window.
pub struct Pane {
    pub id: PaneId,
    pub terminal: Arc<FairMutex<Term<EventProxy>>>,
    pub notifier: Notifier,
    #[cfg(not(windows))]
    pub master_fd: RawFd,
    #[cfg(not(windows))]
    pub shell_pid: u32,
    pub program_activity: ProgramActivity,
    /// What its agent's turns change in the checkout.
    #[cfg(unix)]
    pub turn_tracker: TurnTracker,
    pub search_state: SearchState,
    pub inline_search_state: InlineSearchState,
    /// Where the pane is in the window, and how many cells it has.
    pub size_info: SizeInfo,
    /// The title its programs set, while another pane's shows.
    pub title: Option<String>,
    /// The window from the last session this pane opened again, and when.
    #[cfg(unix)]
    pub restored: Option<(SessionWindow, Instant)>,
    /// Command typed once the shell is ready, such as one resuming the pane's agent.
    #[cfg(unix)]
    pub pending_input: Option<String>,
}

impl Pane {
    /// Start a shell in pane `id` of window `window_id`, `size_info` large.
    pub fn new(
        id: PaneId,
        config: &UiConfig,
        terminal_options: &TerminalOptions,
        size_info: SizeInfo,
        proxy: EventLoopProxy<Event>,
        window_id: WindowId,
    ) -> Result<Self, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        terminal_options.override_pty_config(&mut pty_config);

        info!("PTY dimensions: {:?} x {:?}", size_info.screen_lines(), size_info.columns());

        let event_proxy = EventProxy::new(proxy, window_id, id);

        // Create the terminal.
        //
        // This object contains all of the state about what's being displayed. It's
        // wrapped in a clonable mutex since both the I/O loop and display need to
        // access it.
        let terminal = Term::new(config.term_options(), &size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        // Create the PTY.
        //
        // The PTY forks a process to run the shell on the slave side of the
        // pseudoterminal. A file descriptor for the master side is retained for
        // reading/writing to the shell.
        let pty = tty::new(&pty_config, size_info.into(), window_id.into())?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        // Create the pseudoterminal I/O loop.
        //
        // PTY I/O is ran on another thread as to not occupy cycles used by the
        // renderer and input processing. Note that access to the terminal state is
        // synchronized since the I/O loop updates the state, and the display
        // consumes it periodically.
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy,
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
        )?;

        // The event loop channel allows write requests from the event processor
        // to be sent to the pty loop and ultimately written to the pty.
        let loop_tx = event_loop.channel();

        // Kick off the I/O thread.
        let _io_thread = event_loop.spawn();

        Ok(Self {
            id,
            terminal,
            notifier: Notifier(loop_tx),
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            program_activity: Default::default(),
            #[cfg(unix)]
            turn_tracker: Default::default(),
            search_state: Default::default(),
            inline_search_state: Default::default(),
            size_info,
            title: None,
            #[cfg(unix)]
            restored: None,
            #[cfg(unix)]
            pending_input: None,
        })
    }

    /// Move the pane to `size_info`, resizing its terminal and PTY when its cells change.
    pub fn resize(&mut self, size_info: SizeInfo) {
        if self.size_info.screen_lines() != size_info.screen_lines()
            || self.size_info.columns() != size_info.columns()
        {
            self.notifier.on_resize(size_info.into());
            self.terminal.lock().resize(size_info);
        }

        if self.size_info != size_info {
            self.search_state.clear_focused_match();
        }
        self.size_info = size_info;
    }

    /// Whether the pane covers the window position `x`, `y`.
    pub fn contains(&self, x: usize, y: usize) -> bool {
        let size = &self.size_info;
        let (x, y) = (x as f32, y as f32);
        let bottom = size.padding_y()
            + size.grid_height().max(size.cell_height() * size.screen_lines() as f32);
        x >= size.padding_x()
            && x < size.padding_x() + size.grid_width()
            && y >= size.padding_y()
            && y < bottom
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // Shutdown the terminal's PTY.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}
