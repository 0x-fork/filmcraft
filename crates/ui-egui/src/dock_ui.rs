//! FilmCraft's panels in craft-ui's shared docking, Frame profile. craft-ui draws the frames, tab
//! strips, floating windows and drag previews and owns the gestures; FilmCraft draws the panel
//! bodies, the ≡ panel menu and the Timeline's sequence tabs, and maps its control commands onto
//! craft-ui's `DockCommand`.

use craft_ui::docking::{
    Action, Divider, DockArea, DockCommand, DockContent, DockEvent, DockStyle, FloatSpace, Hit, Layout, Placement, Profile, ResizeEdge, Workspace, Zone,
};
use egui::{Rect, Sense, Stroke, StrokeKind, vec2};
use serde_json::{Value, json};

use crate::dock::{self, PanelKind, SeqAction, SeqTabs};
use crate::icons::{self, Icon};
use crate::theme::Tokens;
use crate::{FilmcraftApp, panels};

/// The dock area's egui id; `DockArea::hit_rects` and `float_space` read by it.
pub fn area_id() -> egui::Id {
    egui::Id::new("filmcraft-docking")
}

/// Where each panel body was drawn last frame, and on which layer.
fn bodies_id() -> egui::Id {
    area_id().with("bodies")
}

/// Width of the ≡ panel menu button at the end of each tab strip.
const MENU_W: f32 = 26.0;

pub(crate) fn route(app: &mut FilmcraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if id == "window.dockAction" {
        return Some((|| {
            let value = params.get("action").cloned().ok_or("missing docking action")?;
            let action = serde_json::from_value(value).map_err(|error| format!("invalid docking action: {error}"))?;
            app.apply_dock(action)?;
            Ok(Value::Null)
        })());
    }
    if matches!(id, "window.dockPanel" | "window.floatPanel" | "window.closePanel") {
        return Some(alias(app, ctx, id, params));
    }
    if id.starts_with("window.panel.") && DockCommand::<PanelKind>::IDS.contains(&id) {
        return Some((|| {
            let mut params = if params.is_object() { params.clone() } else { json!({}) };
            params["command"] = json!(id);
            let command: DockCommand<PanelKind> = serde_json::from_value(params).map_err(|error| format!("invalid docking command: {error}"))?;
            app.run_dock_command(ctx, command)?;
            Ok(Value::Null)
        })());
    }
    None
}

