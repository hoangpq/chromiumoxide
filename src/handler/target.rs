use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures::channel::oneshot::Sender;
use futures::stream::Stream;
use futures::task::{Context, Poll};

use chromiumoxide_types::{Command, Method, Request, Response};

use crate::cmd::CommandChain;
use crate::cmd::CommandMessage;
use crate::error::{CdpError, DeadlineExceeded, Result};
use crate::handler::emulation::EmulationManager;
use crate::handler::frame::FrameNavigationRequest;
use crate::handler::frame::{
    FrameEvent, FrameManager, NavigationError, NavigationId, NavigationOk,
};
use crate::handler::network::NetworkManager;
use crate::handler::page::PageHandle;
use crate::handler::viewport::Viewport;
use crate::handler::PageInner;
use crate::page::Page;
use chromiumoxide_cdp::cdp::browser_protocol::page::{FrameId, GetFrameTreeParams};
use chromiumoxide_cdp::cdp::browser_protocol::{
    browser::BrowserContextId,
    log as cdplog, performance,
    target::{AttachToTargetParams, SessionId, SetAutoAttachParams, TargetId, TargetInfo},
};
use chromiumoxide_cdp::cdp::events::CdpEvent;
use chromiumoxide_cdp::cdp::CdpEventMessage;

macro_rules! advance_state {
    ($s:ident, $cx:ident, $now:ident, $cmds: ident, $next_state:expr ) => {{
        if let Poll::Ready(poll) = $cmds.poll($now) {
            return match poll {
                None => {
                    $s.init_state = $next_state;
                    $s.poll($cx, $now)
                }
                Some(Ok((method, params))) => Some(TargetEvent::Request(Request {
                    method,
                    session_id: $s.session_id.clone().map(Into::into),
                    params,
                })),
                Some(Err(err)) => Some(TargetEvent::RequestTimeout(err)),
            };
        } else {
            return None;
        }
    }};
}

#[derive(Debug)]
pub struct Target {
    /// Info about this target as returned from the chromium instance
    info: TargetInfo,
    /// Whether this target was marked as closed
    is_closed: bool,
    /// The frame manager that maintains the state of all frames and handles
    /// navigations of frames
    frame_manager: FrameManager,
    network_manager: NetworkManager,
    emulation_manager: EmulationManager,
    viewport: Viewport,
    /// The identifier of the session this target is attached to
    session_id: Option<SessionId>,
    /// The handle of the browser page of this target
    page: Option<PageHandle>,
    /// Drives this target towards initialization
    init_state: TargetInit,
    /// Currently queued events to report to the `Handler`
    queued_events: VecDeque<TargetEvent>,
    /// Senders that need to be notified once the main frame has loaded
    wait_until_frame_loaded: Vec<Sender<Result<String>>>,
    /// The sender who requested the page.
    initiator: Option<Sender<Result<Page>>>,
    /// Used to tracked whether this target should initialize its state
    initialize: bool,
}

impl Target {
    /// Create a new target instance with `TargetInfo` after a
    /// `CreateTargetParams` request.
    pub fn new(info: TargetInfo) -> Self {
        Self {
            info,
            is_closed: false,
            frame_manager: Default::default(),
            network_manager: Default::default(),
            emulation_manager: Default::default(),
            viewport: Default::default(),
            session_id: None,
            page: None,
            init_state: TargetInit::AttachToTarget,
            wait_until_frame_loaded: Default::default(),
            queued_events: Default::default(),
            initiator: None,
            initialize: false,
        }
    }

