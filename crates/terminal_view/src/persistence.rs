use anyhow::{Context as _, Result};
use async_recursion::async_recursion;
use collections::{HashMap, HashSet};
use futures::future::join_all;
use gpui::{AppContext as _, AsyncWindowContext, Axis, Entity, Task, WeakEntity};
use project::Project;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ui::{App, Context, Window};
use util::ResultExt as _;

use db::{
    query,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::{
    ItemId, Member, Pane, PaneAxis, PaneGroup, PersistentItemId, SerializableItem as _,
    SplitDirection, Workspace, WorkspaceDb, WorkspaceId,
};

use crate::{
    ResumableCliBinding, ResumableCliEvidence, ResumableCliProvider, ResumableCliSession,
    TerminalView, default_working_directory,
    terminal_panel::{TerminalPanel, new_terminal_pane},
};

pub(crate) fn serialize_pane_group(
    pane_group: &PaneGroup,
    active_pane: &Entity<Pane>,
    cx: &mut App,
) -> SerializedPaneGroup {
    build_serialized_pane_group(&pane_group.root, active_pane, cx)
}

fn build_serialized_pane_group(
    pane_group: &Member,
    active_pane: &Entity<Pane>,
    cx: &mut App,
) -> SerializedPaneGroup {
    match pane_group {
        Member::Axis(PaneAxis {
            axis,
            members,
            flexes,
            bounding_boxes: _,
        }) => SerializedPaneGroup::Group {
            axis: SerializedAxis(*axis),
            children: members
                .iter()
                .map(|member| build_serialized_pane_group(member, active_pane, cx))
                .collect::<Vec<_>>(),
            flexes: Some(flexes.lock().clone()),
        },
        Member::Pane(pane_handle) => {
            SerializedPaneGroup::Pane(serialize_pane(pane_handle, pane_handle == active_pane, cx))
        }
    }
}

fn serialize_pane(pane: &Entity<Pane>, active: bool, cx: &mut App) -> SerializedPane {
    let mut items_to_serialize = HashSet::default();
    let mut persistent_item_ids = HashMap::default();
    let pane = pane.read(cx);
    let children = pane
        .items()
        .filter_map(|item| {
            let terminal_view = item.act_as::<TerminalView>(cx)?;
            if terminal_view.read(cx).terminal().read(cx).task().is_some() {
                None
            } else {
                let id = item.item_id().as_u64();
                items_to_serialize.insert(id);
                if let Some(persistent_item_id) = terminal_view.read(cx).persistent_item_id() {
                    persistent_item_ids
                        .insert(id.to_string(), persistent_item_id.as_str().to_owned());
                }
                Some(id)
            }
        })
        .collect::<Vec<_>>();
    let active_item = pane
        .active_item()
        .map(|item| item.item_id().as_u64())
        .filter(|active_id| items_to_serialize.contains(active_id));

    let pinned_count = pane.pinned_count();
    SerializedPane {
        active,
        children,
        persistent_item_ids,
        active_item,
        pinned_count,
    }
}

pub(crate) fn deserialize_terminal_panel(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    database_id: WorkspaceId,
    serialized_panel: SerializedTerminalPanel,
    terminal_panel: WeakEntity<TerminalPanel>,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<usize>> {
    window.spawn(cx, async move |cx| {
        let restored_items = match &serialized_panel.items {
            SerializedItems::NoSplits(item_ids) => {
                let items = deserialize_terminal_views(
                    database_id,
                    project,
                    workspace,
                    item_ids.as_slice(),
                    &HashMap::default(),
                    cx,
                )
                .await;
                let restored_items = items.len();
                let active_item = serialized_panel.active_item_id;
                terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                    terminal_panel.active_pane.update(cx, |pane, cx| {
                        populate_pane_items(pane, items, active_item, window, cx);
                    });
                })?;
                restored_items
            }
            SerializedItems::WithSplits(serialized_pane_group) => {
                let center_pane = deserialize_pane_group(
                    workspace,
                    project,
                    terminal_panel.clone(),
                    database_id,
                    serialized_pane_group,
                    cx,
                )
                .await;
                if let Some((center_group, active_pane)) = center_pane {
                    terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                        let interim_panes = terminal_panel
                            .center
                            .panes()
                            .into_iter()
                            .filter(|pane| pane.read(cx).items_len() > 0)
                            .cloned()
                            .collect::<Vec<_>>();
                        let focused_interim_pane = interim_panes
                            .iter()
                            .find(|pane| pane.read(cx).has_focus(window, cx))
                            .cloned();
                        terminal_panel.center = PaneGroup::with_root(center_group);
                        terminal_panel.active_pane =
                            active_pane.unwrap_or_else(|| terminal_panel.center.first_pane());
                        let restored_items = terminal_panel
                            .center
                            .panes()
                            .into_iter()
                            .map(|pane| pane.read(cx).items_len())
                            .sum::<usize>();
                        let restored_pane = terminal_panel.active_pane.clone();
                        for interim_pane in &interim_panes {
                            terminal_panel.center.split(
                                &restored_pane,
                                interim_pane,
                                SplitDirection::Right,
                                cx,
                            );
                        }
                        if let Some(focused_interim_pane) = focused_interim_pane {
                            terminal_panel.active_pane = focused_interim_pane;
                        }
                        restored_items
                    })?
                } else {
                    0
                }
            }
        };

        Ok(restored_items)
    })
}

fn populate_pane_items(
    pane: &mut Pane,
    items: Vec<(u64, Entity<TerminalView>)>,
    active_item: Option<u64>,
    window: &mut Window,
    cx: &mut Context<Pane>,
) {
    let interim_active_item = (pane.items_len() > 0).then(|| pane.active_item()).flatten();
    let mut active_item_index = None;
    for (item_index, (saved_item_id, item)) in (pane.items_len()..).zip(items) {
        if Some(saved_item_id) == active_item {
            active_item_index = Some(item_index);
        }
        pane.add_item(Box::new(item), false, false, None, window, cx);
    }
    if let Some(interim_active_item) = interim_active_item {
        if let Some(index) = pane.index_for_item(interim_active_item.as_ref()) {
            pane.activate_item(index, false, false, window, cx);
        }
    } else if let Some(index) = active_item_index {
        pane.activate_item(index, false, false, window, cx);
    }
}

#[async_recursion(?Send)]
async fn deserialize_pane_group(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: WeakEntity<TerminalPanel>,
    workspace_id: WorkspaceId,
    serialized: &SerializedPaneGroup,
    cx: &mut AsyncWindowContext,
) -> Option<(Member, Option<Entity<Pane>>)> {
    match serialized {
        SerializedPaneGroup::Group {
            axis,
            flexes,
            children,
        } => {
            let mut current_active_pane = None;
            let mut members = Vec::new();
            for child in children {
                if let Some((new_member, active_pane)) = deserialize_pane_group(
                    workspace.clone(),
                    project.clone(),
                    panel.clone(),
                    workspace_id,
                    child,
                    cx,
                )
                .await
                {
                    members.push(new_member);
                    current_active_pane = current_active_pane.or(active_pane);
                }
            }

            if members.is_empty() {
                return None;
            }

            if members.len() == 1 {
                return Some((members.remove(0), current_active_pane));
            }

            Some((
                Member::Axis(PaneAxis::load(axis.0, members, flexes.clone())),
                current_active_pane,
            ))
        }
        SerializedPaneGroup::Pane(serialized_pane) => {
            let active = serialized_pane.active;

            let pane = panel
                .update_in(cx, |terminal_panel, window, cx| {
                    new_terminal_pane(
                        workspace.clone(),
                        project.clone(),
                        terminal_panel.active_pane.read(cx).is_zoomed(),
                        window,
                        cx,
                    )
                })
                .log_err()?;
            let active_item = serialized_pane.active_item;
            let pinned_count = serialized_pane.pinned_count;
            let new_items = deserialize_terminal_views(
                workspace_id,
                project.clone(),
                workspace.clone(),
                serialized_pane.children.as_slice(),
                &serialized_pane.persistent_item_ids,
                cx,
            );
            cx.spawn({
                let pane = pane.downgrade();
                async move |cx| {
                    let new_items = new_items.await;

                    let items = pane.update_in(cx, |pane, window, cx| {
                        populate_pane_items(pane, new_items, active_item, window, cx);
                        pane.set_pinned_count(pinned_count.min(pane.items_len()));
                        pane.items_len()
                    });
                    // Avoid blank panes in splits
                    if items.is_ok_and(|items| items == 0) {
                        let working_directory = workspace
                            .update(cx, |workspace, cx| default_working_directory(workspace, cx))
                            .ok()
                            .flatten();
                        let terminal = project
                            .update(cx, |project, cx| {
                                project.create_terminal_shell(working_directory, cx)
                            })
                            .await
                            .log_err();
                        let Some(terminal) = terminal else {
                            return;
                        };
                        pane.update_in(cx, |pane, window, cx| {
                            let terminal_view = Box::new(cx.new(|cx| {
                                TerminalView::new(
                                    terminal,
                                    workspace.clone(),
                                    Some(workspace_id),
                                    project.downgrade(),
                                    window,
                                    cx,
                                )
                            }));
                            pane.add_item(terminal_view, true, false, None, window, cx);
                        })
                        .ok();
                    }
                }
            })
            .await;
            Some((Member::Pane(pane.clone()), active.then_some(pane)))
        }
    }
}