/// FilmCraft's own docking commands, as craft-ui `window.panel.*` commands. `panel` defaults to
/// the focused panel.
fn alias(app: &mut FilmcraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Result<Value, String> {
    let panel = match params.get("panel") {
        None => app.ui.focused,
        Some(value) => value.as_str().and_then(PanelKind::from_name).ok_or("unknown panel")?,
    };
    let command = match id {
        "window.closePanel" => json!({"command": "window.panel.close", "panel": panel}),
        "window.floatPanel" => match params.get("rect") {
            None => json!({"command": "window.panel.float", "panel": panel}),
            Some(value) => {
                let rect = serde_json::from_value::<[f32; 4]>(value.clone()).map_err(|_| "rect must contain four finite numbers")?;
                json!({"command": "window.panel.layout", "action": Action::Float { panel, rect }})
            }
        },
        _ => {
            let zone = match params.get("zone") {
                None => "center",
                Some(value) => value.as_str().ok_or("zone must be a string")?,
            };
            match params.get("anchor") {
                Some(anchor) => {
                    let anchor = anchor.as_str().and_then(PanelKind::from_name).ok_or("a known anchor panel is required")?;
                    let mut command = json!({"command": "window.panel.dock", "panel": panel, "anchor": anchor, "zone": zone.to_ascii_lowercase()});
                    if let Some(before) = params.get("before") {
                        command["before"] = json!(before.as_str().and_then(PanelKind::from_name).ok_or("unknown before panel")?);
                    }
                    command
                }
                None if params.get("before").is_some() => return Err("before needs an anchor panel".into()),
                // Without an anchor a side zone docks along that edge of the whole workspace.
                None => {
                    let edge = match zone.to_ascii_lowercase().as_str() {
                        "left" => Some(Zone::Left),
                        "right" => Some(Zone::Right),
                        "top" => Some(Zone::Top),
                        "bottom" => Some(Zone::Bottom),
                        "center" | "tab" => None,
                        _ => return Err("unknown docking zone".into()),
                    };
                    match edge {
                        None => json!({"command": "window.panel.dock", "panel": panel}),
                        Some(edge) => {
                            let docked = Layout { root: app.ui.dock.root.clone(), floating: vec![] };
                            let anchor =
                                docked.panels().into_iter().copied().find(|other| *other != panel).ok_or("another docked panel is required to dock beside")?;
                            json!({"command": "window.panel.layout", "action": Action::Move { panel, anchor, placement: Placement::Edge(edge) }})
                        }
                    }
                }
            }
        }
    };
    let command: DockCommand<PanelKind> = serde_json::from_value(command).map_err(|error| format!("invalid docking command: {error}"))?;
    app.run_dock_command(ctx, command)?;
    Ok(json!({"panel": panel.id()}))
}

/// Keep one docked destination available for dragging floating panels back into the workspace.
fn check_last_docked(layout: &Layout<PanelKind>, action: &Action<PanelKind>) -> Result<(), String> {
    let panel = match action {
        Action::Float { panel, .. } => Some(panel),
        Action::Move { panel, anchor, .. } if layout.floating.iter().any(|window| window.contains(anchor)) => Some(panel),
        _ => None,
    };
    if let Some(panel) = panel {
        let docked = Layout { root: layout.root.clone(), floating: vec![] };
        if docked.contains(panel) && docked.panels().len() <= 1 {
            return Err("the last docked panel must remain in the workspace".into());
        }
    }
    Ok(())
}

impl FilmcraftApp {
    /// A layout action from the dock area, through the command path the control channel uses.
    fn dock_command(&mut self, ctx: &egui::Context, action: Action<PanelKind>) -> Result<(), String> {
        let value = serde_json::to_value(action).map_err(|error| error.to_string())?;
        crate::menus::invoke(self, ctx, "window.dockAction", json!({"action": value})).map(|_| ())
    }

    /// Run a craft-ui docking command on the current layout, all or nothing. Workspaces are
    /// FilmCraft's own (Window ▸ Workspaces).
    pub(crate) fn run_dock_command(&mut self, ctx: &egui::Context, command: DockCommand<PanelKind>) -> Result<(), String> {
        if command.is_app_owned() {
            return Err("use the window.workspace.* commands for workspaces".into());
        }
        let space = craft_ui::docking::float_space(ctx, area_id()).unwrap_or_else(|| FloatSpace::new(ctx.content_rect()));
        let actions = command.to_actions(&Workspace::new(self.ui.dock.clone()), false, &space).map_err(|error| error.to_string())?;
        self.apply_dock_all(actions)
    }

    pub fn apply_dock(&mut self, action: Action<PanelKind>) -> Result<(), String> {
        self.apply_dock_all(vec![action])
    }

    fn apply_dock_all(&mut self, actions: Vec<Action<PanelKind>>) -> Result<(), String> {
        let mut layout = self.ui.dock.clone();
        let mut relocating = false;
        let mut reveal = None;
        let mut closed_floating = self.ui.closed_floating.clone();
        for action in actions {
            relocating |= matches!(&action, Action::Move { .. } | Action::Float { .. } | Action::MoveGroup { .. } | Action::MoveWindow { .. });
            if let Action::Activate { panel } = &action {
                reveal = Some(*panel);
            }
            // a panel closed while floating remembers where; closed while docked, it forgets (#1985)
            for panel in layout.closed_by(&action).map_err(|error| error.to_string())? {
                closed_floating.retain(|(p, _)| *p != panel);
                if let Some(window) = layout.floating.iter().find(|window| window.contains(&panel)) {
                    closed_floating.push((panel, window.rect));
                }
            }
            check_last_docked(&layout, &action)?;
            layout.apply(action).map_err(|error| error.to_string())?;
        }
        self.ui.dock = layout;
        self.ui.closed_floating = closed_floating;
        if let Some(panel) = reveal {
            self.pending_panel_raise = Some(panel);
        }
        if relocating || self.ui.keys.maximized.is_some_and(|panel| !self.ui.dock.contains(&panel)) {
            self.ui.keys.maximized = None;
        }
        Ok(())
    }

    /// OS drag positions come from the host cursor hook, so egui pointer capture alone cannot
    /// decide whether a Timeline is covered: the Timeline body drawn last frame must hold `pos`,
    /// with no floating window above it there.
    pub(crate) fn timeline_drop_target(&self, ctx: &egui::Context, pos: egui::Pos2) -> bool {
        if !ctx.content_rect().contains(pos) {
            return false;
        }
        let Some(bodies) = ctx.data(|data| data.get_temp::<Vec<(PanelKind, Rect, egui::LayerId)>>(bodies_id())) else { return true };
        let Some((_, _, timeline)) = bodies.iter().find(|(panel, rect, _)| *panel == PanelKind::Timeline && rect.contains(pos)) else { return false };
        let layers = ctx.memory(|memory| memory.layer_ids().collect::<Vec<_>>());
        let rank = |layer: &egui::LayerId| layers.iter().position(|other| other == layer);
        !self.ui.dock.floating.iter().any(|window| {
            let Some(first) = window.panels().next() else { return false };
            let layer = egui::LayerId::new(egui::Order::Middle, area_id().with(("floating", *first)));
            layer != *timeline && rank(&layer) > rank(timeline) && ctx.memory(|memory| memory.area_rect(layer.id)).is_some_and(|rect| rect.contains(pos))
        })
    }

    pub(crate) fn dock_area(&mut self, ui: &mut egui::Ui, body: Rect) {
        let t = self.tokens;
        let ctx = ui.ctx().clone();
        let seqs = SeqTabs {
            open: self
                .session
                .state
                .open_sequences
                .iter()
                .filter_map(|id| {
                    self.session
                        .project
                        .item(*id)
                        .filter(|item| matches!(item.kind, filmcraft_project::ItemKind::Sequence(_)))
                        .map(|item| (id.0, item.name.clone()))
                })
                .collect(),
            active: self.session.state.active_sequence.map(|id| id.0),
        };
        let style = dock_style(ui, &t);
        let mut area = DockArea::new(area_id()).maximized(self.ui.keys.maximized.filter(|panel| self.ui.dock.contains(panel)));
        if let Some(panel) = self.pending_panel_raise.take() {
            area = area.raise_panel(panel);
        }
        let layout = self.ui.dock.clone();
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body).id_salt("filmcraft-dock-area"));
        child.set_clip_rect(body.intersect(ui.clip_rect()));
        let seq_width = dock::sequence_tabs_width(ui, &seqs);
        let mut content = FilmContent { app: self, seqs, seq_width, tokens: t, seq_actions: Vec::new(), menu: None, bodies: Vec::new() };
        let output = area.show_content(&mut child, &layout, &style, &mut content);
        let FilmContent { seq_actions, mut menu, bodies, .. } = content;
        ctx.data_mut(|data| data.insert_temp(bodies_id(), bodies));
        if let Some(panel) = output.focused {
            self.ui.focused = panel;
        }
        if let Some(error) = output.error {
            self.ui.status = error.to_string();
        }
        for event in output.events {
            if let DockEvent::ToggleMaximize { panel } = event {
                panels::keyboard::toggle_maximize(self, panel);
            }
        }
        for action in output.actions {
            if let Err(error) = self.dock_command(&ctx, action) {
                self.ui.status = error;
            }
        }
        for action in seq_actions {
            let result = match action {
                SeqAction::Open(id) => {
                    let _ = self.apply_dock(Action::Activate { panel: PanelKind::Timeline });
                    self.ui.focused = PanelKind::Timeline;
                    if self.session.state.active_sequence.map(|item| item.0) == Some(id) {
                        Ok(Value::Null)
                    } else {
                        self.session.execute("sequence.open", json!({"item": id}))
                    }
                }
                SeqAction::Close(id) => self.session.execute("sequence.close", json!({"item": id})),
                SeqAction::Move(id, index) => self.session.execute("sequence.moveTab", json!({"item": id, "index": index})),
                SeqAction::Menu(id, pos) => {
                    menu = Some((PanelKind::Timeline, pos));
                    self.ui.focused = PanelKind::Timeline;
                    if self.session.state.active_sequence.map(|item| item.0) == Some(id) {
                        Ok(Value::Null)
                    } else {
                        self.session.execute("sequence.open", json!({"item": id}))
                    }
                }
            };
            if let Err(error) = result {
                self.ui.status = error.to_string();
            }
        }
        if let Some((panel, pos)) = menu {
            let frame = ctx.cumulative_frame_nr();
            ctx.data_mut(|data| {
                data.insert_temp(egui::Id::new("panel-menu"), (panel, pos));
                data.insert_temp(egui::Id::new("panel-menu-opened"), frame);
            });
        }
        let hits = DockArea::<PanelKind>::hit_rects(&ctx, area_id());
        self.tab_strip_pointer(&ctx, &hits);
        self.register_dock_hits(&hits);
        panels::panel_menu_popup(self, ui);
        self.cancel_value_drag(ui.ctx());
    }

    /// Automation ids for craft-ui's gesture sources: `panel.tab.<Panel>`, `panel.drag.<Panel>`
    /// (the group's drag bar), `panel.tabs.more.<Panel>`, `panel.float.<n>.move` and `.resize`,
    /// and `dock.gutter.<path>` (`a`/`b` per split level).
    fn register_dock_hits(&mut self, hits: &[(Hit<PanelKind>, Rect)]) {
        let window = |panel: &PanelKind| self.ui.dock.floating.iter().position(|window| window.contains(panel));
        let mut found = Vec::new();
        for (hit, rect) in hits {
            let entry = match hit {
                Hit::Tab(panel) => (format!("panel.tab.{}", panel.id()), panel.title()),
                Hit::DragBar(panel) => (format!("panel.drag.{}", panel.id()), "Move panel"),
                Hit::Overflow(panel) => (format!("panel.tabs.more.{}", panel.id()), "more tabs"),
                Hit::Title(panel) => match window(panel) {
                    Some(index) => (format!("panel.float.{index}.move"), "Move floating panel"),
                    None => continue,
                },
                Hit::Resize(panel, ResizeEdge::BottomRight) => match window(panel) {
                    Some(index) => (format!("panel.float.{index}.resize"), "Resize floating panel"),
                    None => continue,
                },
                Hit::Divider(Divider::Split(path)) => {
                    (format!("dock.gutter.{}", path.iter().map(|second| if *second { 'b' } else { 'a' }).collect::<String>()), "gutter")
                }
                _ => continue,
            };
            found.push((entry, *rect));
        }
        for ((id, label), rect) in found {
            self.auto.add(&id, rect, label);
        }
    }
}