    pub fn set_session_id(&mut self, id: SessionId) {
        self.session_id = Some(id)
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    pub fn session_id_mut(&mut self) -> &mut Option<SessionId> {
        &mut self.session_id
    }

    /// The identifier for this target
    pub fn target_id(&self) -> &TargetId {
        &self.info.target_id
    }

    /// Whether this target is already initialized
    pub fn is_initialized(&self) -> bool {
        matches!(self.init_state, TargetInit::Initialized)
    }

    pub fn goto(&mut self, req: FrameNavigationRequest) {
        self.frame_manager.goto(req)
    }

    fn create_page(&mut self) {
        if self.page.is_none() {
            if let Some(session) = self.session_id.clone() {
                let handle = PageHandle::new(self.target_id().clone(), session);
                self.page = Some(handle);
            }
        }
    }

    pub(crate) fn get_or_create_page(&mut self) -> Option<&Arc<PageInner>> {
        self.create_page();
        self.page.as_ref().map(|p| p.inner())
    }

    pub fn is_page(&self) -> bool {
        todo!()
    }

    pub fn browser_context_id(&self) -> Option<&BrowserContextId> {
        self.info.browser_context_id.as_ref()
    }

    pub fn info(&self) -> &TargetInfo {
        &self.info
    }

    /// Get the target that opened this target. Top-level targets return `None`.
    pub fn opener(&self) -> Option<&TargetId> {
        self.info.opener_id.as_ref()
    }

    pub fn frame_manager_mut(&mut self) -> &mut FrameManager {
        &mut self.frame_manager
    }

    /// Received a response to a command issued by this target

    pub fn on_response(&mut self, resp: Response, method: &str) {
        if let Some(cmds) = self.init_state.commands_mut() {
            cmds.received_response(method);
        }
        #[allow(clippy::single_match)] // allow for now
        match method {
            GetFrameTreeParams::IDENTIFIER => {
                if let Some(resp) = resp
                    .result
                    .and_then(|val| GetFrameTreeParams::response_from_value(val).ok())
                {
                    self.frame_manager.on_frame_tree(resp.frame_tree);
                }
            }
            _ => {}
        }
    }

    pub fn on_event(&mut self, event: CdpEventMessage) {
        match event.params {
            // `FrameManager` events
            CdpEvent::PageFrameAttached(ev) => self
                .frame_manager
                .on_frame_attached(ev.frame_id.clone(), Some(ev.parent_frame_id)),
            CdpEvent::PageFrameDetached(ev) => self.frame_manager.on_frame_detached(&ev),
            CdpEvent::PageFrameNavigated(ev) => self.frame_manager.on_frame_navigated(ev.frame),
            CdpEvent::PageNavigatedWithinDocument(ev) => {
                self.frame_manager.on_frame_navigated_within_document(&ev)
            }
            CdpEvent::RuntimeExecutionContextCreated(ev) => {
                self.frame_manager.on_frame_execution_context_created(&ev)
            }
            CdpEvent::RuntimeExecutionContextDestroyed(ev) => {
                self.frame_manager.on_frame_execution_context_destroyed(&ev)
            }
            CdpEvent::RuntimeExecutionContextsCleared(ev) => {
                self.frame_manager.on_execution_context_cleared(&ev)
            }
            CdpEvent::PageLifecycleEvent(ev) => self.frame_manager.on_page_lifecycle_event(&ev),
            CdpEvent::PageFrameStartedLoading(ev) => {
                self.frame_manager.on_frame_started_loading(&ev);
            }

            // `NetworkManager` events
            CdpEvent::FetchRequestPaused(ev) => self.network_manager.on_fetch_request_paused(&*ev),
            CdpEvent::FetchAuthRequired(ev) => self.network_manager.on_fetch_auth_required(&*ev),
            CdpEvent::NetworkRequestWillBeSent(ev) => {
                self.network_manager.on_request_will_be_sent(&*ev)
            }
            CdpEvent::NetworkRequestServedFromCache(ev) => {
                self.network_manager.on_request_served_from_cache(&ev)
            }
            CdpEvent::NetworkResponseReceived(ev) => {
                self.network_manager.on_response_received(&*ev)
            }
            CdpEvent::NetworkLoadingFinished(ev) => {
                self.network_manager.on_network_loading_finished(&ev)
            }
            CdpEvent::NetworkLoadingFailed(ev) => {
                self.network_manager.on_network_loading_failed(&ev)
            }
            _ => {}
        }
    }

    /// Advance that target's state
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>, now: Instant) -> Option<TargetEvent> {
        if !self.initialize {
            return None;
        }
        match &mut self.init_state {
            TargetInit::AttachToTarget => {
                self.init_state = TargetInit::InitializingFrame(FrameManager::init_commands());
                let params = AttachToTargetParams::builder()
                    .target_id(self.target_id().clone())
                    .flatten(true)
                    .build()
                    .unwrap();

                return Some(TargetEvent::Request(Request::new(
                    params.identifier(),
                    serde_json::to_value(params).unwrap(),
                )));
            }
            TargetInit::InitializingFrame(cmds) => {
                self.session_id.as_ref()?;
                advance_state!(
                    self,
                    cx,
                    now,
                    cmds,
                    TargetInit::InitializingNetwork(self.network_manager.init_commands())
                );
            }
            TargetInit::InitializingNetwork(cmds) => {
                advance_state!(
                    self,
                    cx,
                    now,
                    cmds,
                    TargetInit::InitializingPage(Self::page_init_commands())
                );
            }
            TargetInit::InitializingPage(cmds) => {
                advance_state!(
                    self,
                    cx,
                    now,
                    cmds,
                    TargetInit::InitializingEmulation(
                        self.emulation_manager.init_commands(&self.viewport),
                    )
                );
            }
            TargetInit::InitializingEmulation(cmds) => {
                advance_state!(self, cx, now, cmds, TargetInit::Initialized);
            }
            TargetInit::Initialized => {
                if let Some(initiator) = self.initiator.take() {
                    // make sure that the main frame of the page has finished loading
                    if self
                        .frame_manager
                        .main_frame()
                        .map(|frame| frame.is_loaded())
                        .unwrap_or_default()
                    {
                        if let Some(page) = self.get_or_create_page() {
                            let _ = initiator.send(Ok(page.clone().into()));
                        } else {
                            self.initiator = Some(initiator);
                        }
                    } else {
                        self.initiator = Some(initiator);
                    }
                }
            }
        };
        loop {
            if let Some(frame) = self.frame_manager.main_frame() {
                if frame.is_loaded() {
                    while let Some(tx) = self.wait_until_frame_loaded.pop() {
                        let _ = tx.send(frame.url.clone().ok_or(CdpError::NotFound));
                    }
                }
            }

            // Drain queued messages first.
            if let Some(ev) = self.queued_events.pop_front() {
                return Some(ev);
            }

            if let Some(handle) = self.page.as_mut() {
                while let Poll::Ready(Some(msg)) = Pin::new(&mut handle.rx).poll_next(cx) {
                    match msg {
                        TargetMessage::Command(cmd) => {
                            self.queued_events.push_back(TargetEvent::Command(cmd));
                        }
                        TargetMessage::MainFrame(tx) => {
                            let _ = tx.send(self.frame_manager.main_frame().map(|f| f.id.clone()));
                        }
                        TargetMessage::Url(tx) => {
                            let _ = tx
                                .send(self.frame_manager.main_frame().and_then(|f| f.url.clone()));
                        }
                        TargetMessage::WaitForNavigation(tx) => {
                            if let Some(frame) = self.frame_manager.main_frame() {
                                if frame.is_loaded() {
                                    let _ = tx.send(frame.url.clone().ok_or(CdpError::NotFound));
                                } else {
                                    self.wait_until_frame_loaded.push(tx);
                                }
                            } else {
                                self.wait_until_frame_loaded.push(tx);
                            }
                        }
                    }
                }
            }

            while let Some(event) = self.frame_manager.poll(now) {
                match event {
                    FrameEvent::NavigationResult(res) => {
                        self.queued_events
                            .push_back(TargetEvent::NavigationResult(res));
                    }
                    FrameEvent::NavigationRequest(id, req) => {
                        self.queued_events
                            .push_back(TargetEvent::NavigationRequest(id, req));
                    }
                }
            }

            if self.queued_events.is_empty() {
                return None;
            }
        }
    }