fn deserialize_terminal_views(
    workspace_id: WorkspaceId,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    item_ids: &[u64],
    persistent_item_ids: &HashMap<String, String>,
    cx: &mut AsyncWindowContext,
) -> impl Future<Output = Vec<(u64, Entity<TerminalView>)>> + use<> {
    let tasks = item_ids.iter().filter_map(|item_id| {
        let item_id = *item_id;
        let persistent_item_id = persistent_item_ids
            .get(&item_id.to_string())
            .map(|id| PersistentItemId::new(id.clone()));
        cx.update(|window, cx| {
            if let Some(persistent_item_id) = persistent_item_id {
                TerminalView::deserialize_with_persistent_item_id(
                    project.clone(),
                    workspace.clone(),
                    workspace_id,
                    item_id,
                    Some(persistent_item_id),
                    window,
                    cx,
                )
            } else {
                TerminalView::deserialize(
                    project.clone(),
                    workspace.clone(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            }
        })
        .ok()
        .map(|task| async move { task.await.map(|view| (item_id, view)) })
    });
    let tasks = join_all(tasks);
    async move {
        tasks
            .await
            .into_iter()
            .filter_map(|result| match result {
                Ok(item) => Some(item),
                Err(error) => {
                    log::error!("failed to restore terminal panel item: {error:#}");
                    None
                }
            })
            .collect()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedTerminalPanel {
    pub items: SerializedItems,
    // A deprecated field, kept for backwards compatibility for the code before terminal splits were introduced.
    pub active_item_id: Option<u64>,
}

impl SerializedTerminalPanel {
    pub(crate) fn persistent_terminal_ids(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<PersistentTerminalId>> {
        fn collect(
            group: &SerializedPaneGroup,
            workspace_id: WorkspaceId,
            ids: &mut Vec<PersistentTerminalId>,
        ) -> Result<()> {
            match group {
                SerializedPaneGroup::Group { children, .. } => {
                    for child in children {
                        collect(child, workspace_id, ids)?;
                    }
                }
                SerializedPaneGroup::Pane(pane) => {
                    for item_id in &pane.children {
                        let id =
                            if let Some(id) = pane.persistent_item_ids.get(&item_id.to_string()) {
                                PersistentTerminalId::from_layout(PersistentItemId::new(id.clone()))
                                    .context("invalid saved dock terminal identity")?
                            } else {
                                PersistentTerminalId::legacy(workspace_id, *item_id)
                            };
                        ids.push(id);
                    }
                }
            }
            Ok(())
        }
        let mut ids = Vec::new();
        match &self.items {
            SerializedItems::NoSplits(items) => ids.extend(
                items
                    .iter()
                    .map(|id| PersistentTerminalId::legacy(workspace_id, *id)),
            ),
            SerializedItems::WithSplits(group) => collect(group, workspace_id, &mut ids)?,
        }
        Ok(ids)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum SerializedItems {
    // The data stored before terminal splits were introduced.
    NoSplits(Vec<u64>),
    WithSplits(SerializedPaneGroup),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SerializedPaneGroup {
    Pane(SerializedPane),
    Group {
        axis: SerializedAxis,
        flexes: Option<Vec<f32>>,
        children: Vec<SerializedPaneGroup>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedPane {
    pub active: bool,
    pub children: Vec<u64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub persistent_item_ids: HashMap<String, String>,
    pub active_item: Option<u64>,
    #[serde(default)]
    pub pinned_count: usize,
}

#[derive(Debug)]
pub(crate) struct SerializedAxis(pub Axis);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum ProjectWorktreeAssociation {
    #[default]
    Unknown,
    Recorded(Option<PathBuf>),
}

impl ProjectWorktreeAssociation {
    fn from_row(row: Option<(bool, Option<PathBuf>)>) -> Self {
        match row {
            Some((true, root)) => Self::Recorded(root),
            _ => Self::Unknown,
        }
    }
    fn into_columns(self) -> (bool, Option<PathBuf>) {
        match self {
            Self::Unknown => (false, None),
            Self::Recorded(root) => (true, root),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PersistentTerminalId(PersistentItemId);

impl PersistentTerminalId {
    pub(crate) fn new() -> Self {
        Self(PersistentItemId::new(uuid::Uuid::new_v4().to_string()))
    }

    pub(crate) fn from_layout(id: PersistentItemId) -> Option<Self> {
        (!id.as_str().is_empty() && !id.as_str().chars().any(char::is_control)).then_some(Self(id))
    }

    pub(crate) fn legacy(workspace_id: WorkspaceId, item_id: ItemId) -> Self {
        Self(PersistentItemId::new(format!(
            "terminal-legacy-v1-{}-{item_id}",
            i64::from(workspace_id)
        )))
    }

    pub(crate) fn as_persistent_item_id(&self) -> PersistentItemId {
        self.0.clone()
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Serialize for SerializedAxis {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Axis::Horizontal => serializer.serialize_str("horizontal"),
            Axis::Vertical => serializer.serialize_str("vertical"),
        }
    }
}

impl<'de> Deserialize<'de> for SerializedAxis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "horizontal" => Ok(SerializedAxis(Axis::Horizontal)),
            "vertical" => Ok(SerializedAxis(Axis::Vertical)),
            invalid => Err(serde::de::Error::custom(format!(
                "Invalid axis value: '{invalid}'"
            ))),
        }
    }
}

pub struct TerminalDb(ThreadSafeConnection);

fn decode_resumable_association(
    provider: Option<String>,
    exact_resume_id: Option<String>,
    resume_cwd: Option<PathBuf>,
    revision: u64,
    evidence: Option<String>,
    provider_home: Option<PathBuf>,
    provider_profile: Option<String>,
) -> Result<ResumableCliBinding> {
    let mut binding = ResumableCliBinding {
        revision,
        session: None,
        pending_codex_cwd: None,
    };
    match (provider, exact_resume_id, resume_cwd, evidence) {
        (None, None, None, None) => {
            anyhow::ensure!(
                provider_home.is_none() && provider_profile.is_none(),
                "unbound terminal has provider context"
            );
        }
        (Some(provider), None, Some(cwd), Some(evidence))
            if provider == "codex" && evidence == "pending" =>
        {
            anyhow::ensure!(
                revision > 0
                    && cwd.is_absolute()
                    && provider_home.is_none()
                    && provider_profile.is_none(),
                "invalid pending Codex association"
            );
            binding.pending_codex_cwd = Some(cwd);
        }
        (Some(provider), Some(id), Some(cwd), Some(evidence)) => {
            anyhow::ensure!(
                revision > 0,
                "resumable CLI session has an invalid zero binding revision"
            );
            binding.session = Some(ResumableCliSession::from_persisted(
                ResumableCliProvider::from_persisted(&provider)?,
                id,
                cwd,
                ResumableCliEvidence::from_persisted(&evidence)?,
                provider_home,
                provider_profile,
            )?);
        }
        _ => anyhow::bail!("terminal has an incomplete resumable CLI association"),
    }
    Ok(binding)
}

impl Domain for TerminalDb {
    const NAME: &str = stringify!(TerminalDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE terminals (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        ),
        // Remove the unique constraint on the item_id table
        // SQLite doesn't have a way of doing this automatically, so
        // we have to do this silly copying.
        sql!(
            CREATE TABLE terminals2 (
                workspace_id INTEGER,
                item_id INTEGER,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;

            INSERT INTO terminals2 (workspace_id, item_id, working_directory)
            SELECT workspace_id, item_id, working_directory FROM terminals;

            DROP TABLE terminals;

            ALTER TABLE terminals2 RENAME TO terminals;
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN working_directory_path TEXT;
            UPDATE terminals SET working_directory_path = CAST(working_directory AS TEXT);
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN custom_title TEXT;
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN resumable_provider TEXT;
            ALTER TABLE terminals ADD COLUMN exact_resume_id TEXT;
            ALTER TABLE terminals ADD COLUMN resume_cwd BLOB;
            ALTER TABLE terminals ADD COLUMN binding_revision INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE terminals ADD COLUMN resumable_evidence TEXT;
        ),
        sql!(
            CREATE TABLE persistent_center_terminals (
                workspace_id INTEGER NOT NULL,
                persistent_item_id TEXT NOT NULL,
                working_directory BLOB,
                working_directory_path TEXT,
                custom_title TEXT,
                resumable_provider TEXT,
                exact_resume_id TEXT,
                resume_cwd BLOB,
                binding_revision INTEGER NOT NULL DEFAULT 0,
                resumable_evidence TEXT,
                PRIMARY KEY(workspace_id, persistent_item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
            ) STRICT;
        ),
        sql!(
            ALTER TABLE terminals ADD COLUMN provider_home BLOB;
            ALTER TABLE terminals ADD COLUMN provider_profile TEXT;
            ALTER TABLE persistent_center_terminals ADD COLUMN provider_home BLOB;
            ALTER TABLE persistent_center_terminals ADD COLUMN provider_profile TEXT;
        ),
        sql!(
            ALTER TABLE terminals ADD COLUMN project_worktree_root BLOB;
            ALTER TABLE terminals ADD COLUMN project_worktree_association_recorded INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE persistent_center_terminals ADD COLUMN project_worktree_root BLOB;
            ALTER TABLE persistent_center_terminals ADD COLUMN project_worktree_association_recorded INTEGER NOT NULL DEFAULT 0;
        ),
    ];
}

db::static_connection!(TerminalDb, [WorkspaceDb]);

impl TerminalDb {
    query! {
        fn project_worktree_association_row(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<(bool, Option<PathBuf>)>> {
            SELECT project_worktree_association_recorded, project_worktree_root FROM terminals WHERE item_id = ? AND workspace_id = ?
        }
    }
    pub(crate) fn project_worktree_association(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
    ) -> Result<ProjectWorktreeAssociation> {
        Ok(ProjectWorktreeAssociation::from_row(
            self.project_worktree_association_row(item_id, workspace_id)?,
        ))
    }
    query! {
        fn persistent_project_worktree_association_row(item_id: &str, workspace_id: WorkspaceId) -> Result<Option<(bool, Option<PathBuf>)>> {
            SELECT project_worktree_association_recorded, project_worktree_root FROM persistent_center_terminals WHERE persistent_item_id = ? AND workspace_id = ?
        }
    }
    pub(crate) fn persistent_project_worktree_association(
        &self,
        item_id: &PersistentTerminalId,
        workspace_id: WorkspaceId,
    ) -> Result<ProjectWorktreeAssociation> {
        Ok(ProjectWorktreeAssociation::from_row(
            self.persistent_project_worktree_association_row(item_id.as_str(), workspace_id)?,
        ))
    }
    pub(crate) async fn record_persistent_project_worktree_association(
        &self,
        item_id: PersistentTerminalId,
        workspace_id: WorkspaceId,
        association: ProjectWorktreeAssociation,
    ) -> Result<()> {
        let ProjectWorktreeAssociation::Recorded(root) = association else {
            return Ok(());
        };
        self.write(move |connection| {
            let mut update = Statement::prepare(connection, "UPDATE persistent_center_terminals SET project_worktree_root = ?3, project_worktree_association_recorded = 1 WHERE workspace_id = ?1 AND persistent_item_id = ?2 AND project_worktree_association_recorded = 0")?;
            update.bind(&workspace_id, 1)?;
            update.bind(&item_id.as_str().to_owned(), 2)?;
            update.bind(&root, 3)?;
            update.exec()
        }).await
    }

    pub async fn migrate_legacy_terminal(
        &self,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        persistent_item_id: PersistentTerminalId,
    ) -> Result<()> {
        self.write(move |conn| {
            let mut copy = Statement::prepare(conn, "INSERT OR IGNORE INTO persistent_center_terminals (
                workspace_id, persistent_item_id, working_directory, working_directory_path, custom_title,
                resumable_provider, exact_resume_id, resume_cwd, binding_revision, resumable_evidence, provider_home, provider_profile, project_worktree_root, project_worktree_association_recorded
            ) SELECT workspace_id, ?3, working_directory, working_directory_path, custom_title,
                resumable_provider, exact_resume_id, resume_cwd, binding_revision, resumable_evidence, provider_home, provider_profile, project_worktree_root, project_worktree_association_recorded
              FROM terminals WHERE workspace_id = ?1 AND item_id = ?2")?;
            copy.bind(&workspace_id, 1)?; copy.bind(&item_id, 2)?; copy.bind(&persistent_item_id.as_str(), 3)?;
            copy.exec()
        }).await
    }

    pub async fn migrate_legacy_center_terminals(&self, workspace_id: WorkspaceId) -> Result<()> {
        self.write(move |conn| {
            conn.with_savepoint("migrate_legacy_center_terminals", || {
                let mut backfill = Statement::prepare(
                    conn,
                    "UPDATE items
                     SET persistent_item_id = 'terminal-legacy-v1-' || workspace_id || '-' || item_id
                     WHERE workspace_id = ?1
                       AND kind = 'Terminal'
                       AND persistent_item_id IS NULL",
                )?;
                backfill.bind(&workspace_id, 1)?;
                backfill.exec()?;

                let mut copy = Statement::prepare(
                    conn,
                    "INSERT OR IGNORE INTO persistent_center_terminals (
                        workspace_id,
                        persistent_item_id,
                        working_directory,
                        working_directory_path,
                        custom_title,
                        resumable_provider,
                        exact_resume_id,
                        resume_cwd,
                        binding_revision,
                        resumable_evidence,
                        provider_home,
                        provider_profile,
                        project_worktree_root,
                        project_worktree_association_recorded
                     )
                     SELECT i.workspace_id,
                            i.persistent_item_id,
                            t.working_directory,
                            t.working_directory_path,
                            t.custom_title,
                            CASE WHEN t.resumable_provider IS NOT NULL
                                      AND (t.exact_resume_id IS NOT NULL OR (t.resumable_provider = 'codex' AND t.resumable_evidence = 'pending'))
                                      AND t.resume_cwd IS NOT NULL
                                      AND t.resumable_evidence IS NOT NULL
                                      AND t.binding_revision > 0
                                 THEN t.resumable_provider END,
                            CASE WHEN t.resumable_provider IS NOT NULL
                                      AND (t.exact_resume_id IS NOT NULL OR (t.resumable_provider = 'codex' AND t.resumable_evidence = 'pending'))
                                      AND t.resume_cwd IS NOT NULL
                                      AND t.resumable_evidence IS NOT NULL
                                      AND t.binding_revision > 0
                                 THEN t.exact_resume_id END,
                            CASE WHEN t.resumable_provider IS NOT NULL
                                      AND (t.exact_resume_id IS NOT NULL OR (t.resumable_provider = 'codex' AND t.resumable_evidence = 'pending'))
                                      AND t.resume_cwd IS NOT NULL
                                      AND t.resumable_evidence IS NOT NULL
                                      AND t.binding_revision > 0
                                 THEN t.resume_cwd END,
                            CASE WHEN t.resumable_provider IS NOT NULL
                                      AND (t.exact_resume_id IS NOT NULL OR (t.resumable_provider = 'codex' AND t.resumable_evidence = 'pending'))
                                      AND t.resume_cwd IS NOT NULL
                                      AND t.resumable_evidence IS NOT NULL
                                      AND t.binding_revision > 0
                                 THEN t.binding_revision ELSE 0 END,
                            CASE WHEN t.resumable_provider IS NOT NULL
                                      AND (t.exact_resume_id IS NOT NULL OR (t.resumable_provider = 'codex' AND t.resumable_evidence = 'pending'))
                                      AND t.resume_cwd IS NOT NULL
                                      AND t.resumable_evidence IS NOT NULL
                                      AND t.binding_revision > 0
                                 THEN t.resumable_evidence END,
                            t.provider_home,
                            t.provider_profile,
                            t.project_worktree_root,
                            COALESCE(t.project_worktree_association_recorded, 0)
                     FROM items i
                     LEFT JOIN terminals t
                       ON t.workspace_id = i.workspace_id AND t.item_id = i.item_id
                     WHERE i.workspace_id = ?1
                       AND i.kind = 'Terminal'
                       AND i.persistent_item_id IS NOT NULL",
                )?;
                copy.bind(&workspace_id, 1)?;
                copy.exec()
            })
        })
        .await
    }

    pub async fn save_persistent_center_snapshot(
        &self,
        persistent_item_id: PersistentTerminalId,
        workspace_id: WorkspaceId,
        working_directory: Option<PathBuf>,
        custom_title: Option<String>,
    ) -> Result<()> {
        self.save_persistent_center_snapshot_with_association(
            persistent_item_id,
            workspace_id,
            working_directory,
            custom_title,
            ProjectWorktreeAssociation::Unknown,
        )
        .await
    }

    pub(crate) async fn save_persistent_center_snapshot_with_association(
        &self,
        persistent_item_id: PersistentTerminalId,
        workspace_id: WorkspaceId,
        working_directory: Option<PathBuf>,
        custom_title: Option<String>,
        project_worktree_association: ProjectWorktreeAssociation,
    ) -> Result<()> {
        let (association_recorded, project_worktree_root) =
            project_worktree_association.into_columns();
        let working_directory_path = working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let persistent_item_id = persistent_item_id.as_str().to_owned();

        self.write(move |conn| {
            let mut snapshot = Statement::prepare(
                conn,
                "INSERT INTO persistent_center_terminals (
                    workspace_id, persistent_item_id, working_directory,
                    working_directory_path, custom_title, project_worktree_root, project_worktree_association_recorded
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (workspace_id, persistent_item_id) DO UPDATE SET
                    working_directory = excluded.working_directory,
                    working_directory_path = excluded.working_directory_path,
                    custom_title = excluded.custom_title,
                    project_worktree_root = CASE WHEN persistent_center_terminals.project_worktree_association_recorded = 0 THEN excluded.project_worktree_root ELSE persistent_center_terminals.project_worktree_root END,
                    project_worktree_association_recorded = MAX(persistent_center_terminals.project_worktree_association_recorded, excluded.project_worktree_association_recorded)",
            )?;
            let mut index = snapshot.bind(&workspace_id, 1)?;
            index = snapshot.bind(&persistent_item_id, index)?;
            index = snapshot.bind(&working_directory, index)?;
            index = snapshot.bind(&working_directory_path, index)?;
            index = snapshot.bind(&custom_title, index)?;
            index = snapshot.bind(&project_worktree_root, index)?;
            snapshot.bind(&association_recorded, index)?;
            snapshot.exec()
        })
        .await
    }

    query! {
        pub fn get_persistent_center_snapshot_row(
            persistent_item_id: &str,
            workspace_id: WorkspaceId
        ) -> Result<Option<(Option<PathBuf>, Option<String>, Option<String>, Option<String>, Option<PathBuf>, i64, Option<String>, Option<PathBuf>, Option<String>)>> {
            SELECT working_directory, custom_title, resumable_provider, exact_resume_id,
                   resume_cwd, binding_revision, resumable_evidence, provider_home, provider_profile
            FROM persistent_center_terminals
            WHERE persistent_item_id = ? AND workspace_id = ?
        }
    }

    pub fn get_persistent_center_snapshot(
        &self,
        persistent_item_id: &PersistentTerminalId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<(Option<PathBuf>, Option<String>, ResumableCliBinding)>> {
        let Some((
            cwd,
            title,
            provider,
            exact_resume_id,
            resume_cwd,
            revision,
            evidence,
            provider_home,
            provider_profile,
        )) = self.get_persistent_center_snapshot_row(persistent_item_id.as_str(), workspace_id)?
        else {
            return Ok(None);
        };
        let revision =
            u64::try_from(revision).context("negative resumable CLI binding revision")?;
        let binding = decode_resumable_association(
            provider,
            exact_resume_id,
            resume_cwd,
            revision,
            evidence,
            provider_home,
            provider_profile,
        )?;
        Ok(Some((cwd, title, binding)))
    }

    pub async fn compare_and_set_persistent_center_resumable_cli_session(
        &self,
        persistent_item_id: PersistentTerminalId,
        workspace_id: WorkspaceId,
        expected_revision: u64,
        session: Option<ResumableCliSession>,
    ) -> Result<Option<u64>> {
        self.compare_and_set_persistent_center_resumable_cli_association(
            persistent_item_id,
            workspace_id,
            expected_revision,
            session,
            None,
        )
        .await
    }

    pub(crate) async fn compare_and_set_persistent_center_resumable_cli_association(
        &self,
        persistent_item_id: PersistentTerminalId,
        workspace_id: WorkspaceId,
        expected_revision: u64,
        session: Option<ResumableCliSession>,
        pending_codex_cwd: Option<PathBuf>,
    ) -> Result<Option<u64>> {
        anyhow::ensure!(
            session.is_none() || pending_codex_cwd.is_none(),
            "agent association cannot be pending and bound"
        );
        if let Some(cwd) = pending_codex_cwd.as_ref() {
            anyhow::ensure!(
                cwd.is_absolute(),
                "pending Codex working directory must be absolute"
            );
        }
        let next_revision = i64::try_from(
            expected_revision
                .checked_add(1)
                .context("resumable CLI binding revision overflow")?,
        )
        .context("resumable CLI binding revision exceeds SQLite INTEGER range")?;
        let expected_revision = i64::try_from(expected_revision)
            .context("resumable CLI binding revision exceeds SQLite INTEGER range")?;
        let (provider, exact_resume_id, resume_cwd, evidence, provider_home, provider_profile) =
            match session {
                Some(session) => (
                    Some(session.provider.as_str().to_owned()),
                    Some(session.exact_resume_id),
                    Some(session.resume_cwd),
                    Some(session.evidence.as_str().to_owned()),
                    session.provider_home,
                    session.provider_profile,
                ),
                None => match pending_codex_cwd {
                    Some(cwd) => (
                        Some("codex".to_owned()),
                        None,
                        Some(cwd),
                        Some("pending".to_owned()),
                        None,
                        None,
                    ),
                    None => (None, None, None, None, None, None),
                },
            };
        let persistent_item_id = persistent_item_id.as_str().to_owned();

        self.write(move |conn| {
            let mut update = Statement::prepare(
                conn,
                "UPDATE persistent_center_terminals
                 SET resumable_provider = ?1, exact_resume_id = ?2, resume_cwd = ?3,
                     binding_revision = ?4, resumable_evidence = ?5,
                     provider_home = ?6, provider_profile = ?7
                 WHERE workspace_id = ?8 AND persistent_item_id = ?9 AND binding_revision = ?10
                 RETURNING binding_revision",
            )?;
            let mut index = update.bind(&provider, 1)?;
            index = update.bind(&exact_resume_id, index)?;
            index = update.bind(&resume_cwd, index)?;
            index = update.bind(&next_revision, index)?;
            index = update.bind(&evidence, index)?;
            index = update.bind(&provider_home, index)?;
            index = update.bind(&provider_profile, index)?;
            index = update.bind(&workspace_id, index)?;
            index = update.bind(&persistent_item_id, index)?;
            update.bind(&expected_revision, index)?;
            if let Some(revision) = update.maybe_row::<i64>()? {
                return Ok(Some(u64::try_from(revision)?));
            }
            if expected_revision != 0 {
                return Ok(None);
            }

            let mut insert = Statement::prepare(
                conn,
                "INSERT OR IGNORE INTO persistent_center_terminals (
                    workspace_id, persistent_item_id, resumable_provider, exact_resume_id,
                    resume_cwd, binding_revision, resumable_evidence, provider_home, provider_profile
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 RETURNING binding_revision",
            )?;
            let mut index = insert.bind(&workspace_id, 1)?;
            index = insert.bind(&persistent_item_id, index)?;
            index = insert.bind(&provider, index)?;
            index = insert.bind(&exact_resume_id, index)?;
            index = insert.bind(&resume_cwd, index)?;
            index = insert.bind(&next_revision, index)?;
            index = insert.bind(&evidence, index)?;
            index = insert.bind(&provider_home, index)?;
            insert.bind(&provider_profile, index)?;
            insert
                .maybe_row::<i64>()?
                .map(u64::try_from)
                .transpose()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_unloaded_persistent_center_terminals(
        &self,
        workspace_id: WorkspaceId,
        alive_items: Vec<PersistentTerminalId>,
    ) -> Result<()> {
        let placeholders = alive_items
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let query = if alive_items.is_empty() {
            "DELETE FROM persistent_center_terminals WHERE workspace_id = ?".to_owned()
        } else {
            format!(
                "DELETE FROM persistent_center_terminals
                 WHERE workspace_id = ? AND persistent_item_id NOT IN ({placeholders})"
            )
        };
        let alive_items = alive_items
            .into_iter()
            .map(|id| id.as_str().to_owned())
            .collect::<Vec<_>>();
        self.write(move |conn| {
            let mut statement = Statement::prepare(conn, query)?;
            let mut index = statement.bind(&workspace_id, 1)?;
            for id in alive_items {
                index = statement.bind(&id, index)?;
            }
            statement.exec()
        })
        .await
    }

    pub async fn save_snapshot(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        working_directory: Option<PathBuf>,
        custom_title: Option<String>,
    ) -> Result<()> {
        self.save_snapshot_with_association(
            item_id,
            workspace_id,
            working_directory,
            custom_title,
            ProjectWorktreeAssociation::Unknown,
        )
        .await
    }

    pub(crate) async fn save_snapshot_with_association(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        working_directory: Option<PathBuf>,
        custom_title: Option<String>,
        project_worktree_association: ProjectWorktreeAssociation,
    ) -> Result<()> {
        let (association_recorded, project_worktree_root) =
            project_worktree_association.into_columns();
        let working_directory_path = working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        self.write(move |conn| {
            let mut snapshot = Statement::prepare(
                conn,
                "INSERT INTO terminals (
                        workspace_id,
                        item_id,
                        working_directory,
                        working_directory_path,
                        custom_title, project_worktree_root, project_worktree_association_recorded
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (workspace_id, item_id) DO UPDATE SET
                    working_directory = excluded.working_directory,
                    working_directory_path = excluded.working_directory_path,
                    custom_title = excluded.custom_title,
                    project_worktree_root = CASE WHEN terminals.project_worktree_association_recorded = 0 THEN excluded.project_worktree_root ELSE terminals.project_worktree_root END,
                    project_worktree_association_recorded = MAX(terminals.project_worktree_association_recorded, excluded.project_worktree_association_recorded)",
            )?;
            let mut index = snapshot.bind(&workspace_id, 1)?;
            index = snapshot.bind(&item_id, index)?;
            index = snapshot.bind(&working_directory, index)?;
            index = snapshot.bind(&working_directory_path, index)?;
            index = snapshot.bind(&custom_title, index)?;
            index = snapshot.bind(&project_worktree_root, index)?;
            snapshot.bind(&association_recorded, index)?;
            snapshot.exec()
        })
        .await
    }

    query! {
       pub async fn update_workspace_id(
            new_id: WorkspaceId,
            old_id: WorkspaceId,
            item_id: ItemId
        ) -> Result<()> {
            UPDATE terminals
            SET workspace_id = ?
            WHERE workspace_id = ? AND item_id = ?
        }
    }

    query! {
       pub async fn update_persistent_center_workspace_id(
            new_id: WorkspaceId,
            old_id: WorkspaceId,
            persistent_item_id: String
        ) -> Result<()> {
            UPDATE persistent_center_terminals
            SET workspace_id = ?
            WHERE workspace_id = ? AND persistent_item_id = ?
        }
    }

    #[cfg(test)]
    pub async fn save_working_directory(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        working_directory: PathBuf,
    ) -> Result<()> {
        log::debug!(
            "Saving working directory {working_directory:?} for item {item_id} in workspace {workspace_id:?}"
        );
        let query =
            "INSERT INTO terminals(item_id, workspace_id, working_directory, working_directory_path)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT DO UPDATE SET
                item_id = ?1,
                workspace_id = ?2,
                working_directory = ?3,
                working_directory_path = ?4"
        ;
        self.write(move |conn| {
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            next_index = statement.bind(&working_directory, next_index)?;
            statement.bind(
                &working_directory.to_string_lossy().into_owned(),
                next_index,
            )?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_working_directory(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
            SELECT working_directory
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    #[cfg(test)]
    pub async fn save_custom_title(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        custom_title: Option<String>,
    ) -> Result<()> {
        log::debug!(
            "Saving custom title {:?} for item {} in workspace {:?}",
            custom_title,
            item_id,
            workspace_id
        );
        self.write(move |conn| {
            let query = "INSERT INTO terminals (item_id, workspace_id, custom_title)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_id, item_id) DO UPDATE SET
                    custom_title = excluded.custom_title";
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            statement.bind(&custom_title, next_index)?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_custom_title(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT custom_title
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    query! {
        fn get_resumable_cli_binding_row(
            item_id: ItemId,
            workspace_id: WorkspaceId
        ) -> Result<Option<(Option<String>, Option<String>, Option<PathBuf>, i64, Option<String>, Option<PathBuf>, Option<String>)>> {
            SELECT resumable_provider, exact_resume_id, resume_cwd, binding_revision,
                   resumable_evidence, provider_home, provider_profile
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub fn get_resumable_cli_binding(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
    ) -> Result<ResumableCliBinding> {
        let Some((
            provider,
            exact_resume_id,
            resume_cwd,
            revision,
            evidence,
            provider_home,
            provider_profile,
        )) = self.get_resumable_cli_binding_row(item_id, workspace_id)?
        else {
            return Ok(ResumableCliBinding::default());
        };
        let revision =
            u64::try_from(revision).context("negative resumable CLI binding revision")?;

        decode_resumable_association(
            provider,
            exact_resume_id,
            resume_cwd,
            revision,
            evidence,
            provider_home,
            provider_profile,
        )
    }

    pub async fn compare_and_set_resumable_cli_session(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        expected_revision: u64,
        session: Option<ResumableCliSession>,
    ) -> Result<Option<u64>> {
        self.compare_and_set_resumable_cli_association(
            item_id,
            workspace_id,
            expected_revision,
            session,
            None,
        )
        .await
    }

    pub(crate) async fn compare_and_set_resumable_cli_association(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        expected_revision: u64,
        session: Option<ResumableCliSession>,
        pending_codex_cwd: Option<PathBuf>,
    ) -> Result<Option<u64>> {
        anyhow::ensure!(
            session.is_none() || pending_codex_cwd.is_none(),
            "agent association cannot be pending and bound"
        );
        if let Some(cwd) = pending_codex_cwd.as_ref() {
            anyhow::ensure!(
                cwd.is_absolute(),
                "pending Codex working directory must be absolute"
            );
        }
        let next_revision = expected_revision
            .checked_add(1)
            .context("resumable CLI binding revision overflow")?;
        let next_revision = i64::try_from(next_revision)
            .context("resumable CLI binding revision exceeds SQLite INTEGER range")?;
        let expected_revision = i64::try_from(expected_revision)
            .context("resumable CLI binding revision exceeds SQLite INTEGER range")?;
        let (provider, exact_resume_id, resume_cwd, evidence, provider_home, provider_profile) =
            match session {
                Some(session) => (
                    Some(session.provider.as_str().to_owned()),
                    Some(session.exact_resume_id),
                    Some(session.resume_cwd),
                    Some(session.evidence.as_str().to_owned()),
                    session.provider_home,
                    session.provider_profile,
                ),
                None => match pending_codex_cwd {
                    Some(cwd) => (
                        Some("codex".to_owned()),
                        None,
                        Some(cwd),
                        Some("pending".to_owned()),
                        None,
                        None,
                    ),
                    None => (None, None, None, None, None, None),
                },
            };

        self.write(move |conn| {
            let mut update = Statement::prepare(
                conn,
                "UPDATE terminals
                 SET resumable_provider = ?1,
                     exact_resume_id = ?2,
                     resume_cwd = ?3,
                     binding_revision = ?4,
                     resumable_evidence = ?5,
                     provider_home = ?6,
                     provider_profile = ?7
                 WHERE workspace_id = ?8 AND item_id = ?9 AND binding_revision = ?10
                 RETURNING binding_revision",
            )?;
            let mut index = update.bind(&provider, 1)?;
            index = update.bind(&exact_resume_id, index)?;
            index = update.bind(&resume_cwd, index)?;
            index = update.bind(&next_revision, index)?;
            index = update.bind(&evidence, index)?;
            index = update.bind(&provider_home, index)?;
            index = update.bind(&provider_profile, index)?;
            index = update.bind(&workspace_id, index)?;
            index = update.bind(&item_id, index)?;
            update.bind(&expected_revision, index)?;
            if let Some(revision) = update.maybe_row::<i64>()? {
                return Ok(Some(u64::try_from(revision)?));
            }

            if expected_revision != 0 {
                return Ok(None);
            }

            let mut insert = Statement::prepare(
                conn,
                "INSERT OR IGNORE INTO terminals (
                    workspace_id,
                    item_id,
                    resumable_provider,
                    exact_resume_id,
                    resume_cwd,
                    binding_revision,
                    resumable_evidence,
                    provider_home,
                    provider_profile
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 RETURNING binding_revision",
            )?;
            let mut index = insert.bind(&workspace_id, 1)?;
            index = insert.bind(&item_id, index)?;
            index = insert.bind(&provider, index)?;
            index = insert.bind(&exact_resume_id, index)?;
            index = insert.bind(&resume_cwd, index)?;
            index = insert.bind(&next_revision, index)?;
            index = insert.bind(&evidence, index)?;
            index = insert.bind(&provider_home, index)?;
            insert.bind(&provider_profile, index)?;
            insert
                .maybe_row::<i64>()?
                .map(u64::try_from)
                .transpose()
                .map_err(Into::into)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::AppDatabase;
    use gpui::TestAppContext;

    fn test_resume_cwd() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\project-a")
        } else {
            PathBuf::from("/project-a")
        }
    }

    async fn test_databases(cx: &mut TestAppContext) -> (TerminalDb, WorkspaceId) {
        cx.update(|cx| cx.set_global(AppDatabase::test_new()));
        let (terminal_db, workspace_db) =
            cx.update(|cx| (TerminalDb::global(cx), WorkspaceDb::global(cx)));
        let workspace_id = workspace_db.next_id().await.unwrap();
        (terminal_db, workspace_id)
    }

    fn session(id: &str) -> ResumableCliSession {
        ResumableCliSession::new(
            ResumableCliProvider::Codex,
            id.to_owned(),
            test_resume_cwd(),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap()
    }

    #[gpui::test]
    async fn pending_codex_round_trips_and_becomes_bound_or_forgotten(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let cwd = test_resume_cwd();
        assert_eq!(
            db.compare_and_set_resumable_cli_association(
                71,
                workspace_id,
                0,
                None,
                Some(cwd.clone())
            )
            .await
            .unwrap(),
            Some(1)
        );
        let pending = db.get_resumable_cli_binding(71, workspace_id).unwrap();
        assert_eq!(pending.pending_codex_cwd(), Some(cwd.as_path()));
        assert!(pending.session().is_none());
        db.save_snapshot(
            71,
            workspace_id,
            Some(cwd.clone()),
            Some("codex pending".into()),
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_resumable_cli_binding(71, workspace_id).unwrap(),
            pending
        );
        let session = session("0199a4f5-1fe0-7000-8000-000000000071");
        assert_eq!(
            db.compare_and_set_resumable_cli_session(71, workspace_id, 0, Some(session.clone()))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db.compare_and_set_resumable_cli_session(71, workspace_id, 1, Some(session.clone()))
                .await
                .unwrap(),
            Some(2)
        );
        let bound = db.get_resumable_cli_binding(71, workspace_id).unwrap();
        assert_eq!(bound.session(), Some(&session));
        assert!(bound.pending_codex_cwd().is_none());
        assert_eq!(
            db.compare_and_set_resumable_cli_association(71, workspace_id, 2, None, Some(cwd))
                .await
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            db.compare_and_set_resumable_cli_session(71, workspace_id, 3, None)
                .await
                .unwrap(),
            Some(4)
        );
        let forgotten = db.get_resumable_cli_binding(71, workspace_id).unwrap();
        assert!(forgotten.session().is_none());
        assert!(forgotten.pending_codex_cwd().is_none());
        assert!(
            db.get_resumable_cli_binding(72, workspace_id)
                .unwrap()
                .pending_codex_cwd()
                .is_none()
        );
    }

    #[gpui::test]
    async fn pending_codex_survives_legacy_promotion_and_stale_snapshots(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let cwd = test_resume_cwd();
        db.save_snapshot(
            73,
            workspace_id,
            Some(cwd.clone()),
            Some("pending centre".into()),
        )
        .await
        .unwrap();
        db.compare_and_set_resumable_cli_association(73, workspace_id, 0, None, Some(cwd.clone()))
            .await
            .unwrap();
        db.write(move |conn| {
            conn.exec_bound(sql!(INSERT INTO panes(pane_id, workspace_id, active) VALUES (?, ?, ?)))?((73_i64, workspace_id, true))?;
            conn.exec_bound(sql!(INSERT INTO items(workspace_id, pane_id, position, kind, item_id, active, preview)
                VALUES (?, ?, ?, ?, ?, ?, ?)))?((workspace_id, 73_i64, 0_i64, "Terminal", 73_u64, true, false))?;
            anyhow::Ok(())
        }).await.unwrap();
        db.migrate_legacy_center_terminals(workspace_id)
            .await
            .unwrap();
        let terminal_id = PersistentTerminalId::legacy(workspace_id, 73);
        let (_, title, pending) = db
            .get_persistent_center_snapshot(&terminal_id, workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(title.as_deref(), Some("pending centre"));
        assert_eq!(pending.pending_codex_cwd(), Some(cwd.as_path()));
        assert_eq!(pending.revision(), 1);
        let session = session("0199a4f5-1fe0-7000-8000-000000000073");
        assert_eq!(
            db.compare_and_set_persistent_center_resumable_cli_session(
                terminal_id.clone(),
                workspace_id,
                1,
                Some(session.clone())
            )
            .await
            .unwrap(),
            Some(2)
        );
        db.migrate_legacy_center_terminals(workspace_id)
            .await
            .unwrap();
        let current = db
            .get_persistent_center_snapshot(&terminal_id, workspace_id)
            .unwrap()
            .unwrap()
            .2;
        assert_eq!(current.session(), Some(&session));
        assert_eq!(current.revision(), 2);
    }

    #[test]
    fn pending_codex_decoder_rejects_ambiguous_partial_rows() {
        let cwd = test_resume_cwd();
        assert!(
            decode_resumable_association(
                Some("codex".into()),
                None,
                Some(cwd.clone()),
                1,
                Some("pending".into()),
                None,
                None
            )
            .is_ok()
        );
        for (provider, evidence, revision) in [
            ("claude", "pending", 1),
            ("codex", "provider_event", 1),
            ("codex", "pending", 0),
        ] {
            assert!(
                decode_resumable_association(
                    Some(provider.into()),
                    None,
                    Some(cwd.clone()),
                    revision,
                    Some(evidence.into()),
                    None,
                    None
                )
                .is_err()
            );
        }
        assert!(
            decode_resumable_association(
                Some("codex".into()),
                None,
                Some(PathBuf::from("relative")),
                1,
                Some("pending".into()),
                None,
                None
            )
            .is_err()
        );
        assert!(
            decode_resumable_association(
                Some("codex".into()),
                None,
                Some(cwd),
                1,
                Some("pending".into()),
                Some(test_resume_cwd()),
                None
            )
            .is_err()
        );
    }

    #[gpui::test]
    async fn test_resumable_cli_binding_compare_and_set(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let item_id = 1;
        let first = session("0199a4f5-1fe0-7000-8000-000000000001");
        let second = session("0199a4f5-1fe0-7000-8000-000000000002");

        assert_eq!(
            db.get_resumable_cli_binding(item_id, workspace_id).unwrap(),
            ResumableCliBinding::default()
        );
        assert_eq!(
            db.compare_and_set_resumable_cli_session(item_id, workspace_id, 0, Some(first.clone()))
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            db.get_resumable_cli_binding(item_id, workspace_id).unwrap(),
            ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 1,
                session: Some(first.clone()),
            }
        );

        assert_eq!(
            db.compare_and_set_resumable_cli_session(
                item_id,
                workspace_id,
                0,
                Some(second.clone())
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            db.compare_and_set_resumable_cli_session(
                item_id,
                workspace_id,
                1,
                Some(second.clone())
            )
            .await
            .unwrap(),
            Some(2)
        );
        assert_eq!(
            db.compare_and_set_resumable_cli_session(item_id, workspace_id, 2, None)
                .await
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            db.get_resumable_cli_binding(item_id, workspace_id).unwrap(),
            ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 3,
                session: None,
            }
        );

        assert_eq!(
            db.compare_and_set_resumable_cli_session(item_id, workspace_id, 2, Some(first))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db.compare_and_set_resumable_cli_session(
                item_id,
                workspace_id,
                3,
                Some(second.clone())
            )
            .await
            .unwrap(),
            Some(4)
        );
        assert_eq!(
            db.get_resumable_cli_binding(item_id, workspace_id)
                .unwrap()
                .session(),
            Some(&second)
        );
    }

    #[gpui::test]
    async fn test_all_provider_bindings_survive_snapshot_updates(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let cwd = std::env::current_dir().unwrap();
        for (index, provider) in [
            ResumableCliProvider::Codex,
            ResumableCliProvider::Claude,
            ResumableCliProvider::Copilot,
            ResumableCliProvider::Hermes,
        ]
        .into_iter()
        .enumerate()
        {
            let session_id = if provider == ResumableCliProvider::Hermes {
                "20261003_120000_a1b2c3".to_owned()
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            let session = ResumableCliSession::new(
                provider,
                session_id,
                cwd.clone(),
                if provider == ResumableCliProvider::Copilot {
                    ResumableCliEvidence::ProviderWorkspace
                } else {
                    ResumableCliEvidence::ProviderEvent
                },
            )
            .unwrap();
            let session = if provider == ResumableCliProvider::Hermes {
                session
                    .with_provider_context(cwd.join("hermes-home"), "work".to_owned())
                    .unwrap()
            } else {
                session
            };
            let terminal_id = PersistentTerminalId::from_layout(PersistentItemId::new(format!(
                "capture-{}",
                provider.as_str()
            )))
            .unwrap();
            let revision = db
                .compare_and_set_persistent_center_resumable_cli_session(
                    terminal_id.clone(),
                    workspace_id,
                    0,
                    Some(session.clone()),
                )
                .await
                .unwrap();
            assert_eq!(revision, Some(1));
            db.save_persistent_center_snapshot(
                terminal_id.clone(),
                workspace_id,
                Some(cwd.join("later-directory")),
                Some("renamed".to_owned()),
            )
            .await
            .unwrap();
            let (saved_cwd, saved_title, binding) = db
                .get_persistent_center_snapshot(&terminal_id, workspace_id)
                .unwrap()
                .unwrap();
            assert_eq!(binding.revision(), 1);
            assert_eq!(binding.session(), Some(&session));
            assert_eq!(saved_cwd, Some(cwd.join("later-directory")));
            assert_eq!(saved_title.as_deref(), Some("renamed"));
            let item_id = index as u64 + 1;
            assert_eq!(
                db.compare_and_set_resumable_cli_session(
                    item_id,
                    workspace_id,
                    0,
                    Some(session.clone())
                )
                .await
                .unwrap(),
                Some(1)
            );
            db.save_snapshot(
                item_id,
                workspace_id,
                Some(cwd.join("later-directory")),
                Some("renamed".to_owned()),
            )
            .await
            .unwrap();
            let binding = db.get_resumable_cli_binding(item_id, workspace_id).unwrap();
            assert_eq!(binding.revision(), 1);
            assert_eq!(binding.session(), Some(&session));
        }
    }

    #[gpui::test]
    async fn test_hermes_provider_context_round_trips(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let provider_home = if cfg!(windows) {
            PathBuf::from(r"C:\Hermes\profiles\work")
        } else {
            PathBuf::from("/hermes/profiles/work")
        };
        let session = ResumableCliSession::new(
            ResumableCliProvider::Hermes,
            "20260922_123456_a1b2c3".into(),
            test_resume_cwd(),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap()
        .with_provider_context(provider_home, "work".into())
        .unwrap();

        assert_eq!(
            db.compare_and_set_resumable_cli_session(1, workspace_id, 0, Some(session.clone()))
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            db.get_resumable_cli_binding(1, workspace_id)
                .unwrap()
                .session(),
            Some(&session)
        );
    }

    #[gpui::test]
    async fn test_stale_snapshot_cannot_roll_back_or_resurrect_resumable_cli_binding(
        cx: &mut TestAppContext,
    ) {
        let (db, workspace_id) = test_databases(cx).await;
        let item_id = 1;
        let cwd = test_resume_cwd();
        let binding_session = session("0199a4f5-1fe0-7000-8000-000000000001");

        db.save_working_directory(item_id, workspace_id, PathBuf::from("stale"))
            .await
            .unwrap();
        db.save_custom_title(item_id, workspace_id, Some("stale".to_owned()))
            .await
            .unwrap();

        // Capture the ordinary snapshot before the binding transition, then complete that stale
        // write afterward. Snapshot writes intentionally contain no resumable-binding columns.
        let stale_snapshot = (Some(cwd.clone()), Some("codex-1".to_owned()));
        assert_eq!(
            db.compare_and_set_resumable_cli_session(
                item_id,
                workspace_id,
                0,
                Some(binding_session.clone())
            )
            .await
            .unwrap(),
            Some(1)
        );
        db.save_snapshot(
            item_id,
            workspace_id,
            stale_snapshot.0.clone(),
            stale_snapshot.1.clone(),
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_working_directory(item_id, workspace_id).unwrap(),
            Some(cwd)
        );
        assert_eq!(
            db.get_custom_title(item_id, workspace_id).unwrap(),
            Some("codex-1".to_owned())
        );
        assert_eq!(
            db.get_resumable_cli_binding(item_id, workspace_id).unwrap(),
            ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 1,
                session: Some(binding_session),
            }
        );

        assert_eq!(
            db.compare_and_set_resumable_cli_session(item_id, workspace_id, 1, None)
                .await
                .unwrap(),
            Some(2)
        );
        db.save_snapshot(item_id, workspace_id, stale_snapshot.0, stale_snapshot.1)
            .await
            .unwrap();
        assert_eq!(
            db.get_resumable_cli_binding(item_id, workspace_id).unwrap(),
            ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 2,
                session: None,
            }
        );
    }

    #[gpui::test]
    async fn test_persistent_center_snapshots_ignore_transient_item_id_collisions(
        cx: &mut TestAppContext,
    ) {
        let (db, workspace_id) = test_databases(cx).await;
        let terminal_a =
            PersistentTerminalId::from_layout(PersistentItemId::new("terminal-a")).unwrap();
        let terminal_b =
            PersistentTerminalId::from_layout(PersistentItemId::new("terminal-b")).unwrap();
        let session_a = session("0199a4f5-1fe0-7000-8000-000000000001");
        let session_b = session("0199a4f5-1fe0-7000-8000-000000000002");

        db.save_persistent_center_snapshot(
            terminal_a.clone(),
            workspace_id,
            Some(PathBuf::from(r"C:\terminal-a")),
            Some("codex-a".to_owned()),
        )
        .await
        .unwrap();
        db.compare_and_set_persistent_center_resumable_cli_session(
            terminal_a.clone(),
            workspace_id,
            0,
            Some(session_a.clone()),
        )
        .await
        .unwrap();
        db.save_persistent_center_snapshot(
            terminal_b.clone(),
            workspace_id,
            Some(PathBuf::from(r"C:\terminal-b")),
            Some("codex-b".to_owned()),
        )
        .await
        .unwrap();
        db.compare_and_set_persistent_center_resumable_cli_session(
            terminal_b.clone(),
            workspace_id,
            0,
            Some(session_b.clone()),
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_persistent_center_snapshot(&terminal_a, workspace_id)
                .unwrap(),
            Some((
                Some(PathBuf::from(r"C:\terminal-a")),
                Some("codex-a".to_owned()),
                ResumableCliBinding {
                    pending_codex_cwd: None,
                    revision: 1,
                    session: Some(session_a),
                },
            ))
        );
        assert_eq!(
            db.get_persistent_center_snapshot(&terminal_b, workspace_id)
                .unwrap()
                .unwrap()
                .2
                .session(),
            Some(&session_b),
        );
    }

    #[gpui::test]
    async fn test_persistent_center_binding_compare_and_set(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let terminal_id =
            PersistentTerminalId::from_layout(PersistentItemId::new("terminal-a")).unwrap();
        let first = session("0199a4f5-1fe0-7000-8000-000000000001");
        let second = session("0199a4f5-1fe0-7000-8000-000000000002");

        assert_eq!(
            db.compare_and_set_persistent_center_resumable_cli_session(
                terminal_id.clone(),
                workspace_id,
                0,
                Some(first.clone())
            )
            .await
            .unwrap(),
            Some(1)
        );
        assert_eq!(
            db.compare_and_set_persistent_center_resumable_cli_session(
                terminal_id.clone(),
                workspace_id,
                0,
                Some(second.clone())
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            db.compare_and_set_persistent_center_resumable_cli_session(
                terminal_id.clone(),
                workspace_id,
                1,
                Some(second.clone())
            )
            .await
            .unwrap(),
            Some(2)
        );
        assert_eq!(
            db.get_persistent_center_snapshot(&terminal_id, workspace_id)
                .unwrap()
                .unwrap()
                .2,
            ResumableCliBinding {
                pending_codex_cwd: None,
                revision: 2,
                session: Some(second),
            }
        );
    }

    #[gpui::test]
    async fn test_persistent_center_cleanup_does_not_delete_panel_rows(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let kept = PersistentTerminalId::from_layout(PersistentItemId::new("kept")).unwrap();
        let orphan = PersistentTerminalId::from_layout(PersistentItemId::new("orphan")).unwrap();
        db.save_snapshot(
            7,
            workspace_id,
            Some(test_resume_cwd()),
            Some("panel".to_owned()),
        )
        .await
        .unwrap();
        for id in [kept.clone(), orphan.clone()] {
            db.save_persistent_center_snapshot(id, workspace_id, Some(test_resume_cwd()), None)
                .await
                .unwrap();
        }

        db.delete_unloaded_persistent_center_terminals(workspace_id, vec![kept.clone()])
            .await
            .unwrap();

        assert!(
            db.get_persistent_center_snapshot(&kept, workspace_id)
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_persistent_center_snapshot(&orphan, workspace_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_custom_title(7, workspace_id).unwrap(),
            Some("panel".to_owned())
        );

        db.delete_unloaded_persistent_center_terminals(workspace_id, Vec::new())
            .await
            .unwrap();
        assert!(
            db.get_persistent_center_snapshot(&kept, workspace_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_custom_title(7, workspace_id).unwrap(),
            Some("panel".to_owned())
        );
    }

    #[gpui::test]
    async fn test_invalid_persisted_resume_id_keeps_terminal_binding(cx: &mut TestAppContext) {
        let (db, workspace_id) = test_databases(cx).await;
        let terminal_id =
            PersistentTerminalId::from_layout(PersistentItemId::new("terminal-a")).unwrap();
        db.save_persistent_center_snapshot(
            terminal_id.clone(),
            workspace_id,
            Some(test_resume_cwd()),
            None,
        )
        .await
        .unwrap();
        db.compare_and_set_persistent_center_resumable_cli_session(
            terminal_id.clone(),
            workspace_id,
            0,
            Some(session("0199a4f5-1fe0-7000-8000-000000000001")),
        )
        .await
        .unwrap();
        let persistent_item_id = terminal_id.as_str().to_owned();
        db.write(move |conn| {
            let mut update = Statement::prepare(
                conn,
                "UPDATE persistent_center_terminals SET exact_resume_id = 'invalid-id'
                 WHERE workspace_id = ?1 AND persistent_item_id = ?2",
            )?;
            let index = update.bind(&workspace_id, 1)?;
            update.bind(&persistent_item_id, index)?;
            update.exec()
        })
        .await
        .unwrap();

        let binding = db
            .get_persistent_center_snapshot(&terminal_id, workspace_id)
            .unwrap()
            .unwrap()
            .2;
        assert_eq!(binding.revision(), 1);
        assert_eq!(binding.session().unwrap().exact_resume_id(), "invalid-id");
        assert!(
            binding
                .session()
                .unwrap()
                .provider()
                .validate_resume_id(binding.session().unwrap().exact_resume_id())
                .is_err()
        );
    }

    #[gpui::test]
    async fn test_legacy_center_terminal_migration_is_batch_and_idempotent(
        cx: &mut TestAppContext,
    ) {
        let (db, workspace_id) = test_databases(cx).await;
        let session_a = session("0199a4f5-1fe0-7000-8000-000000000001");
        let session_b = session("0199a4f5-1fe0-7000-8000-000000000002");
        for (item_id, title, session) in [
            (1, "legacy-a", session_a.clone()),
            (2, "legacy-b", session_b.clone()),
        ] {
            db.save_snapshot_with_association(
                item_id,
                workspace_id,
                Some(PathBuf::from(format!(r"C:\terminal-{item_id}"))),
                Some(title.to_owned()),
                ProjectWorktreeAssociation::Recorded(if item_id == 1 {
                    Some(test_resume_cwd())
                } else {
                    None
                }),
            )
            .await
            .unwrap();
            db.compare_and_set_resumable_cli_session(item_id, workspace_id, 0, Some(session))
                .await
                .unwrap();
        }
        db.write(move |conn| {
            conn.exec_bound(sql!(
                INSERT INTO panes(pane_id, workspace_id, active) VALUES (?, ?, ?)
            ))?((42_i64, workspace_id, true))?;
            let mut insert = conn.exec_bound(sql!(
                INSERT INTO items(workspace_id, pane_id, position, kind, item_id, active, preview)
                VALUES (?, ?, ?, ?, ?, ?, ?)
            ))?;
            insert((workspace_id, 42_i64, 0_i64, "Terminal", 1_u64, true, false))?;
            insert((workspace_id, 42_i64, 1_i64, "Terminal", 2_u64, false, false))?;
            anyhow::Ok(())
        })
        .await
        .unwrap();

        db.migrate_legacy_center_terminals(workspace_id)
            .await
            .unwrap();
        db.migrate_legacy_center_terminals(workspace_id)
            .await
            .unwrap();

        let terminal_a = PersistentTerminalId::legacy(workspace_id, 1);
        let terminal_b = PersistentTerminalId::legacy(workspace_id, 2);
        assert_eq!(
            db.get_persistent_center_snapshot(&terminal_a, workspace_id)
                .unwrap()
                .unwrap()
                .2
                .session(),
            Some(&session_a)
        );
        assert_eq!(
            db.get_persistent_center_snapshot(&terminal_b, workspace_id)
                .unwrap()
                .unwrap()
                .2
                .session(),
            Some(&session_b)
        );
        assert_eq!(
            db.get_custom_title(1, workspace_id).unwrap(),
            Some("legacy-a".to_owned())
        );
        assert_eq!(
            db.get_custom_title(2, workspace_id).unwrap(),
            Some("legacy-b".to_owned())
        );
        assert_eq!(
            db.persistent_project_worktree_association(&terminal_a, workspace_id)
                .unwrap(),
            ProjectWorktreeAssociation::Recorded(Some(test_resume_cwd()))
        );
        assert_eq!(
            db.persistent_project_worktree_association(&terminal_b, workspace_id)
                .unwrap(),
            ProjectWorktreeAssociation::Recorded(None)
        );
    }

    #[gpui::test]
    async fn test_legacy_panel_snapshot_promotes_without_changing_binding(cx: &mut TestAppContext) {
        let (database, workspace_id) = test_databases(cx).await;
        let terminal_id = PersistentTerminalId::legacy(workspace_id, 21);
        let original_session = session("0199a4f5-1fe0-7000-8000-000000000021");
        database
            .save_snapshot(
                21,
                workspace_id,
                Some(test_resume_cwd()),
                Some("legacy panel".into()),
            )
            .await
            .unwrap();
        database
            .compare_and_set_resumable_cli_session(
                21,
                workspace_id,
                0,
                Some(original_session.clone()),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            database
                .migrate_legacy_terminal(workspace_id, 21, terminal_id.clone())
                .await
                .unwrap();
        }
        let snapshot = database
            .get_persistent_center_snapshot(&terminal_id, workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.0, Some(test_resume_cwd()));
        assert_eq!(snapshot.1.as_deref(), Some("legacy panel"));
        assert_eq!(snapshot.2.revision(), 1);
        assert_eq!(snapshot.2.session(), Some(&original_session));
        assert_eq!(
            database
                .get_resumable_cli_binding(21, workspace_id)
                .unwrap()
                .session(),
            Some(&original_session)
        );
        let old_layout: SerializedTerminalPanel = serde_json::from_str(r#"{"items":{"Pane":{"active":true,"children":[21],"active_item":21}},"active_item_id":null}"#).unwrap();
        assert_eq!(
            old_layout.persistent_terminal_ids(workspace_id).unwrap(),
            vec![terminal_id]
        );
    }

    #[test]
    fn test_resumable_cli_session_validation() {
        assert!(
            ResumableCliSession::new(
                ResumableCliProvider::Codex,
                "not-a-uuid".to_owned(),
                test_resume_cwd(),
                ResumableCliEvidence::ProviderEvent,
            )
            .is_err()
        );
        assert!(
            ResumableCliSession::new(
                ResumableCliProvider::Codex,
                "0199a4f5-1fe0-7000-8000-000000000001".to_owned(),
                PathBuf::from("relative"),
                ResumableCliEvidence::ProviderEvent,
            )
            .is_err()
        );
    }
    #[gpui::test]
    async fn test_project_association_states_are_immutable_and_preserve_bindings(
        cx: &mut TestAppContext,
    ) {
        let (database, workspace_id) = test_databases(cx).await;
        for (index, association) in [
            ProjectWorktreeAssociation::Unknown,
            ProjectWorktreeAssociation::Recorded(None),
            ProjectWorktreeAssociation::Recorded(Some(test_resume_cwd())),
        ]
        .into_iter()
        .enumerate()
        {
            let item_id = index as u64 + 220;
            let persistent_id = PersistentTerminalId::legacy(workspace_id, item_id);
            let original_session = session("0199a4f5-1fe0-7000-8000-000000000022");
            database
                .save_snapshot_with_association(
                    item_id,
                    workspace_id,
                    Some(test_resume_cwd()),
                    Some("original".into()),
                    association.clone(),
                )
                .await
                .unwrap();
            database
                .compare_and_set_resumable_cli_session(
                    item_id,
                    workspace_id,
                    0,
                    Some(original_session.clone()),
                )
                .await
                .unwrap();
            assert_eq!(
                database
                    .project_worktree_association(item_id, workspace_id)
                    .unwrap(),
                association
            );
            database
                .migrate_legacy_terminal(workspace_id, item_id, persistent_id.clone())
                .await
                .unwrap();
            assert_eq!(
                database
                    .persistent_project_worktree_association(&persistent_id, workspace_id)
                    .unwrap(),
                association
            );
            for incoming in [
                ProjectWorktreeAssociation::Unknown,
                ProjectWorktreeAssociation::Recorded(Some(test_resume_cwd().join("different"))),
            ] {
                database
                    .save_persistent_center_snapshot_with_association(
                        persistent_id.clone(),
                        workspace_id,
                        Some(test_resume_cwd().join("later")),
                        Some("renamed".into()),
                        incoming.clone(),
                    )
                    .await
                    .unwrap();
                database
                    .save_snapshot_with_association(
                        item_id,
                        workspace_id,
                        Some(test_resume_cwd().join("later")),
                        Some("renamed".into()),
                        incoming,
                    )
                    .await
                    .unwrap();
            }
            let expected = if association == ProjectWorktreeAssociation::Unknown {
                ProjectWorktreeAssociation::Recorded(Some(test_resume_cwd().join("different")))
            } else {
                association
            };
            assert_eq!(
                database
                    .project_worktree_association(item_id, workspace_id)
                    .unwrap(),
                expected
            );
            assert_eq!(
                database
                    .persistent_project_worktree_association(&persistent_id, workspace_id)
                    .unwrap(),
                expected
            );
            let snapshot = database
                .get_persistent_center_snapshot(&persistent_id, workspace_id)
                .unwrap()
                .unwrap();
            assert_eq!(snapshot.0, Some(test_resume_cwd().join("later")));
            assert_eq!(snapshot.1.as_deref(), Some("renamed"));
            assert_eq!(snapshot.2.session(), Some(&original_session));
            assert_eq!(snapshot.2.revision(), 1);
            assert_eq!(
                database
                    .get_resumable_cli_binding(item_id, workspace_id)
                    .unwrap()
                    .session(),
                Some(&original_session)
            );
            database
                .record_persistent_project_worktree_association(
                    persistent_id.clone(),
                    workspace_id,
                    ProjectWorktreeAssociation::Recorded(None),
                )
                .await
                .unwrap();
            assert_eq!(
                database
                    .persistent_project_worktree_association(&persistent_id, workspace_id)
                    .unwrap(),
                expected
            );
        }
    }
}