impl FilmcraftApp {
    /// Over a tab strip the wheel steps through the group's tabs (down = next, one tab per notch,
    /// trackpad scrolling adding up to a notch, stopping at the ends), and a middle click on a tab
    /// closes it; on the Timeline's tab it closes the active sequence and keeps the panel.
    fn tab_strip_pointer(&mut self, ctx: &egui::Context, hits: &[(Hit<PanelKind>, Rect)]) {
        let wheel_id = area_id().with("tab-wheel");
        let pos = ctx.pointer_hover_pos();
        let over = hits.iter().find_map(|(hit, rect)| match hit {
            Hit::Tab(panel) | Hit::DragBar(panel) | Hit::Overflow(panel) if pos.is_some_and(|pos| rect.contains(pos)) => {
                Some((matches!(hit, Hit::Tab(_)), *panel))
            }
            _ => None,
        });
        let Some((on_tab, panel)) = over else {
            ctx.data_mut(|data| data.remove::<f32>(wheel_id));
            return;
        };
        if on_tab && ctx.input(|input| input.pointer.button_clicked(egui::PointerButton::Middle)) {
            let result = if panel == PanelKind::Timeline && self.session.state.active_sequence.is_some() {
                self.session.execute("sequence.close", json!({})).map(drop).map_err(|error| error.to_string())
            } else {
                self.dock_command(ctx, Action::Close { panel })
            };
            if let Err(error) = result {
                self.ui.status = error;
            }
            return;
        }
        let Some((panels, active)) = group_of(&self.ui.dock, panel) else { return };
        const NOTCH: f32 = 40.0;
        let total = ctx.data(|data| data.get_temp::<f32>(wheel_id)).unwrap_or(0.0) + panels::timeline::wheel_input(ctx).delta.y;
        let steps = (total / NOTCH).trunc();
        ctx.data_mut(|data| data.insert_temp(wheel_id, total - steps * NOTCH));
        let to = (active as i64).saturating_sub(steps as i64).clamp(0, (panels.len() as i64 - 1).max(0)) as usize;
        if let Some(next) = panels.get(to).filter(|_| to != active).copied() {
            let _ = self.dock_command(ctx, Action::Activate { panel: next });
            self.ui.focused = next;
        }
    }
}

