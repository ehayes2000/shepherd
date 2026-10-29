mod session_pair;
mod ui;

pub use ui::StatusMessage;
use ui::{
    CreateDialog, DeleteConfirmDialog, HelpPopup, KillConfirmDialog, MainView, QuitConfirmDialog,
    SelectorItemKind, SessionSelector, StatusBar, WorktreeCleanupDialog,
};


use crossterm::ExecutableCommand;
use crossterm::event::DisableMouseCapture;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, backend::CrosstermBackend};

use std::io::{self, Read, stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use crate::core::Core;
use crate::session::{AttachedSession, SharedSize};
use crate::status_socket::EventKind;
use crate::worktree::WorktreeWorkflow;

use std::sync::mpsc::Sender;

use session_pair::{ActivePair, BackgroundPair, SessionActivity};

const BUF_SIZE: usize = 1024;

/// Convert an absolute path to a home-relative path string with `~`.
fn path_to_display(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(suffix) = path.strip_prefix(&home)
    {
        return format!("~/{}", suffix.display());
    }
    path.display().to_string()
}

/// Convert a display path (possibly with `~/`) back to an actual path.
fn display_path_to_actual(path_display: &str) -> PathBuf {
    if let Some(suffix) = path_display.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(suffix);
    }
    PathBuf::from(path_display)
}

const CTRL_H: u8 = 0x08;
const CTRL_N: u8 = 0x0E;
const CTRL_L: u8 = 0x0C;
const CTRL_X: u8 = 0x18;
const CTRL_D: u8 = 0x04;
const CTRL_K: u8 = 0x0B;

#[derive(Default, Clone, PartialEq)]
enum UiMode {
    #[default]
    Normal,
    HelpPopup,
    ListSessions,
    NewSession,
    KillConfirmation,
    QuitConfirmation,
    WorktreeCleanup,
    WorktreeDeleteConfirm,
}

pub struct TuiSessionManager {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: Option<ActivePair>,
    background: Vec<BackgroundPair>,
    size: SharedSize,
    mode: UiMode,
    input_rx: Receiver<Vec<u8>>,
    session_counter: usize,
    core: Core,
    // UI components
    main_view: MainView,
    help_popup: HelpPopup,
    session_selector: SessionSelector,
    create_dialog: CreateDialog,
    kill_confirm_dialog: KillConfirmDialog,
    quit_confirm_dialog: QuitConfirmDialog,
    worktree_cleanup_dialog: WorktreeCleanupDialog,
    delete_confirm_dialog: DeleteConfirmDialog,
    status_bar: StatusBar,
    status_tx: Sender<StatusMessage>,
    /// Original active session name when selector opened (for revert on escape)
    selector_original_session: Option<String>,
    /// Cached session list when selector opened (indices stay consistent during preview)
    selector_sessions: Vec<(String, String)>,
    /// Number of live sessions in selector_sessions
    selector_live_count: usize,
    /// Number of dead sessions in selector_sessions (after live)
    selector_dead_count: usize,
    /// Flag to signal the main loop to exit
    should_quit: bool,
}

impl TuiSessionManager {
    pub fn new() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        stdout().execute(EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout());
        let terminal = Terminal::new(backend)?;

        let term_size = terminal.size()?;
        let size = SharedSize::new(
            term_size.height.saturating_sub(2),
            term_size.width.saturating_sub(2),
        );

        let (input_tx, input_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; BUF_SIZE];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if input_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let core = Core::new()?;
        let (status_bar, status_tx) = StatusBar::new();