    /// Set the sender half of the channel who requested the creation of this
    /// target
    pub fn set_initiator(&mut self, tx: Sender<Result<Page>>) {
        self.initiator = Some(tx);
        self.initialize();
    }

    /// Start with the initialization process
    pub fn initialize(&mut self) {
        self.initialize = true;
    }

    // TODO move to other location
    pub(crate) fn page_init_commands() -> CommandChain {
        let attach = SetAutoAttachParams::builder()
            .flatten(true)
            .auto_attach(true)
            .wait_for_debugger_on_start(true)
            .build()
            .unwrap();
        let enable_performance = performance::EnableParams::default();
        let enable_log = cdplog::EnableParams::default();
        CommandChain::new(vec![
            (attach.identifier(), serde_json::to_value(attach).unwrap()),
            (
                enable_performance.identifier(),
                serde_json::to_value(enable_performance).unwrap(),
            ),
            (
                enable_log.identifier(),
                serde_json::to_value(enable_log).unwrap(),
            ),
        ])
    }
}

#[derive(Debug)]
pub(crate) enum TargetEvent {
    /// An internal request
    Request(Request),
    /// An internal navigation request
    NavigationRequest(NavigationId, Request),
    /// Indicates that a previous requested navigation has finished
    NavigationResult(Result<NavigationOk, NavigationError>),
    /// An internal request timed out
    RequestTimeout(DeadlineExceeded),
    /// A new command arrived via a channel
    Command(CommandMessage),
}

// TODO this can be moved into the classes?
#[derive(Debug)]
pub enum TargetInit {
    InitializingFrame(CommandChain),
    InitializingNetwork(CommandChain),
    InitializingPage(CommandChain),
    InitializingEmulation(CommandChain),
    AttachToTarget,
    Initialized,
}

impl TargetInit {
    fn commands_mut(&mut self) -> Option<&mut CommandChain> {
        match self {
            TargetInit::InitializingFrame(cmd) => Some(cmd),
            TargetInit::InitializingNetwork(cmd) => Some(cmd),
            TargetInit::InitializingPage(cmd) => Some(cmd),
            TargetInit::InitializingEmulation(cmd) => Some(cmd),
            TargetInit::AttachToTarget => None,
            TargetInit::Initialized => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum TargetMessage {
    /// Execute a command within the session of this target
    Command(CommandMessage),
    /// Return the main frame of this target
    MainFrame(Sender<Option<FrameId>>),
    /// Return the url of this target's page
    Url(Sender<Option<String>>),
    /// A Message that resolves when the frame finished loading a new url
    WaitForNavigation(Sender<Result<String>>),
}