/// The panels of the tab group holding `panel`, and its active index.
fn group_of(layout: &Layout<PanelKind>, panel: PanelKind) -> Option<(Vec<PanelKind>, usize)> {
    fn find(node: &craft_ui::docking::Node<PanelKind>, panel: PanelKind) -> Option<(Vec<PanelKind>, usize)> {
        match node {
            craft_ui::docking::Node::Tabs { panels, active } if panels.contains(&panel) => Some((panels.clone(), *active)),
            craft_ui::docking::Node::Split { first, second, .. } => find(first, panel).or_else(|| find(second, panel)),
            _ => None,
        }
    }
    layout.root.as_ref().and_then(|root| find(root, panel)).or_else(|| {
        layout.floating.iter().flat_map(|window| &window.groups).find(|group| group.panels.contains(&panel)).map(|group| (group.panels.clone(), group.active))
    })
}

fn dock_style(ui: &egui::Ui, t: &Tokens) -> DockStyle {
    let mut style = DockStyle::from_ui(ui);
    style.profile = Profile::Frame;
    style.tab_height = t.tab_h;
    style.gap = t.gap;
    style.background = t.panel_bg;
    style.tab_background = t.tab_bg;
    style.active_background = t.panel_bg;
    style.floating_background = t.panel_bg;
    style.text = t.tab_text_active;
    style.inactive_text = t.tab_text;
    style.accent = t.focus;
    style.font = Tokens::ui(12.0);
    style
}