        Ok(Self {
            terminal,
            active: None,
            background: Vec::new(),
            size,
            mode: UiMode::Normal,
            input_rx,
            session_counter: 0,
            core,
            main_view: MainView::new(),
            help_popup: HelpPopup::new(),
            session_selector: SessionSelector::new(),
            create_dialog: CreateDialog::new(),
            kill_confirm_dialog: KillConfirmDialog::new(),
            quit_confirm_dialog: QuitConfirmDialog::new(),
            worktree_cleanup_dialog: WorktreeCleanupDialog::new(),
            delete_confirm_dialog: DeleteConfirmDialog::new(),
            status_bar,
            status_tx,
            selector_original_session: None,
            selector_sessions: Vec::new(),
            selector_live_count: 0,
            selector_dead_count: 0,
            should_quit: false,
        })
    }

    fn create_claude_session(
        &self,
        name: &str,
        command: &str,
        args: &[&str],
        cwd: &Path,
    ) -> anyhow::Result<AttachedSession> {
        let (tx, _rx) = mpsc::channel();

        // Build env vars for shepherd hooks integration
        let socket_path = self
            .core
            .status_socket
            .as_ref()
            .map(|s| s.socket_path().to_string_lossy().to_string())
            .unwrap_or_default();

        let env_vars: Vec<(&str, &str)> = if !socket_path.is_empty() {
            vec![
                ("SHEPHERD_SESSION", name),
                ("SHEPHERD_SOCKET", socket_path.as_str()),
            ]
        } else {
            vec![]
        };

        AttachedSession::new_with_env(command, args, tx, self.size.clone(), Some(cwd), &env_vars)
    }

    pub fn add_claude_session(
        &mut self,
        name: &str,
        command: &str,
        args: &[&str],
        cwd: &Path,
        resumed: bool,
    ) -> anyhow::Result<()> {
        let session = self.create_claude_session(name, command, args, cwd)?;

        if let Some(old_pair) = self.active.take() {
            self.background.push(old_pair.detach());
        }

        self.active = Some(ActivePair::new(
            name.to_string(),
            cwd.to_path_buf(),
            session,
            resumed,
        ));

        Ok(())
    }

    pub fn new_named_claude_session(&mut self, name: &str) -> anyhow::Result<()> {
        let metadata = match WorktreeWorkflow::pre_session_hook(name, &self.core.config) {
            Ok(m) => m,
            Err(status_msg) => {
                let _ = self.status_tx.send(status_msg);
                self.mode = UiMode::NewSession;
                return Ok(());
            }
        };

        // Get repo name and project path for history
        if let (Some(repo_name), Some(project_path)) = (
            self.core.get_current_repo_name(),
            self.core.get_current_project_path(),
        ) {
            self.core
                .history
                .set_recent_session(repo_name, name.to_string(), project_path)?;
        }

        let args_owned = self.core.config.claude_args.clone();
        let args: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();
        self.add_claude_session(name, "claude", &args, &metadata.path, false)
    }

    pub fn try_resume(&mut self) -> anyhow::Result<bool> {
        let repo_name = match self.core.get_current_repo_name() {
            Some(r) => r,
            None => return Ok(false),
        };

        let recent = match self.core.history.get_recent_session(&repo_name) {
            Some(r) => r.clone(),
            None => return Ok(false),
        };

        let worktree_path = self.core.worktree_path(&repo_name, &recent.name);

        if !worktree_path.exists() {
            let _ = self.status_tx.send(StatusMessage::err(
                "Resume failed",
                format!("Session path no longer exists: {}", worktree_path.display()),
            ));
            return Ok(false);
        }

        let mut args_owned: Vec<String> = vec!["--continue".to_string()];
        args_owned.extend(self.core.config.claude_args.clone());
        let args: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();

        self.add_claude_session(&recent.name, "claude", &args, &worktree_path, true)?;
        Ok(true)
    }

    pub fn open_new_session(&mut self) {
        self.create_dialog.clear();
        self.mode = UiMode::NewSession;
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        loop {
            if self.should_quit {
                break;
            }

            // Check for dead sessions before rendering
            self.check_dead_sessions();

            // Poll for status events from Claude hooks
            self.poll_status_events();

            let inner_size = self.render_frame()?;
            self.size.set(inner_size.height, inner_size.width);

            match self
                .input_rx
                .recv_timeout(std::time::Duration::from_millis(16))
            {
                Ok(bytes) => {
                    if !self.handle_hotkey(&bytes)? {
                        match self.mode {
                            UiMode::Normal => self.handle_normal_input(&bytes)?,
                            UiMode::HelpPopup => self.handle_help_input(&bytes)?,
                            UiMode::ListSessions => self.handle_list_input(&bytes)?,
                            UiMode::NewSession => self.handle_new_session_input(&bytes)?,
                            UiMode::KillConfirmation => {
                                self.handle_kill_confirmation_input(&bytes)?
                            }
                            UiMode::QuitConfirmation => {
                                self.handle_quit_confirmation_input(&bytes)?
                            }
                            UiMode::WorktreeCleanup => {
                                self.handle_worktree_cleanup_input(&bytes)?
                            }
                            UiMode::WorktreeDeleteConfirm => {
                                self.handle_delete_confirm_input(&bytes)?
                            }
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        Ok(())
    }

    /// Check if the active session has died and handle cleanup
    fn check_dead_sessions(&mut self) {
        // Collect info about dead claude session
        let dead_session_info = if let Some(ref pair) = self.active {
            if pair.claude.is_dead() {
                let error = pair.claude.get_error();
                let log_msg = error.unwrap_or_else(|| "Process exited".to_string());
                let _ = self.status_tx.send(StatusMessage::err(
                    format!("Session {} (claude) died", pair.name),
                    log_msg,
                ));
                Some((pair.name.clone(), pair.path.clone(), pair.resumed))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((name, path, was_resumed)) = dead_session_info {
            // Shutdown and remove the active session
            if let Some(pair) = self.active.take() {
                pair.claude.shutdown();
            }

            // Close any popups and return to normal mode
            if self.mode == UiMode::ListSessions {
                self.mode = UiMode::Normal;
            }

            // If this was a resumed session, start a fresh session in the same directory
            // without the --continue flag
            if was_resumed {
                let args_owned = self.core.config.claude_args.clone();
                let args: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();
                if let Err(e) = self.add_claude_session(&name, "claude", &args, &path, false) {
                    let _ = self.status_tx.send(StatusMessage::err(
                        "Failed to restart session",
                        format!("{}", e),
                    ));
                } else {
                    let _ = self.status_tx.send(StatusMessage::info(
                        "Session restarted",
                        format!("Started fresh session in {}", path.display()),
                    ));
                }
            }
        }
    }

    /// Poll the status socket for events from Claude hooks and update session states
    fn poll_status_events(&mut self) {
        let Some(ref socket) = self.core.status_socket else {
            return;
        };

        let events = socket.poll();
        for event in events {
            let new_activity = match &event.event {
                EventKind::Stop | EventKind::Notification => SessionActivity::Stopped,
                EventKind::ToolStart(tool) => SessionActivity::RunningTool(tool.clone()),
                EventKind::ToolEnd => SessionActivity::Active,
            };

            // Update the activity state for the matching session
            if let Some(ref mut pair) = self.active
                && pair.name == event.session
            {
                pair.activity = new_activity;
                continue;
            }

            // Check background sessions
            for pair in &mut self.background {
                if pair.name == event.session {
                    pair.activity = new_activity;
                    break;
                }
            }
        }
    }

    /// Get count of sessions with stopped activity
    pub fn stopped_session_count(&self) -> usize {
        let active_stopped = self
            .active
            .as_ref()
            .map(|p| p.activity == SessionActivity::Stopped)
            .unwrap_or(false) as usize;

        let bg_stopped = self
            .background
            .iter()
            .filter(|p| p.activity == SessionActivity::Stopped)
            .count();

        active_stopped + bg_stopped
    }

    /// Handle global hotkeys. Returns true if a hotkey was processed.
    fn handle_hotkey(&mut self, bytes: &[u8]) -> anyhow::Result<bool> {
        // Handle global hotkeys
        let hotkey = match bytes {
            [b] if *b == CTRL_H => CTRL_H,
            [b] if *b == CTRL_N => CTRL_N,
            [b] if *b == CTRL_L => CTRL_L,
            [b] if *b == CTRL_X => CTRL_X,
            [b] if *b == CTRL_D => CTRL_D,
            [b] if *b == CTRL_K => CTRL_K,
            _ => return Ok(false),
        };

        // Clean up current mode before switching
        if self.mode == UiMode::NewSession {
            self.create_dialog.clear();
        }

        match hotkey {
            CTRL_H => {
                self.mode = if self.mode == UiMode::HelpPopup {
                    UiMode::Normal
                } else {
                    UiMode::HelpPopup
                };
            }
            CTRL_N => {
                if self.mode != UiMode::NewSession {
                    self.create_dialog.clear();
                    self.mode = UiMode::NewSession;
                }
            }
            CTRL_L => {
                if self.mode == UiMode::ListSessions {
                    self.mode = UiMode::Normal;
                } else {
                    self.open_session_selector();
                    self.mode = UiMode::ListSessions;
                }
            }
            CTRL_X => {
                if self.active.is_some() {
                    if let Some(ref pair) = self.active {
                        self.kill_confirm_dialog.set_session_name(&pair.name);
                    }
                    self.mode = UiMode::KillConfirmation;
                }
            }
            CTRL_D => {
                self.mode = UiMode::QuitConfirmation;
            }
            CTRL_K => {
                if self.mode == UiMode::WorktreeCleanup {
                    self.mode = UiMode::Normal;
                } else {
                    self.open_worktree_cleanup();
                    self.mode = UiMode::WorktreeCleanup;
                }
            }
            _ => return Ok(false),
        }

        Ok(true)
    }

    fn render_frame(&mut self) -> anyhow::Result<ratatui::layout::Rect> {
        // Update status bar (check for new messages, clear expired)
        self.status_bar.update();

        let (screen, scroll_offset) = match &self.active {
            Some(pair) => (Some(pair.claude.get_screen()), pair.scroll_offset),
            None => (None, 0),
        };
        let active_name = self.active.as_ref().map(|p| p.name.clone());
        let active_path = self.active.as_ref().map(|p| p.path.clone());
        let background_count = self.background.len();
        let mode = self.mode.clone();

        // Get status bar render data
        let stopped_count = self.stopped_session_count();
        let bottom_left = self.status_bar.render_bottom_left();
        let bottom_center = self.status_bar.render_bottom_center();

        // Build map of session names to their activity states for selector rendering
        let session_states: std::collections::HashMap<String, SessionActivity> = self
            .active
            .iter()
            .map(|p| (p.name.clone(), p.activity.clone()))
            .chain(
                self.background
                    .iter()
                    .map(|p| (p.name.clone(), p.activity.clone())),
            )
            .collect();

        let mut inner_area = ratatui::layout::Rect::default();

        self.terminal.draw(|frame| {
            let area = frame.area();

            // Render main view (frame/borders)
            let main_inner = self.main_view.render(
                frame,
                screen.as_ref(),
                active_name.as_deref(),
                active_path.as_deref(),
                background_count,
                stopped_count,
                bottom_left,
                bottom_center,
                scroll_offset,
            );
            inner_area = main_inner;

            // Render overlays based on mode
            match mode {
                UiMode::Normal => {}
                UiMode::HelpPopup => {
                    self.help_popup.render(frame, area);
                }
                UiMode::ListSessions => {
                    self.session_selector.render(
                        frame,
                        area,
                        &self.selector_sessions,
                        &session_states,
                    );
                }
                UiMode::NewSession => {
                    self.create_dialog.render(frame, area);
                }
                UiMode::KillConfirmation => {
                    self.kill_confirm_dialog.render(frame, area);
                }
                UiMode::QuitConfirmation => {
                    self.quit_confirm_dialog.render(frame, area);
                }
                UiMode::WorktreeCleanup => {
                    self.worktree_cleanup_dialog.render(frame, area);
                }
                UiMode::WorktreeDeleteConfirm => {
                    self.delete_confirm_dialog.render(frame, area);
                }
            }
        })?;

        Ok(inner_area)
    }

    /// Check if bytes contain mouse events (SGR or legacy format).
    /// Returns true if the bytes are entirely mouse events that should not be forwarded to PTY.
    fn is_mouse_event(bytes: &[u8]) -> bool {
        // Check if all content is mouse events (may be multiple concatenated)
        let mut pos = 0;
        while pos < bytes.len() {
            // SGR mouse mode: ESC [ < ... M or ESC [ < ... m
            if bytes[pos..].starts_with(b"\x1b[<") {
                // Find the terminating M or m
                if let Some(end) = bytes[pos..].iter().position(|&b| b == b'M' || b == b'm') {
                    pos += end + 1;
                    continue;
                }
            }

            // Legacy mouse mode: ESC [ M followed by 3 bytes
            if bytes[pos..].len() >= 6 && bytes[pos..].starts_with(b"\x1b[M") {
                pos += 6;
                continue;
            }

            // Not a mouse event
            return false;
        }

        // All bytes were mouse events
        pos > 0
    }

    /// Parse mouse scroll events from escape sequences.
    /// Returns Some(lines) where positive = scroll up, negative = scroll down.
    /// Handles multiple concatenated events and sums up scroll deltas.
    /// Returns None if no scroll events found.
    fn parse_scroll_event(bytes: &[u8]) -> Option<i32> {
        let mut total_delta = 0i32;
        let mut pos = 0;

        while pos < bytes.len() {
            // SGR mouse mode: ESC [ < Ps ; Px ; Py M (or m for release)
            if bytes[pos..].starts_with(b"\x1b[<") {
                // Find the end of this event (M or m)
                if let Some(end_offset) = bytes[pos..].iter().position(|&b| b == b'M' || b == b'm')
                {
                    let event = &bytes[pos..pos + end_offset + 1];

                    // Parse the button code (between '<' and first ';')
                    if let Some(semi_pos) = event[3..].iter().position(|&b| b == b';')
                        && let Ok(button_str) = std::str::from_utf8(&event[3..3 + semi_pos])
                        && let Ok(button) = button_str.parse::<u8>()
                    {
                        // Button 64 = scroll up, 65 = scroll down
                        let base_button = button & 0b11000011;
                        match base_button {
                            64 => total_delta += 1, // scroll up
                            65 => total_delta -= 1, // scroll down
                            _ => {}
                        }
                    }

                    pos += end_offset + 1;
                    continue;
                }
            }

            // Legacy mouse mode: ESC [ M Cb Cx Cy
            if bytes[pos..].len() >= 6 && bytes[pos..].starts_with(b"\x1b[M") {
                let button = bytes[pos + 3];
                let base_button = button & 0b11000011;
                match base_button {
                    96 => total_delta += 1, // scroll up
                    97 => total_delta -= 1, // scroll down
                    _ => {}
                }
                pos += 6;
                continue;
            }

            // Not a recognized event at this position
            break;
        }

        if total_delta != 0 {
            Some(total_delta)
        } else {
            None
        }
    }

    fn handle_normal_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if self.active.is_none() {
            return Ok(());
        }

        // Handle scroll events - adjust scroll offset instead of forwarding to PTY
        if let Some(scroll_delta) = Self::parse_scroll_event(bytes) {
            if let Some(ref mut pair) = self.active {
                // vt100 will clamp the scrollback position to the actual scrollback buffer size
                // The max is SCROLLBACK (1000) lines from session.rs
                const MAX_SCROLLBACK: usize = 1000;

                if scroll_delta > 0 {
                    // Scroll up (show older content)
                    pair.scroll_offset =
                        (pair.scroll_offset + scroll_delta as usize).min(MAX_SCROLLBACK);
                } else {
                    // Scroll down (show newer content)
                    let abs_delta = (-scroll_delta) as usize;
                    pair.scroll_offset = pair.scroll_offset.saturating_sub(abs_delta);
                }
            }
            return Ok(());
        }

        // Filter out all other mouse events (clicks, motion, etc.) - don't forward to PTY
        if Self::is_mouse_event(bytes) {
            return Ok(());
        }

        // Any other input resets scroll to bottom
        if let Some(ref mut pair) = self.active {
            pair.scroll_offset = 0;
        }

        if let Some(ref mut pair) = self.active {
            if pair.claude.is_dead() {
                return Ok(());
            }
            // Clear stopped state when user interacts with session
            pair.activity = SessionActivity::Active;
            // Ignore write errors - check_dead_sessions will handle cleanup
            let _ = pair.claude.write_input(bytes);
        }
        Ok(())
    }

    fn handle_help_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        // Any non-hotkey key closes help
        if !bytes.is_empty() {
            self.mode = UiMode::Normal;
        }
        Ok(())
    }

    fn handle_kill_confirmation_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        match bytes[0] {
            // Escape key
            0x1b if bytes.len() == 1 => {
                self.mode = UiMode::Normal;
            }
            // 'y' or 'Y' - confirm kill
            b'y' | b'Y' => {
                if let Some(pair) = self.active.take() {
                    let name = pair.name.clone();
                    pair.claude.shutdown();

                    let _ = self.status_tx.send(StatusMessage::info(
                        "Session killed",
                        format!("Killed session '{}'", name),
                    ));
                }
                self.mode = UiMode::Normal;
            }
            // 'n' or 'N' or any other key - cancel
            b'n' | b'N' => {
                self.mode = UiMode::Normal;
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_quit_confirmation_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        match bytes[0] {
            // Escape key
            0x1b if bytes.len() == 1 => {
                self.mode = UiMode::Normal;
            }
            // 'y' or 'Y' - confirm quit
            b'y' | b'Y' => {
                self.should_quit = true;
            }
            // 'n' or 'N' - cancel
            b'n' | b'N' => {
                self.mode = UiMode::Normal;
            }
            _ => {}
        }

        Ok(())
    }

    fn open_session_selector(&mut self) {
        self.session_selector.reset();

        // Save original active session name for revert on escape
        self.selector_original_session = self.active.as_ref().map(|p| p.name.clone());

        // Active session is at index 0 if it exists
        if self.active.is_some() {
            self.session_selector.set_active_index(Some(0));
        }

        // Cache session list (indices remain consistent during preview)
        let (sessions, live_count, dead_count) = self.build_session_list();
        self.selector_sessions = sessions;
        self.selector_live_count = live_count;
        self.selector_dead_count = dead_count;
        self.session_selector.set_counts(live_count, dead_count);
        self.session_selector.update_filter(&self.selector_sessions);
    }

    /// Build session list with live sessions first, then dead sessions.
    /// Returns (list, live_count, dead_count).
    fn build_session_list(&self) -> (Vec<(String, String)>, usize, usize) {
        // Collect live sessions first
        let live: Vec<(String, String)> = self
            .active
            .iter()
            .map(|p| (p.name.clone(), path_to_display(&p.path)))
            .chain(
                self.background
                    .iter()
                    .map(|p| (p.name.clone(), path_to_display(&p.path))),
            )
            .collect();

        let live_count = live.len();

        // Collect paths that are currently live (to filter out from recent/worktrees)
        let live_paths: std::collections::HashSet<_> = self
            .active
            .iter()
            .map(|p| p.path.clone())
            .chain(self.background.iter().map(|p| p.path.clone()))
            .collect();

        // Collect recent sessions from history that aren't currently live
        let repo_name = self.core.get_current_repo_name();
        let recent_items: Vec<(String, String)> = repo_name
            .as_ref()
            .map(|rn| {
                self.core
                    .history
                    .get_recent_sessions(rn)
                    .map(|s| (s.name.clone(), self.core.worktree_path(rn, &s.name)))
                    .filter(|(_, path)| !live_paths.contains(path))
                    .map(|(name, path)| (name, path_to_display(&path)))
                    .collect()
            })
            .unwrap_or_default();

        // Collect worktree directories that aren't currently live or recent
        let recent_paths: std::collections::HashSet<_> = repo_name
            .as_ref()
            .map(|rn| {
                self.core
                    .history
                    .get_recent_sessions(rn)
                    .map(|s| self.core.worktree_path(rn, &s.name))
                    .collect()
            })
            .unwrap_or_default();

        let worktree_items: Vec<(String, String)> = self
            .core
            .list_worktree_dirs()
            .into_iter()
            .filter(|path| !live_paths.contains(path) && !recent_paths.contains(path))
            .map(|path| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unnamed".to_string());
                (name, path_to_display(&path))
            })
            .collect();

        let mut list = live;
        list.extend(recent_items);
        list.extend(worktree_items);

        let dead_count = list.len().saturating_sub(live_count);
        (list, live_count, dead_count)
    }

    fn handle_list_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        // Handle escape sequences (arrows, escape key)
        if bytes[0] == 0x1b {
            if bytes.len() == 1 {
                // Escape key - revert to original session and close
                if let Some(ref original_name) = self.selector_original_session.clone() {
                    self.switch_to_session_by_name(original_name)?;
                }
                self.mode = UiMode::Normal;
                return Ok(());
            }
            if bytes.len() >= 3 && bytes[1] == b'[' {
                match bytes[2] {
                    b'A' => {
                        self.session_selector.move_up();
                        self.preview_selected_session()?;
                    }
                    b'B' => {
                        self.session_selector.move_down();
                        self.preview_selected_session()?;
                    }
                    _ => {}
                }
            }
            return Ok(());
        }

        match bytes[0] {
            b'\r' | b'\n' => {
                // Enter - confirm selection based on item kind
                match self.session_selector.selected_kind() {
                    Some(SelectorItemKind::Live) => {
                        // Live session - already previewed, just close
                    }
                    Some(SelectorItemKind::Dead) => {
                        // Dead session - resume it
                        if let Some(selected) = self.session_selector.selected_original_index()
                            && let Some((name, path_display)) =
                                self.selector_sessions.get(selected).cloned()
                        {
                            self.resume_recent_session(&name, &path_display)?;
                        }
                    }
                    None => {}
                }
                self.mode = UiMode::Normal;
            }
            0x7f => {
                // Backspace - remove character from filter
                self.session_selector.pop_char();
                self.session_selector.update_filter(&self.selector_sessions);
                self.preview_selected_session()?;
            }
            b if b.is_ascii_graphic() || b == b' ' => {
                // Printable character - add to filter
                self.session_selector.push_char(b as char);
                self.session_selector.update_filter(&self.selector_sessions);
                self.preview_selected_session()?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Preview the currently selected session (switch to it without closing selector).
    /// Only previews live sessions, not recent or worktree items.
    fn preview_selected_session(&mut self) -> anyhow::Result<()> {
        // Only preview live sessions
        if self.session_selector.selected_kind() != Some(SelectorItemKind::Live) {
            return Ok(());
        }

        if let Some(selected) = self.session_selector.selected_original_index() {
            // Get the name from cached session list (indices are stable)
            if let Some((name, _)) = self.selector_sessions.get(selected).cloned() {
                self.switch_to_session_by_name(&name)?;
            }
        }
        Ok(())
    }

    /// Resume a recent session from history.
    fn resume_recent_session(&mut self, name: &str, path_display: &str) -> anyhow::Result<()> {
        // Convert display path back to actual path
        let path = display_path_to_actual(path_display);

        // Check if path still exists
        if !path.exists() {
            let _ = self.status_tx.send(StatusMessage::err(
                "Path not found",
                format!("Session path no longer exists: {}", path.display()),
            ));
            return Ok(());
        }

        // Resume with --continue flag
        let mut args_owned: Vec<String> = vec!["--continue".to_string()];
        args_owned.extend(self.core.config.claude_args.clone());
        let args: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();
        self.add_claude_session(name, "claude", &args, &path, true)?;

        let _ = self.status_tx.send(StatusMessage::info(
            "Resumed session",
            format!("Resumed '{}' from history", name),
        ));

        Ok(())
    }

    /// Switch to a session by name, searching both active and background.
    /// Returns true if the session was found and switched to.
    fn switch_to_session_by_name(&mut self, name: &str) -> anyhow::Result<bool> {
        // Check if already active
        if let Some(ref active) = self.active
            && active.name == name
        {
            return Ok(true);
        }

        // Find in background
        let bg_index = self.background.iter().position(|p| p.name == name);

        if let Some(idx) = bg_index {
            let bg_pair = self.background.remove(idx);

            if let Some(old_pair) = self.active.take() {
                self.background.push(old_pair.detach());
            }

            self.active = Some(bg_pair.attach()?);

            return Ok(true);
        }

        Ok(false)
    }

    fn handle_new_session_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        if bytes[0] == 0x1b && bytes.len() == 1 {
            self.create_dialog.clear();
            self.mode = UiMode::Normal;
            return Ok(());
        }

        match bytes[0] {
            b'\r' | b'\n' => {
                let input = self.create_dialog.take_input();
                let name = if input.trim().is_empty() {
                    self.session_counter += 1;
                    format!("claude-{}", self.session_counter)
                } else {
                    input.trim().to_string()
                };
                self.new_named_claude_session(&name)?;
                self.mode = UiMode::Normal;
            }
            0x7f => {
                self.create_dialog.pop();
            }
            b if b.is_ascii_graphic() || b == b' ' => {
                self.create_dialog.push(b as char);
            }
            _ => {}
        }

        Ok(())
    }

    /// Open the worktree cleanup dialog
    fn open_worktree_cleanup(&mut self) {
        self.worktree_cleanup_dialog.reset();
        let worktrees = self.core.list_worktree_dirs();
        let active_paths = self.get_active_session_paths();
        self.worktree_cleanup_dialog
            .set_worktrees_with_active(worktrees, active_paths);
    }

    /// Get paths of all active/background sessions.
    fn get_active_session_paths(&self) -> std::collections::HashSet<PathBuf> {
        self.active
            .iter()
            .map(|p| p.path.clone())
            .chain(self.background.iter().map(|p| p.path.clone()))
            .collect()
    }

    /// Handle input in worktree cleanup mode
    fn handle_worktree_cleanup_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        // Handle escape sequences (arrows, escape key)
        if bytes[0] == 0x1b {
            if bytes.len() == 1 {
                // Escape - close dialog
                self.mode = UiMode::Normal;
                return Ok(());
            }
            if bytes.len() >= 3 && bytes[1] == b'[' {
                match bytes[2] {
                    b'A' => self.worktree_cleanup_dialog.move_up(),
                    b'B' => self.worktree_cleanup_dialog.move_down(),
                    _ => {}
                }
            }
            return Ok(());
        }

        match bytes[0] {
            b'\r' | b'\n' => {
                // Enter - toggle selection
                self.worktree_cleanup_dialog.toggle_selection();
            }
            b'd' => {
                // Delete selected, or current item if nothing selected
                let to_delete = if self.worktree_cleanup_dialog.has_selections() {
                    self.worktree_cleanup_dialog.get_selected_worktrees()
                } else {
                    self.worktree_cleanup_dialog
                        .get_current_worktree()
                        .into_iter()
                        .collect()
                };
                if !to_delete.is_empty() {
                    let active_paths = self.get_active_session_paths();
                    self.delete_confirm_dialog
                        .set_worktrees_with_active(to_delete, active_paths);
                    self.mode = UiMode::WorktreeDeleteConfirm;
                }
            }
            0x7f => {
                // Backspace - remove character from filter
                self.worktree_cleanup_dialog.pop_char();
                self.worktree_cleanup_dialog.update_filter();
            }
            b if b.is_ascii_graphic() || b == b' ' => {
                // Printable character - add to filter
                self.worktree_cleanup_dialog.push_char(b as char);
                self.worktree_cleanup_dialog.update_filter();
            }
            _ => {}
        }

        Ok(())
    }

    /// Handle input in delete confirmation mode
    fn handle_delete_confirm_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        match bytes[0] {
            0x1b if bytes.len() == 1 => {
                // Escape - cancel, return to cleanup dialog
                self.mode = UiMode::WorktreeCleanup;
            }
            b'y' | b'Y' => {
                // Confirm - delete worktrees
                self.delete_selected_worktrees()?;
            }
            b'n' | b'N' => {
                // Cancel - return to cleanup dialog
                self.mode = UiMode::WorktreeCleanup;
            }
            _ => {}
        }

        Ok(())
    }

    /// Delete selected worktrees
    fn delete_selected_worktrees(&mut self) -> anyhow::Result<()> {
        let worktrees = self.delete_confirm_dialog.get_worktrees().to_vec();
        let active_paths = self.delete_confirm_dialog.get_active_paths().clone();
        let mut deleted_count = 0;
        let mut errors = Vec::new();

        // First, kill any active sessions for worktrees being deleted
        for worktree_path in &worktrees {
            if active_paths.contains(worktree_path) {
                self.kill_session_at_path(worktree_path);
            }
        }

        // Now delete the worktrees
        let repo_name = self.core.get_current_repo_name();
        for worktree_path in &worktrees {
            match self.core.delete_worktree(worktree_path) {
                Ok(()) => {
                    deleted_count += 1;
                    // Remove from history - extract session name from path
                    if let (Some(rn), Some(session_name)) = (
                        &repo_name,
                        worktree_path.file_name().and_then(|n| n.to_str()),
                    ) {
                        self.core.history.remove_by_name(rn, session_name);
                    }
                }
                Err(e) => {
                    errors.push(format!("{}: {}", worktree_path.display(), e));
                }
            }
        }

        // Save history after all deletions
        let _ = self.core.history.save();

        // Show status message
        if errors.is_empty() {
            let _ = self.status_tx.send(StatusMessage::info(
                format!("Deleted {} worktree(s)", deleted_count),
                format!("Successfully deleted {} worktree(s)", deleted_count),
            ));
        } else {
            let _ = self.status_tx.send(StatusMessage::err(
                format!(
                    "Deleted {} of {} worktree(s)",
                    deleted_count,
                    worktrees.len()
                ),
                errors.join("; "),
            ));
        }

        // Refresh the worktree list
        let remaining = self.core.list_worktree_dirs();
        let active_paths = self.get_active_session_paths();
        self.worktree_cleanup_dialog
            .set_worktrees_with_active(remaining, active_paths);

        // Return to cleanup mode if worktrees remain, otherwise normal
        if self.worktree_cleanup_dialog.is_empty() {
            self.mode = UiMode::Normal;
        } else {
            self.mode = UiMode::WorktreeCleanup;
        }

        Ok(())
    }

    /// Kill a session at the given path (active or background)
    fn kill_session_at_path(&mut self, path: &Path) {
        // Check if it's the active session
        if let Some(ref pair) = self.active
            && pair.path == path
        {
            if let Some(pair) = self.active.take() {
                pair.claude.shutdown();
            }
            return;
        }

        // Check background sessions
        if let Some(idx) = self.background.iter().position(|p| p.path == path) {
            self.background.remove(idx);

            // Note: BackgroundPair doesn't have a shutdown method, but dropping it should clean up
        }
    }

}

impl Drop for TuiSessionManager {
    fn drop(&mut self) {
        let _ = stdout().execute(DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
    }
}
