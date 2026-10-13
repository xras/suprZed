use crate::{
    AgentPanel, ConversationView, NewCenterThread,
    thread_metadata_store::{ThreadId, ThreadMetadataStore},
};
use agent_settings::AgentSettings;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Subscription, WeakEntity, Window, rgba,
};
use project::{DisableAiSettings, ProjectPath};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Color, ContextMenu, Icon, IconName, Indicator, prelude::*, utils::WithRemSize};
use workspace::{Event as WorkspaceEvent, Item, Workspace, item::ItemEvent};

pub(crate) fn center_menu_entries(
    menu: ContextMenu,
    workspace: &WeakEntity<Workspace>,
    _window: &mut Window,
    cx: &mut App,
) -> ContextMenu {
    let can_create_thread = !DisableAiSettings::get_global(cx).disable_ai
        && AgentSettings::get_global(cx).enabled
        && workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .panel::<AgentPanel>(cx)
                .is_some_and(|panel| panel.read(cx).has_open_project(cx))
        });

    if can_create_thread {
        menu.action("New Agent Thread", NewCenterThread.boxed_clone())
    } else {
        menu
    }
}

pub struct ThreadItem {
    conversation_view: Entity<ConversationView>,
    workspace: WeakEntity<Workspace>,
    last_tab_title: SharedString,
    discarded: bool,
    _subscriptions: Vec<Subscription>,
}

impl ThreadItem {
    pub(crate) fn deploy(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            return;
        };
        if !panel.read(cx).has_open_project(cx) {
            return;
        }
        let agent = panel.read(cx).selected_agent(cx);
        if workspace.project().read(cx).is_via_collab() && !agent.is_native() {
            return;
        }
        Self::open(workspace, agent, None, true, window, cx);
    }

    pub fn open(
        workspace: &mut Workspace,
        agent: crate::Agent,
        thread_id: Option<ThreadId>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            return;
        };
        let view = panel.update(cx, |panel, cx| {
            panel.create_center_thread(agent, thread_id, window, cx)
        });
        let item = cx.new(|cx| Self::new(view, workspace.weak_handle(), cx));
        workspace.add_item_to_active_pane(Box::new(item), None, focus, window, cx);
    }

    pub(crate) fn new(
        conversation_view: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let last_tab_title = conversation_view.read(cx).title(cx);
        let mut this = Self {
            conversation_view: conversation_view.clone(),
            workspace,
            last_tab_title,
            discarded: false,
            _subscriptions: Vec::new(),
        };
        this._subscriptions
            .push(cx.observe(&conversation_view, |this, view, cx| {
                let title = view.read(cx).title(cx);
                if title != this.last_tab_title {
                    this.last_tab_title = title;
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }));
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            this._subscriptions
                .push(cx.observe(&store, |this, _store, cx| {
                    let title = this.conversation_view.read(cx).title(cx);
                    if title != this.last_tab_title {
                        this.last_tab_title = title;
                    }
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }));
        }
        this
    }

    pub fn thread_id(&self, cx: &App) -> ThreadId {
        self.conversation_view.read(cx).thread_id
    }

    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.conversation_view
    }

    fn project_path_for_work_dirs(&self, cx: &App) -> Option<ProjectPath> {
        let work_dirs = self.conversation_view.read(cx).root_work_dirs(cx)?;
        let project = self.conversation_view.read(cx).project().clone();
        let project = project.read(cx);

        work_dirs
            .ordered_paths()
            .find_map(|work_dir| project.find_project_path(work_dir, cx))
    }

    pub(crate) fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.discarded = true;
        let item = cx.entity();
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                if let Some(pane) = workspace.pane_for(&item) {
                    pane.update(cx, |pane, cx| {
                        pane.remove_item(item.entity_id(), false, true, window, cx);
                    });
                }
            });
        });
    }
}

impl EventEmitter<ItemEvent> for ThreadItem {}

impl Focusable for ThreadItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.read(cx).activation_focus_handle(cx)
    }
}

impl Render for ThreadItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_settings = ThemeSettings::get_global(cx);
        WithRemSize::new(theme_settings.agent_ui_font_size(cx))
            .size_full()
            .font_family(theme_settings.agent_ui_font_family().clone())
            .child(self.conversation_view.clone())
    }
}

impl Item for ThreadItem {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation_view.read(cx).title(cx)
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        let icon = self
            .conversation_view
            .read(cx)
            .agent_key()
            .icon()
            .unwrap_or(IconName::ZedAgent);
        Some(Icon::new(icon).color(Color::Muted))
    }

    fn tab_indicator(&self, _cx: &App) -> Option<Indicator> {
        Some(Indicator::bar().color(Color::Custom(rgba(0x3aafa9b3).into())))
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.project_path_for_work_dirs(cx)
    }

    fn affects_workspace_theme(&self, _: &App) -> bool {
        false
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Thread Opened")
    }

    fn include_in_nav_history() -> bool {
        false
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace_entity) = self.workspace.upgrade() {
            self._subscriptions.push(cx.subscribe_in(
                &workspace_entity,
                window,
                |this, _workspace, event: &WorkspaceEvent, _window, cx| {
                    if matches!(
                        event,
                        WorkspaceEvent::ActiveItemChanged | WorkspaceEvent::ZoomChanged
                    ) {
                        this.conversation_view.update(cx, |view, cx| {
                            view.dismiss_notifications_if_visible(_window, cx);
                        });
                    }
                },
            ));
        }
        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
            let thread_id = self.thread_id(cx);
            let item = cx.weak_entity();
            panel.update(cx, |panel, _cx| {
                panel.register_center_thread(thread_id, item)
            });
        }
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        if self.discarded {
            return;
        }
        let conversation_view = self.conversation_view.clone();
        let item_id = cx.entity_id();
        let workspace = self.workspace.clone();
        App::defer(cx, move |cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            if workspace
                .read(cx)
                .items_of_type::<ThreadItem>(cx)
                .any(|item| item.entity_id() == item_id)
            {
                return;
            }
            if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.retain_closed_center_thread(conversation_view, cx)
                });
            }
        });
    }
}