/// FilmCraft's side of the dock area for one frame.
struct FilmContent<'a> {
    app: &'a mut FilmcraftApp,
    seqs: SeqTabs,
    /// The width the sequence tabs take in the Timeline's tab strip.
    seq_width: f32,
    tokens: Tokens,
    seq_actions: Vec<SeqAction>,
    /// The ≡ button clicked: its panel and where its menu opens.
    menu: Option<(PanelKind, egui::Pos2)>,
    bodies: Vec<(PanelKind, Rect, egui::LayerId)>,
}

impl DockContent<PanelKind> for FilmContent<'_> {
    fn body(&mut self, ui: &mut egui::Ui, panel: &PanelKind) {
        let t = self.tokens;
        let rect = ui.max_rect();
        self.bodies.push((*panel, rect, ui.layer_id()));
        self.app.auto.add(&format!("panel.{}", panel.id()), rect, panel.title());
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("panel", panel.id())));
        child.set_clip_rect(rect.intersect(ui.clip_rect()));
        panels::show(self.app, &mut child, *panel, rect);
        // Painted after the body so child backgrounds cannot hide the focus outline (#1075).
        if self.app.ui.focused == *panel {
            ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0, t.focus), StrokeKind::Inside);
        }
    }

    fn logical_label(&self, panel: &PanelKind) -> String {
        crate::i18n::t(panel.title()).to_string()
    }

    /// With no sequence open, the Timeline's tab says so.
    fn label(&mut self, _ui: &egui::Ui, panel: &PanelKind, logical: &str, _font: &egui::FontId, _color: egui::Color32) -> egui::WidgetText {
        if *panel == PanelKind::Timeline && self.seqs.open.is_empty() { self.seqs.label().into() } else { logical.into() }
    }

    /// A floated panel is at most 420 by 320 points: panel bodies fill their frame, so their
    /// drawn height says nothing about the size they need.
    fn floating_size(&self, _panel: &PanelKind) -> Option<egui::Vec2> {
        Some(vec2(420.0, 320.0))
    }

    /// Tools and the audio meters are narrow strips without a tab strip.
    fn bare(&self, panel: &PanelKind) -> bool {
        panel.compact()
    }

    /// The ≡ menu, and the Timeline's sequence tabs before it while the Timeline is shown.
    fn group_header_width(&self, _panels: &[PanelKind], active: &PanelKind, _floating: bool) -> f32 {
        if *active == PanelKind::Timeline && !self.seqs.open.is_empty() { self.seq_width + MENU_W } else { MENU_W }
    }

    /// The ≡ panel menu of the group's shown panel; for the Timeline, its sequence tabs first.
    fn group_header(&mut self, ui: &mut egui::Ui, _panels: &[PanelKind], active: &PanelKind, _floating: bool) {
        let t = self.tokens;
        let header = ui.max_rect();
        let menu = Rect::from_min_max(egui::pos2((header.max.x - MENU_W).max(header.min.x), header.min.y), header.max);
        if *active == PanelKind::Timeline && !self.seqs.open.is_empty() {
            let strip = Rect::from_min_max(header.min, egui::pos2(menu.min.x, header.max.y));
            self.seq_actions.extend(dock::sequence_tabs(ui, strip, &self.seqs, &t, &mut self.app.auto));
        }
        let rect = Rect::from_center_size(menu.center(), vec2(18.0, 18.0));
        let response = ui.interact(rect, egui::Id::new(("panel-menu-button", active.id())), Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, response.enabled(), "panel menu"));
        self.app.auto.add_response(&format!("panel.menu.{}", active.id()), &response, "panel menu");
        let color = if response.hovered() { t.tab_text_active } else { t.tab_text };
        icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(16.0, 16.0)), Icon::Hamburger, color);
        dock::paint_widget_focus(ui.painter(), &response, &t);
        if response.clicked() {
            self.app.ui.focused = *active;
            self.menu = Some((*active, rect.left_bottom()));
        }
    }
}
