//! A tab for browsing Prime Agent's saved data. Everything comes from
//! `prime-agent data … --json`, so it shows exactly what that command shows:
//! saved sessions (chat, thinking, tool calls, usage, events, raw records),
//! the harness memory refinements build up (memories, prompts, skills,
//! subagents and the refinements themselves) and the research log (run
//! metadata, model calls and harness decisions), filtered by text and date.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Local, TimeDelta};
use editor::Editor;
use gpui::{
    AnyElement, App, ClickEvent, Context, DragMoveEvent, Entity, EventEmitter, FocusHandle,
    Focusable, ListAlignment, ListState, Pixels, Render, SharedString, Subscription, Task, Window,
    list, prelude::*, px,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use ui::{
    ButtonStyle, ContextMenu, DropdownMenu, IconButton, IconName, IconPosition, ScrollAxes,
    Scrollbars, Tooltip, WithScrollbar, prelude::*,
};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent};

/// How long typing in a search box waits before querying.
const SEARCH_DELAY: Duration = Duration::from_millis(250);
/// Lines of an item shown until it is clicked open.
const COLLAPSED_LINES: usize = 8;
const DEFAULT_LIST_WIDTH: Pixels = px(360.);
const MIN_LIST_WIDTH: Pixels = px(220.);
/// Room always left for the item column when the list is dragged wider.
const MIN_ITEMS_WIDTH: Pixels = px(320.);

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionRow {
    id: String,
    short_id: String,
    cwd: String,
    title: String,
    created: String,
    model: Option<String>,
    message_count: usize,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HarnessScope {
    id: String,
    short_id: String,
    title: String,
    path: PathBuf,
    counts: HashMap<String, usize>,
    updated: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResearchRow {
    id: String,
    short_id: String,
    title: String,
    path: PathBuf,
    acp_session_id: Option<String>,
    runs: usize,
    model_calls: usize,
    errors: usize,
    corrections: usize,
    verifications: usize,
    cost: f64,
    harness_versions: Vec<String>,
    last: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct DataItem {
    line: usize,
    timestamp: String,
    kind: String,
    label: String,
    text: String,
}

/// `data show`, `data harness <scope>` and `data research <id>` all answer
/// with item counts by kind and the selected items.
#[derive(Clone, Debug, Deserialize)]
struct ItemsData {
    counts: HashMap<String, usize>,
    items: Vec<DataItem>,
}

/// One entry of the left-hand list: a session, a harness memory scope, or a
/// session's research log.
#[derive(Clone, Debug)]
struct Row {
    id: String,
    title: String,
    /// Created (sessions) or last updated (harness memory, research log); the
    /// date filter reads it.
    when: Option<DateTime<Local>>,
    details: String,
    footer: String,
    /// The line under the title in the item column.
    header: String,
}

impl Row {
    fn from_session(session: SessionRow) -> Self {
        let when = local_time(&session.created);
        let shown_when = format_when(when, &session.created);
        let model = session.model.as_deref().unwrap_or("unknown model");
        Self {
            details: format!(
                "{shown_when} · {} msgs · {}/{} tokens · ${:.2}",
                session.message_count, session.input_tokens, session.output_tokens, session.cost
            ),
            footer: format!("{} · {model}", session.short_id),
            header: format!(
                "{} · {model} · {shown_when} · {} · {}/{} tokens · ${:.4}",
                session.short_id,
                session.cwd,
                session.input_tokens,
                session.output_tokens,
                session.cost
            ),
            id: session.id,
            title: session.title,
            when,
        }
    }

    fn from_scope(scope: HarnessScope) -> Self {
        let when = scope.updated.as_deref().and_then(local_time);
        let shown_when = scope.updated.as_deref().map_or_else(
            || "never updated".to_string(),
            |updated| format_when(when, updated),
        );
        let count = |kind: &str| scope.counts.get(kind).copied().unwrap_or(0);
        Self {
            details: format!(
                "{} memories · {} refinements · {shown_when}",
                count("memory"),
                count("refinement")
            ),
            footer: format!(
                "{} · {} prompts · {} skills · {} subagents",
                scope.short_id,
                count("prompt"),
                count("skill"),
                count("subagent")
            ),
            header: format!("{} · {}", scope.short_id, scope.path.display()),
            id: scope.id,
            title: scope.title,
            when,
        }
    }

    fn from_research(log: ResearchRow) -> Self {
        let when = log.last.as_deref().and_then(local_time);
        let shown_when = log
            .last
            .as_deref()
            .map_or_else(String::new, |last| format_when(when, last));
        let acp = log.acp_session_id.as_deref().unwrap_or("-");
        Self {
            details: format!(
                "{shown_when} · {} calls · {} errors · {} corrections · ${:.4}",
                log.model_calls, log.errors, log.corrections, log.cost
            ),
            footer: format!(
                "{} · {} runs · {} checks · {}",
                log.short_id,
                log.runs,
                log.verifications,
                log.harness_versions.join(", ")
            ),
            header: format!(
                "{} · ACP session {acp} · {}",
                log.short_id,
                log.path.display()
            ),
            id: log.id,
            title: log.title,
            when,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Sessions,
    Harness,
    Research,
}

impl Source {
    /// The `--kind` values the source's command takes, with their labels.
    /// The first is always everything.
    fn kinds(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Sessions => &[
                ("all", "Everything"),
                ("chat", "Chat"),
                ("thinking", "Thinking"),
                ("tools", "Tool calls"),
                ("usage", "Usage"),
                ("events", "Events"),
                ("raw", "Raw JSON"),
            ],
            Self::Harness => &[
                ("all", "Everything"),
                ("memory", "Memories"),
                ("prompt", "Prompts"),
                ("skill", "Skills"),
                ("subagent", "Subagents"),
                ("factory", "Factory"),
                ("refinement", "Refinements"),
            ],
            // Research `--kind` values match the start of an event type.
            Self::Research => &[
                ("all", "Everything"),
                ("run", "Run start and end"),
                ("user", "Prompts"),
                ("context", "Context built / compacted"),
                ("api", "Model calls and retries"),
                ("tool", "Tool results"),
                ("verification", "Verification"),
                ("error", "Errors detected"),
                ("correction", "Corrections"),
                ("steering", "Steering (cancel, mid-run)"),
                ("permission", "Permission requests"),
                ("diff", "Diff review and survival"),
                ("code", "Code origin"),
                ("survey", "Survey"),
                ("acp", "ACP link and every frame"),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DateRange {
    AllTime,
    Today,
    LastWeek,
    LastMonth,
}

impl DateRange {
    const ALL: [Self; 4] = [Self::AllTime, Self::Today, Self::LastWeek, Self::LastMonth];

    fn label(self) -> &'static str {
        match self {
            Self::AllTime => "All time",
            Self::Today => "Today",
            Self::LastWeek => "Last 7 days",
            Self::LastMonth => "Last 30 days",
        }
    }

    fn includes(self, when: DateTime<Local>, now: DateTime<Local>) -> bool {
        match self {
            Self::AllTime => true,
            Self::Today => when.date_naive() == now.date_naive(),
            Self::LastWeek => now - when <= TimeDelta::days(7),
            Self::LastMonth => now - when <= TimeDelta::days(30),
        }
    }
}

/// Dragging the divider between the two columns.
struct DraggedDivider;

impl Render for DraggedDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// The installed Prime Agent: `RUBIKS_PRIME_AGENT_BIN` when set, else where
/// its installer puts it, else whatever `prime-agent` is on PATH.
fn prime_agent_program() -> PathBuf {
    if let Some(program) =
        std::env::var_os("RUBIKS_PRIME_AGENT_BIN").filter(|value| !value.is_empty())
    {
        return PathBuf::from(program);
    }
    let installed = util::paths::home_dir().join(".local/bin/prime-agent");
    if installed.exists() {
        installed
    } else {
        PathBuf::from("prime-agent")
    }
}

/// Run `prime-agent` with `args` and parse its JSON output.
async fn run_json<T: DeserializeOwned>(program: PathBuf, args: Vec<String>) -> Result<T> {
    let output = util::command::new_command(&program)
        .args(&args)
        .output()
        .await
        .with_context(|| format!("could not run {}", program.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("{}", stderr.trim().trim_start_matches("Error: ")));
    }
    serde_json::from_slice(&output.stdout).context("unexpected output from prime-agent data")
}

fn local_time(timestamp: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|time| time.with_timezone(&Local))
}

fn format_when(when: Option<DateTime<Local>>, raw: &str) -> String {
    when.map_or_else(
        || raw.to_string(),
        |time| time.format("%d %b %Y, %H:%M").to_string(),
    )
}

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(existing) = workspace.item_of_type::<PrimeAgentDataView>(cx) {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let view = cx.new(|cx| PrimeAgentDataView::new(window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

pub struct PrimeAgentDataView {
    focus_handle: FocusHandle,
    program: PathBuf,
    source: Source,
    list_search: Entity<Editor>,
    item_search: Entity<Editor>,
    date_range: DateRange,
    /// A `--kind` value from [`Source::kinds`].
    kind: &'static str,
    list_width: Pixels,
    /// Rows matching the search; `rows` further applies the date range.
    searched_rows: Vec<Row>,
    rows: Vec<Row>,
    selected: Option<String>,
    data: Option<ItemsData>,
    expanded: HashSet<usize>,
    rows_list: ListState,
    items_list: ListState,
    rows_status: Option<SharedString>,
    items_status: Option<SharedString>,
    /// The installed Prime Agent is not the Rubiks build (it has no `prime-agent
    /// data`): the list offers to install it.
    offer_rubiks_install: bool,
    load_rows: Task<()>,
    load_items: Task<()>,
    /// The Rubiks-build check after a failed load, then the install.
    install: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl PrimeAgentDataView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let list_search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search…", window, cx);
            editor
        });
        let item_search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search in this list…", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&list_search, |this: &mut Self, _, event, cx| {
                if let editor::EditorEvent::BufferEdited = event {
                    this.reload_rows(SEARCH_DELAY, cx);
                }
            }),
            cx.subscribe(&item_search, |this: &mut Self, _, event, cx| {
                if let editor::EditorEvent::BufferEdited = event {
                    this.reload_items(SEARCH_DELAY, cx);
                }
            }),
        ];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            program: prime_agent_program(),
            source: Source::Sessions,
            list_search,
            item_search,
            date_range: DateRange::AllTime,
            kind: "all",
            list_width: DEFAULT_LIST_WIDTH,
            searched_rows: Vec::new(),
            rows: Vec::new(),
            selected: None,
            data: None,
            expanded: HashSet::default(),
            rows_list: ListState::new(0, ListAlignment::Top, px(1000.)),
            items_list: ListState::new(0, ListAlignment::Top, px(1000.)),
            rows_status: Some("Loading…".into()),
            items_status: None,
            offer_rubiks_install: false,
            load_rows: Task::ready(()),
            load_items: Task::ready(()),
            install: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.reload_rows(Duration::ZERO, cx);
        this
    }

    fn set_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        self.source = source;
        self.kind = "all";
        self.selected = None;
        self.data = None;
        self.searched_rows.clear();
        self.rows.clear();
        self.rows_list.reset(0);
        self.items_list.reset(0);
        self.rows_status = Some("Loading…".into());
        self.reload_rows(Duration::ZERO, cx);
        cx.notify();
    }

    fn reload_rows(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let query = self.list_search.read(cx).text(cx);
        let program = self.program.clone();
        let source = self.source;
        self.load_rows = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let result = match source {
                Source::Sessions => {
                    let mut args = vec!["data".into(), "sessions".into(), "--json".into()];
                    if !query.trim().is_empty() {
                        args.extend(["--search".to_string(), query]);
                    }
                    cx.background_spawn(run_json::<Vec<SessionRow>>(program, args))
                        .await
                        .map(|sessions| sessions.into_iter().map(Row::from_session).collect())
                }
                Source::Research => {
                    let args = vec!["data".into(), "research".into(), "--json".into()];
                    let query = query.trim().to_lowercase();
                    cx.background_spawn(run_json::<Vec<ResearchRow>>(program, args))
                        .await
                        .map(|logs| {
                            logs.into_iter()
                                .map(Row::from_research)
                                .filter(|row| {
                                    query.is_empty()
                                        || row.title.to_lowercase().contains(&query)
                                        || row.id.to_lowercase().contains(&query)
                                })
                                .collect()
                        })
                }
                Source::Harness => {
                    let args = vec!["data".into(), "harness".into(), "--json".into()];
                    let query = query.trim().to_lowercase();
                    cx.background_spawn(run_json::<Vec<HarnessScope>>(program, args))
                        .await
                        .map(|scopes| {
                            scopes
                                .into_iter()
                                .map(Row::from_scope)
                                .filter(|row| {
                                    query.is_empty()
                                        || row.title.to_lowercase().contains(&query)
                                        || row.id.to_lowercase().contains(&query)
                                })
                                .collect()
                        })
                }
            };
            this.update(cx, |this, cx| this.rows_loaded(result, cx))
                .log_err();
        });
    }

    fn rows_loaded(&mut self, result: Result<Vec<Row>>, cx: &mut Context<Self>) {
        match result {
            Ok(rows) => {
                self.searched_rows = rows;
                self.rows_status = None;
                self.offer_rubiks_install = false;
            }
            Err(error) => {
                self.searched_rows.clear();
                self.rows_status = Some(format!("{error:#}").into());
                self.check_for_rubiks_build(cx);
            }
        }
        self.apply_date_range(cx);
    }

    /// After a failed load: when the installed Prime Agent is not the Rubiks
    /// build, say so and offer to install it instead of showing its error.
    fn check_for_rubiks_build(&mut self, cx: &mut Context<Self>) {
        let program = self.program.clone();
        self.install = cx.spawn(async move |this, cx| {
            let is_rubiks = cx
                .background_spawn(async move {
                    client::rubiks_account::is_rubiks_prime_agent(&program).await
                })
                .await;
            if is_rubiks {
                return;
            }
            this.update(cx, |this, cx| {
                this.rows_status = Some(
                    "This Prime Agent has no `prime-agent data` command: it is not the Rubiks build. \
                     Install Prime Agent (Rubiks) to browse sessions, research logs and harness memory."
                        .into(),
                );
                this.offer_rubiks_install = true;
                cx.notify();
            })
            .log_err();
        });
    }

    fn install_rubiks_build(&mut self, cx: &mut Context<Self>) {
        self.offer_rubiks_install = false;
        self.rows_status = Some("Installing Prime Agent (Rubiks)…".into());
        cx.notify();
        self.install = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(client::rubiks_account::install_rubiks_prime_agent())
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.program = prime_agent_program();
                        this.rows_status = Some("Loading…".into());
                        this.reload_rows(Duration::ZERO, cx);
                    }
                    Err(error) => {
                        this.rows_status = Some(
                            format!("Could not install Prime Agent (Rubiks): {error:#}").into(),
                        );
                        this.offer_rubiks_install = true;
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn apply_date_range(&mut self, cx: &mut Context<Self>) {
        let now = Local::now();
        let range = self.date_range;
        self.rows = self
            .searched_rows
            .iter()
            .filter(|row| row.when.is_none_or(|when| range.includes(when, now)))
            .cloned()
            .collect();
        self.rows_list.reset(self.rows.len());
        let still_listed = self
            .selected
            .as_ref()
            .is_some_and(|selected| self.rows.iter().any(|row| &row.id == selected));
        if !still_listed {
            match self.rows.first().map(|row| row.id.clone()) {
                Some(first) => self.select_row(first, cx),
                None => {
                    self.selected = None;
                    self.data = None;
                    self.items_list.reset(0);
                }
            }
        }
        cx.notify();
    }

    fn set_date_range(&mut self, range: DateRange, cx: &mut Context<Self>) {
        self.date_range = range;
        self.apply_date_range(cx);
    }

    fn set_kind(&mut self, kind: &'static str, cx: &mut Context<Self>) {
        self.kind = kind;
        self.reload_items(Duration::ZERO, cx);
    }

    fn select_row(&mut self, id: String, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.data = None;
        self.items_list.reset(0);
        self.reload_items(Duration::ZERO, cx);
        cx.notify();
    }

    fn reload_items(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let query = self.item_search.read(cx).text(cx);
        let program = self.program.clone();
        let command = match self.source {
            Source::Sessions => "show",
            Source::Harness => "harness",
            Source::Research => "research",
        };
        let mut args = vec![
            "data".to_string(),
            command.to_string(),
            id,
            "--kind".to_string(),
            self.kind.to_string(),
            "--json".to_string(),
        ];
        if !query.trim().is_empty() {
            args.extend(["--search".to_string(), query]);
        }
        self.items_status = Some("Loading…".into());
        self.load_items = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let result = cx
                .background_spawn(run_json::<ItemsData>(program, args))
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(data) => {
                        this.items_list.reset(data.items.len());
                        this.data = Some(data);
                        this.items_status = None;
                    }
                    Err(error) => {
                        this.items_list.reset(0);
                        this.data = None;
                        this.items_status = Some(format!("{error:#}").into());
                    }
                }
                this.expanded.clear();
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn toggle_item(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&index) {
            self.expanded.insert(index);
        }
        // The item's height changed; let the list measure it again.
        self.items_list.splice(index..index + 1, 1);
        cx.notify();
    }

    fn resize_list(&mut self, event: &DragMoveEvent<DraggedDivider>, cx: &mut Context<Self>) {
        let max = (event.bounds.size.width - MIN_ITEMS_WIDTH).max(MIN_LIST_WIDTH);
        self.list_width = (event.event.position.x - event.bounds.left()).clamp(MIN_LIST_WIDTH, max);
        cx.notify();
    }

    fn reset_list_width(&mut self, event: &ClickEvent, cx: &mut Context<Self>) {
        if event.click_count() >= 2 {
            self.list_width = DEFAULT_LIST_WIDTH;
            cx.notify();
        }
    }

    /// The selected kind's count; everything but raw records for "all".
    fn kind_count(&self, kind: &str) -> usize {
        let Some(data) = self.data.as_ref() else {
            return 0;
        };
        if kind == "all" {
            return data
                .counts
                .iter()
                .filter(|(kind, _)| kind.as_str() != "raw")
                .map(|(_, count)| count)
                .sum();
        }
        // Research kinds are event-type prefixes (`api` covers `api.call`
        // and `api.retry`).
        data.counts
            .iter()
            .filter(|(name, _)| {
                name.as_str() == kind
                    || name
                        .strip_prefix(kind)
                        .is_some_and(|rest| rest.starts_with('.'))
            })
            .map(|(_, count)| count)
            .sum()
    }

    fn kind_label(&self, kind: &str) -> String {
        let label = self
            .source
            .kinds()
            .iter()
            .find(|(value, _)| *value == kind)
            .map_or(kind, |(_, label)| label);
        format!("{label} ({})", self.kind_count(kind))
    }

    fn search_box(editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .px_2()
            .py_1()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(editor.clone())
    }

    fn render_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };
        let selected = self.selected.as_deref() == Some(row.id.as_str());
        let id = row.id.clone();
        let colors = cx.theme().colors();
        v_flex()
            .id(("prime-agent-row", index))
            .px_3()
            .py_2()
            .gap_0p5()
            .cursor_pointer()
            .border_b_1()
            .border_color(colors.border_variant)
            .when(selected, |row| row.bg(colors.element_selected))
            .hover(|row| row.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _, _, cx| this.select_row(id.clone(), cx)))
            .child(Label::new(row.title.clone()).truncate())
            .child(
                Label::new(row.details.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
            .child(
                Label::new(row.footer.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
            .into_any_element()
    }

    fn render_item(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.data.as_ref().and_then(|data| data.items.get(index)) else {
            return div().into_any_element();
        };
        let expanded = self.expanded.contains(&index);
        let lines: Vec<&str> = item.text.lines().collect();
        let hidden = lines.len().saturating_sub(COLLAPSED_LINES);
        let shown = if expanded || hidden == 0 {
            item.text.clone()
        } else {
            lines[..COLLAPSED_LINES].join("\n")
        };
        let time_format = match self.source {
            Source::Sessions | Source::Research => "%H:%M:%S",
            Source::Harness => "%d %b %Y, %H:%M",
        };
        let time = local_time(&item.timestamp).map_or_else(
            || item.timestamp.clone(),
            |time| time.format(time_format).to_string(),
        );
        let position = match self.source {
            Source::Sessions | Source::Research => format!("line {}", item.line),
            Source::Harness => format!("#{}", item.line),
        };
        let kind_color = match item.kind.as_str() {
            "chat" | "memory" | "api.call" | "user.prompt" | "run.end" | "survey"
            | "diff.survival" | "permission" => Color::Accent,
            "thinking" | "context.built" | "run.start" => Color::Muted,
            "tools" | "refinement" | "tool.result" | "api.retry" => Color::Warning,
            "usage" | "skill" | "verification" | "correction.delivered" | "diff.review" => {
                Color::Success
            }
            "raw" | "prompt" | "context.compacted" | "acp.session_bound" | "code.origin" => {
                Color::Info
            }
            "error.detected" | "steering" => Color::Error,
            _ => Color::Default,
        };
        let colors = cx.theme().colors();
        v_flex()
            .id(("prime-agent-item", index))
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .when(hidden > 0, |row| {
                row.cursor_pointer()
                    .hover(|row| row.bg(colors.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_item(index, cx)))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(time).size(LabelSize::XSmall).color(Color::Muted))
                    .child(
                        Label::new(item.kind.clone())
                            .size(LabelSize::XSmall)
                            .color(kind_color),
                    )
                    .child(
                        Label::new(item.label.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        Label::new(position)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(div().text_xs().font_buffer(cx).child(shown))
            .when(hidden > 0, |row| {
                row.child(
                    Label::new(if expanded {
                        "Click to collapse".to_string()
                    } else {
                        format!("… {hidden} more lines, click to expand")
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_list_column(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let current = self.date_range;
        let date_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for range in DateRange::ALL {
                let view = view.clone();
                menu = menu.toggleable_entry(
                    range.label(),
                    range == current,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        view.update(cx, |this, cx| this.set_date_range(range, cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        let source_button = |source: Source, label: &'static str, cx: &mut Context<Self>| {
            Button::new(label, label)
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .toggle_state(self.source == source)
                .on_click(cx.listener(move |this, _, _, cx| this.set_source(source, cx)))
        };
        let empty = match self.source {
            Source::Sessions => "No sessions match.",
            Source::Harness => "No harness memory matches.",
            Source::Research => {
                "No research logs yet. New sessions write one once Prime Agent runs the new build."
            }
        };
        let body = if let Some(status) = self.rows_status.clone() {
            v_flex()
                .flex_1()
                .p_3()
                .gap_2()
                .child(Label::new(status).color(Color::Muted))
                .when(self.offer_rubiks_install, |body| {
                    body.child(
                        Button::new(
                            "prime-agent-data-install-rubiks",
                            "Install Prime Agent (Rubiks)",
                        )
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(|this, _, _, cx| this.install_rubiks_build(cx))),
                    )
                })
                .into_any_element()
        } else if self.rows.is_empty() {
            v_flex()
                .flex_1()
                .p_3()
                .child(Label::new(empty).color(Color::Muted))
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .overflow_hidden()
                .child(list(self.rows_list.clone(), cx.processor(Self::render_row)).size_full())
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.rows_list),
                    window,
                    cx,
                )
                .into_any_element()
        };
        v_flex()
            .w(self.list_width)
            .flex_none()
            .h_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .gap_1()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(source_button(Source::Sessions, "Sessions", cx))
                            .child(source_button(Source::Harness, "Harness memory", cx))
                            .child(source_button(Source::Research, "Research log", cx)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(self.rows.len().to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new("prime-agent-data-refresh", IconName::RotateCw)
                                    .tooltip(Tooltip::text("Reload"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_rows(Duration::ZERO, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .pb_2()
                    .gap_2()
                    .child(Self::search_box(&self.list_search, cx))
                    .child(DropdownMenu::new(
                        "prime-agent-date-range",
                        current.label(),
                        date_menu,
                    )),
            )
            .child(body)
    }

    fn render_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .id("prime-agent-data-divider")
            .flex_none()
            .w(px(5.))
            .h_full()
            .cursor_col_resize()
            .border_l_1()
            .border_color(colors.border)
            .hover(|divider| divider.bg(colors.element_hover))
            .tooltip(Tooltip::text("Drag to resize; double-click to reset"))
            .on_drag(DraggedDivider, |_, _, _, cx| cx.new(|_| DraggedDivider))
            .on_click(cx.listener(|this, event, _, cx| this.reset_list_width(event, cx)))
    }

    fn render_items_column(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(row) = self
            .selected
            .as_ref()
            .and_then(|selected| self.rows.iter().find(|row| &row.id == selected))
        else {
            return v_flex()
                .flex_1()
                .p_4()
                .child(Label::new("Pick an entry on the left to see its data.").color(Color::Muted))
                .into_any_element();
        };
        let view = cx.entity().downgrade();
        let current = self.kind;
        let labels: Vec<(&'static str, String)> = self
            .source
            .kinds()
            .iter()
            .map(|(kind, _)| (*kind, self.kind_label(kind)))
            .collect();
        let kind_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for (kind, label) in labels {
                let view = view.clone();
                menu = menu.toggleable_entry(
                    label,
                    kind == current,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        view.update(cx, |this, cx| this.set_kind(kind, cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        let body = if let Some(status) = self.items_status.clone() {
            v_flex()
                .flex_1()
                .p_3()
                .child(Label::new(status).color(Color::Muted))
                .into_any_element()
        } else if self.data.as_ref().is_none_or(|data| data.items.is_empty()) {
            v_flex()
                .flex_1()
                .p_3()
                .child(Label::new("Nothing matches these filters.").color(Color::Muted))
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .overflow_hidden()
                .child(list(self.items_list.clone(), cx.processor(Self::render_item)).size_full())
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.items_list),
                    window,
                    cx,
                )
                .into_any_element()
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_0p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(row.title.clone())
                            .size(LabelSize::Large)
                            .truncate(),
                    )
                    .child(
                        Label::new(row.header.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .child(DropdownMenu::new(
                        "prime-agent-kind",
                        self.kind_label(current),
                        kind_menu,
                    ))
                    .child(Self::search_box(&self.item_search, cx)),
            )
            .child(body)
            .into_any_element()
    }
}

impl EventEmitter<()> for PrimeAgentDataView {}

impl Focusable for PrimeAgentDataView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PrimeAgentDataView {
    type Event = ();

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Prime Agent Data".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

impl Render for PrimeAgentDataView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("PrimeAgentDataView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_drag_move::<DraggedDivider>(cx.listener(
                |this, event: &DragMoveEvent<DraggedDivider>, _, cx| this.resize_list(event, cx),
            ))
            .child(self.render_list_column(window, cx))
            .child(self.render_divider(cx))
            .child(self.render_items_column(window, cx))
    }
}
