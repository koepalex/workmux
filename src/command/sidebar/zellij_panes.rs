use anyhow::{Context, Result, anyhow, bail};
use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use std::collections::HashSet;
use tracing::debug;

use crate::config::{Config, SidebarHeight, SidebarPosition, SidebarWidth};
use crate::multiplexer::zellij::{ZellijBackend, ZellijSidebarPane, ZellijSidebarTab};

const SIDEBAR_PANE_NAME: &str = "workmux-sidebar";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PaneRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl PaneRect {
    fn center(self) -> (f64, f64) {
        (
            f64::from(self.x) + f64::from(self.width) / 2.0,
            f64::from(self.y) + f64::from(self.height) / 2.0,
        )
    }

    fn area(self) -> u32 {
        u32::from(self.width) * u32::from(self.height)
    }
}

fn is_sidebar_pane(title: &str) -> bool {
    title == SIDEBAR_PANE_NAME
}

fn backend(instance_id: &str) -> ZellijBackend {
    ZellijBackend::for_session(instance_id)
}

fn tab_extent(tab: &ZellijSidebarTab, position: SidebarPosition) -> Option<u16> {
    let bounds = tab.panes.iter().filter_map(|pane| match position {
        SidebarPosition::Left => Some((pane.pane_x?, pane.pane_x? + pane.pane_columns?)),
        SidebarPosition::Top => Some((pane.pane_y?, pane.pane_y? + pane.pane_rows?)),
    });
    let (minimum, maximum) =
        bounds.fold(None::<(u16, u16)>, |bounds, (start, end)| match bounds {
            Some((minimum, maximum)) => Some((minimum.min(start), maximum.max(end))),
            None => Some((start, end)),
        })?;
    Some(maximum.saturating_sub(minimum))
}

fn pane_rect(pane: &ZellijSidebarPane) -> Option<PaneRect> {
    Some(PaneRect {
        x: pane.pane_x?,
        y: pane.pane_y?,
        width: pane.pane_columns?,
        height: pane.pane_rows?,
    })
}

fn bounds(rects: impl Iterator<Item = PaneRect>) -> Option<PaneRect> {
    rects.fold(None, |bounds, rect| match bounds {
        Some(bounds) => {
            let right = (bounds.x + bounds.width).max(rect.x + rect.width);
            let bottom = (bounds.y + bounds.height).max(rect.y + rect.height);
            let x = bounds.x.min(rect.x);
            let y = bounds.y.min(rect.y);
            Some(PaneRect {
                x,
                y,
                width: right - x,
                height: bottom - y,
            })
        }
        None => Some(rect),
    })
}

fn slot_cost(
    original: PaneRect,
    original_bounds: PaneRect,
    slot: PaneRect,
    slot_bounds: PaneRect,
) -> f64 {
    let (original_center_x, original_center_y) = original.center();
    let expected_center_x = f64::from(slot_bounds.x)
        + (original_center_x - f64::from(original_bounds.x)) * f64::from(slot_bounds.width)
            / f64::from(original_bounds.width);
    let expected_center_y = f64::from(slot_bounds.y)
        + (original_center_y - f64::from(original_bounds.y)) * f64::from(slot_bounds.height)
            / f64::from(original_bounds.height);
    let expected_width =
        f64::from(original.width) * f64::from(slot_bounds.width) / f64::from(original_bounds.width);
    let expected_height = f64::from(original.height) * f64::from(slot_bounds.height)
        / f64::from(original_bounds.height);
    let (slot_center_x, slot_center_y) = slot.center();
    (slot_center_x - expected_center_x).powi(2)
        + (slot_center_y - expected_center_y).powi(2)
        + (f64::from(slot.width) - expected_width).powi(2)
        + (f64::from(slot.height) - expected_height).powi(2)
}

fn target_slots(
    original: &std::collections::HashMap<String, PaneRect>,
    current: &std::collections::HashMap<String, PaneRect>,
) -> Result<std::collections::HashMap<String, PaneRect>> {
    let original_bounds =
        bounds(original.values().copied()).ok_or_else(|| anyhow!("no original pane geometry"))?;
    let slot_bounds =
        bounds(current.values().copied()).ok_or_else(|| anyhow!("no current pane geometry"))?;
    let mut candidates = Vec::new();
    for (pane_id, original_rect) in original {
        for slot in current.values() {
            candidates.push((
                slot_cost(*original_rect, original_bounds, *slot, slot_bounds),
                pane_id.clone(),
                *slot,
            ));
        }
    }
    candidates.sort_by(|left, right| left.0.total_cmp(&right.0));

    let mut assigned_panes = HashSet::new();
    let mut assigned_slots = HashSet::new();
    let mut targets = std::collections::HashMap::new();
    for (_, pane_id, slot) in candidates {
        if assigned_panes.contains(&pane_id) || assigned_slots.contains(&slot) {
            continue;
        }
        assigned_panes.insert(pane_id.clone());
        assigned_slots.insert(slot);
        targets.insert(pane_id, slot);
    }
    if targets.len() != original.len() {
        bail!("could not map all retained Zellij panes to their original slots");
    }
    Ok(targets)
}

fn move_direction(current: PaneRect, target: PaneRect) -> Option<&'static str> {
    if current == target {
        return None;
    }
    let (current_x, current_y) = current.center();
    let (target_x, target_y) = target.center();
    let dx = target_x - current_x;
    let dy = target_y - current_y;
    if dx.abs() >= dy.abs() {
        Some(if dx < 0.0 { "left" } else { "right" })
    } else {
        Some(if dy < 0.0 { "up" } else { "down" })
    }
}

fn sidebar_size(config: &Config, position: SidebarPosition, extent: u16) -> KdlValue {
    let maximum = extent.saturating_sub(1).max(1);
    let resolved = match position {
        SidebarPosition::Left => match config.sidebar.width {
            Some(SidebarWidth::Absolute(width)) => width,
            Some(SidebarWidth::Percent(percent)) => extent.saturating_mul(percent) / 100,
            None => super::resolve_width_for(config, extent, None),
        },
        SidebarPosition::Top => match config.sidebar.height {
            Some(SidebarHeight::Absolute(height)) => height,
            Some(SidebarHeight::Percent(percent)) => extent.saturating_mul(percent) / 100,
            None => super::resolve_height_for(config, extent, None),
        },
    };
    let resolved = resolved.clamp(1, maximum);
    // Zellij adds the separator column to a fixed-width bordered pane.
    let layout_size = match position {
        SidebarPosition::Left => resolved.saturating_sub(1).max(1),
        SidebarPosition::Top => resolved,
    };
    KdlValue::Integer(layout_size.into())
}

fn is_zellij_ui_pane(node: &KdlNode) -> bool {
    node.children().is_some_and(|children| {
        children.nodes().iter().any(|child| {
            child.name().value() == "plugin"
                && child
                    .get("location")
                    .and_then(KdlValue::as_string)
                    .is_some_and(|location| {
                        matches!(location, "zellij:tab-bar" | "zellij:status-bar")
                    })
        })
    })
}

fn property(node: &mut KdlNode, name: &str, value: impl Into<KdlValue>) {
    node.insert(name, KdlEntry::new(value));
}

fn remove_property(node: &mut KdlNode, name: &str) {
    node.entries_mut().retain(|entry| {
        entry
            .name()
            .is_none_or(|entry_name| entry_name.value() != name)
    });
}

fn sidebar_pane(size: KdlValue, executable: &str, cwd: &str) -> KdlNode {
    let mut pane = KdlNode::new("pane");
    property(&mut pane, "name", SIDEBAR_PANE_NAME);
    property(&mut pane, "size", size);
    property(&mut pane, "command", executable);
    property(&mut pane, "cwd", cwd);

    let mut args = KdlNode::new("args");
    args.push("_sidebar-run");
    let mut children = KdlDocument::new();
    children.nodes_mut().push(args);
    pane.set_children(children);
    pane
}

fn active_dumped_tab(layout: &KdlNode, tab_position: u32) -> Option<KdlNode> {
    let tabs: Vec<&KdlNode> = layout
        .children()?
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "tab")
        .collect();
    tabs.iter()
        .find(|tab| tab.get("focus").and_then(KdlValue::as_bool) == Some(true))
        .or_else(|| tabs.get(tab_position as usize))
        .map(|tab| (*tab).clone())
}

fn prepare_retained_content(node: &mut KdlNode) {
    if node.name().value() != "pane" || is_zellij_ui_pane(node) {
        return;
    }
    for property in ["command", "cwd", "name", "close_on_exit", "start_suspended"] {
        remove_property(node, property);
    }
    if let Some(children) = node.children_mut() {
        children.nodes_mut().retain(|child| {
            matches!(
                child.name().value(),
                "pane" | "children" | "plugin" | "floating_panes"
            )
        });
        for child in children.nodes_mut() {
            prepare_retained_content(child);
        }
        if children.nodes().is_empty() {
            *node.children_mut() = None;
        }
    }
}

fn content_container(mut content: Vec<KdlNode>, split_direction: Option<KdlValue>) -> KdlNode {
    for node in &mut content {
        prepare_retained_content(node);
    }
    if content.len() == 1 {
        let mut node = content.remove(0);
        remove_property(&mut node, "size");
        return node;
    }
    let mut container = KdlNode::new("pane");
    if let Some(direction) = split_direction {
        property(&mut container, "split_direction", direction);
    }
    let mut children = KdlDocument::new();
    children.nodes_mut().append(&mut content);
    container.set_children(children);
    container
}

fn layout_with_sidebar(
    dumped_layout: &str,
    tab_position: u32,
    position: SidebarPosition,
    size: KdlValue,
    executable: &str,
    cwd: &str,
) -> Result<String> {
    let document =
        KdlDocument::parse_v1(dumped_layout).context("failed to parse dumped Zellij layout")?;
    let layout = document
        .get("layout")
        .ok_or_else(|| anyhow!("dumped Zellij layout has no layout node"))?;
    let mut tab = active_dumped_tab(layout, tab_position)
        .ok_or_else(|| anyhow!("dumped Zellij layout has no target tab"))?;
    let original_direction = tab.get("split_direction").cloned();
    let original_children = tab
        .children_mut()
        .take()
        .ok_or_else(|| anyhow!("dumped Zellij tab has no panes"))?
        .nodes()
        .to_vec();
    let mut content = Vec::new();
    for node in &original_children {
        if !is_zellij_ui_pane(node) {
            content.push(node.clone());
        }
    }
    if content.is_empty() {
        bail!("dumped Zellij tab has no terminal content panes");
    }

    let mut wrapper = KdlNode::new("pane");
    property(
        &mut wrapper,
        "split_direction",
        match position {
            SidebarPosition::Left => "vertical",
            SidebarPosition::Top => "horizontal",
        },
    );
    let mut wrapper_children = KdlDocument::new();
    wrapper_children
        .nodes_mut()
        .push(sidebar_pane(size, executable, cwd));
    wrapper_children
        .nodes_mut()
        .push(content_container(content, original_direction));
    wrapper.set_children(wrapper_children);

    let mut replacement_children = KdlDocument::new();
    let mut inserted = false;
    for node in original_children {
        if is_zellij_ui_pane(&node) {
            replacement_children.nodes_mut().push(node);
        } else if !inserted {
            replacement_children.nodes_mut().push(wrapper.clone());
            inserted = true;
        }
    }
    tab.set_children(replacement_children);

    let mut root = KdlNode::new("layout");
    let mut root_children = KdlDocument::new();
    root_children.nodes_mut().push(tab);
    root.set_children(root_children);
    let mut output = KdlDocument::new();
    output.nodes_mut().push(root);
    output.autoformat_no_comments();
    output.ensure_v1();
    Ok(output.to_string())
}

fn wait_for_pane_to_close(instance_id: &str, pane_id: &str) -> Result<()> {
    for _ in 0..10 {
        if backend(instance_id)
            .sidebar_tabs()?
            .iter()
            .all(|tab| tab.panes.iter().all(|pane| pane.pane_id != pane_id))
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    bail!("Zellij sidebar pane {pane_id} did not close")
}

fn close_generated_terminal_placeholders(
    instance_id: &str,
    tab_id: u32,
    original_pane_ids: &HashSet<String>,
) -> Result<()> {
    let mux = backend(instance_id);
    let Some(tab) = mux
        .sidebar_tabs()?
        .into_iter()
        .find(|tab| tab.tab_id == tab_id)
    else {
        return Ok(());
    };
    for pane in tab.panes {
        if !original_pane_ids.contains(&pane.pane_id) && !is_sidebar_pane(&pane.title) {
            mux.close_pane_by_id(&pane.pane_id)?;
        }
    }
    Ok(())
}

fn restore_pane_slots(
    instance_id: &str,
    tab_id: u32,
    original: &std::collections::HashMap<String, PaneRect>,
) -> Result<()> {
    if original.len() < 2 {
        return Ok(());
    }
    let mux = backend(instance_id);
    let current_tab = mux
        .sidebar_tabs()?
        .into_iter()
        .find(|tab| tab.tab_id == tab_id)
        .ok_or_else(|| anyhow!("Zellij tab {tab_id} disappeared during layout override"))?;
    let current: std::collections::HashMap<String, PaneRect> = current_tab
        .panes
        .iter()
        .filter(|pane| original.contains_key(&pane.pane_id))
        .filter_map(|pane| pane_rect(pane).map(|rect| (pane.pane_id.clone(), rect)))
        .collect();
    if current.len() != original.len() {
        bail!("Zellij did not report geometry for all retained panes in tab {tab_id}");
    }
    let targets = target_slots(original, &current)?;
    let mut ordered_targets: Vec<_> = targets.iter().collect();
    ordered_targets.sort_by_key(|(_, rect)| std::cmp::Reverse(rect.area()));

    for _ in 0..original.len().saturating_mul(original.len()).max(1) {
        let current_tab = mux
            .sidebar_tabs()?
            .into_iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or_else(|| anyhow!("Zellij tab {tab_id} disappeared while restoring pane order"))?;
        let current: std::collections::HashMap<String, PaneRect> = current_tab
            .panes
            .iter()
            .filter(|pane| targets.contains_key(&pane.pane_id))
            .filter_map(|pane| pane_rect(pane).map(|rect| (pane.pane_id.clone(), rect)))
            .collect();
        let Some((pane_id, target, rect, direction)) =
            ordered_targets.iter().find_map(|(pane_id, target)| {
                let rect = current.get(*pane_id)?;
                move_direction(*rect, **target)
                    .map(|direction| ((*pane_id).clone(), **target, *rect, direction))
            })
        else {
            return Ok(());
        };
        mux.move_pane_by_id(&pane_id, direction)?;
        let updated = mux
            .sidebar_tabs()?
            .into_iter()
            .find(|tab| tab.tab_id == tab_id)
            .and_then(|tab| tab.panes.into_iter().find(|pane| pane.pane_id == pane_id))
            .and_then(|pane| pane_rect(&pane));
        if updated == Some(rect) {
            bail!(
                "Zellij could not move pane {pane_id} {direction} toward its original slot {target:?}"
            );
        }
    }
    bail!("Zellij pane order did not converge after layout override")
}

fn create_sidebar_in_tab(
    instance_id: &str,
    tab: &ZellijSidebarTab,
    position: SidebarPosition,
    config: &Config,
    replace_existing: bool,
) -> Result<()> {
    let mux = backend(instance_id);
    if let Some(sidebar) = tab.panes.iter().find(|pane| is_sidebar_pane(&pane.title)) {
        if !replace_existing {
            return Ok(());
        }
        mux.close_pane_by_id(&sidebar.pane_id)?;
        wait_for_pane_to_close(instance_id, &sidebar.pane_id)?;
    }
    let Some(target) = tab
        .panes
        .iter()
        .find(|pane| !is_sidebar_pane(&pane.title) && pane.is_focused)
        .or_else(|| tab.panes.iter().find(|pane| !is_sidebar_pane(&pane.title)))
    else {
        return Ok(());
    };
    let original_pane_ids: HashSet<String> = tab
        .panes
        .iter()
        .filter(|pane| !is_sidebar_pane(&pane.title))
        .map(|pane| pane.pane_id.clone())
        .collect();
    let original_pane_rects: std::collections::HashMap<String, PaneRect> = tab
        .panes
        .iter()
        .filter(|pane| !is_sidebar_pane(&pane.title))
        .filter_map(|pane| pane_rect(pane).map(|rect| (pane.pane_id.clone(), rect)))
        .collect();
    let extent = tab_extent(tab, position)
        .ok_or_else(|| anyhow!("Zellij tab {} has no pane geometry", tab.tab_id))?;
    let size = sidebar_size(config, position, extent);
    let executable = std::env::current_exe()?;
    let executable = executable
        .to_str()
        .ok_or_else(|| anyhow!("exe path not UTF-8"))?;
    let cwd = std::env::current_dir()?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow!("current directory path not UTF-8"))?;

    debug!(
        tab_id = tab.tab_id,
        ?position,
        "overriding Zellij tab layout"
    );
    mux.go_to_tab_by_id(tab.tab_id)?;
    let dumped = mux.dump_layout()?;
    let layout = layout_with_sidebar(&dumped, tab.position, position, size, executable, cwd)?;
    mux.override_active_tab_layout(&layout)
        .with_context(|| format!("failed to create sidebar in Zellij tab {}", tab.tab_id))?;
    close_generated_terminal_placeholders(instance_id, tab.tab_id, &original_pane_ids)?;
    restore_pane_slots(instance_id, tab.tab_id, &original_pane_rects)?;
    let _ = mux.focus_pane_by_id(&target.pane_id);
    Ok(())
}

pub(super) fn create_sidebars_in_all_tabs(
    instance_id: &str,
    position: SidebarPosition,
    resize_existing: bool,
) -> Result<()> {
    let state = super::session_state::read(instance_id)?;
    let mut config = Config::load(None)?;
    config.sidebar.width = state.width;
    config.sidebar.height = state.height;
    let tabs = backend(instance_id).sidebar_tabs()?;
    let original_tab = tabs.iter().find(|tab| tab.active).map(|tab| tab.tab_id);
    let mut errors = Vec::new();
    for tab in &tabs {
        if let Err(error) =
            create_sidebar_in_tab(instance_id, tab, position, &config, resize_existing)
        {
            errors.push(format!("tab {}: {error}", tab.tab_id));
        }
    }
    if let Some(tab_id) = original_tab {
        let _ = backend(instance_id).go_to_tab_by_id(tab_id);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to create sidebars in {} Zellij tab(s): {}",
            errors.len(),
            errors.join("; ")
        ))
    }
}

pub(super) fn current_tab_has_sidebar(instance_id: &str) -> Result<bool> {
    Ok(backend(instance_id)
        .sidebar_tabs()?
        .into_iter()
        .find(|tab| tab.active)
        .is_some_and(|tab| tab.panes.iter().any(|pane| is_sidebar_pane(&pane.title))))
}

pub(super) fn list_sidebar_panes(instance_id: &str) -> Result<Vec<(String, String)>> {
    Ok(backend(instance_id)
        .sidebar_tabs()?
        .into_iter()
        .flat_map(|tab| {
            let window_id = tab.tab_id.to_string();
            tab.panes
                .into_iter()
                .filter(|pane| is_sidebar_pane(&pane.title))
                .map(move |pane| (window_id.clone(), pane.pane_id))
        })
        .collect())
}

pub(super) fn kill_all_sidebars(instance_id: &str, except_pane: Option<&str>) {
    let mux = backend(instance_id);
    let panes = list_sidebar_panes(instance_id).unwrap_or_default();
    for (_, pane_id) in panes {
        if except_pane == Some(pane_id.as_str()) {
            continue;
        }
        let _ = mux.close_pane_by_id(&pane_id);
    }
}

pub(super) fn sidebar_is_only_pane(instance_id: &str, window_id: &str, pane_id: &str) -> bool {
    backend(instance_id).sidebar_tabs().is_ok_and(|tabs| {
        tabs.into_iter()
            .find(|tab| tab.tab_id.to_string() == window_id)
            .is_some_and(|tab| tab.panes.len() == 1 && tab.panes[0].pane_id == pane_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_the_reserved_pane_name() {
        assert!(is_sidebar_pane("workmux-sidebar"));
        assert!(!is_sidebar_pane("workmux sidebar"));
        assert!(!is_sidebar_pane("agent"));
    }

    #[test]
    fn fixed_sidebar_size_accounts_for_zellij_separator() {
        let mut config = Config::default();
        config.sidebar.width = Some(SidebarWidth::Absolute(24));
        config.sidebar.height = Some(SidebarHeight::Absolute(4));
        assert_eq!(
            sidebar_size(&config, SidebarPosition::Left, 80),
            KdlValue::Integer(23)
        );
        assert_eq!(
            sidebar_size(&config, SidebarPosition::Top, 22),
            KdlValue::Integer(4)
        );
    }

    #[test]
    fn maps_scaled_slots_back_to_original_panes() {
        let original = std::collections::HashMap::from([
            (
                "left".to_string(),
                PaneRect {
                    x: 0,
                    y: 1,
                    width: 40,
                    height: 22,
                },
            ),
            (
                "right-top".to_string(),
                PaneRect {
                    x: 40,
                    y: 1,
                    width: 40,
                    height: 11,
                },
            ),
            (
                "right-bottom".to_string(),
                PaneRect {
                    x: 40,
                    y: 12,
                    width: 40,
                    height: 11,
                },
            ),
        ]);
        let current = std::collections::HashMap::from([
            (
                "right-top".to_string(),
                PaneRect {
                    x: 24,
                    y: 1,
                    width: 28,
                    height: 22,
                },
            ),
            (
                "right-bottom".to_string(),
                PaneRect {
                    x: 52,
                    y: 1,
                    width: 28,
                    height: 11,
                },
            ),
            (
                "left".to_string(),
                PaneRect {
                    x: 52,
                    y: 12,
                    width: 28,
                    height: 11,
                },
            ),
        ]);
        let targets = target_slots(&original, &current).unwrap();
        assert_eq!(targets["left"], current["right-top"]);
        assert_eq!(targets["right-top"], current["right-bottom"]);
        assert_eq!(targets["right-bottom"], current["left"]);
    }

    #[test]
    fn chooses_direction_toward_target_slot() {
        let left = PaneRect {
            x: 24,
            y: 1,
            width: 28,
            height: 22,
        };
        let top_right = PaneRect {
            x: 52,
            y: 1,
            width: 28,
            height: 11,
        };
        let bottom_right = PaneRect {
            x: 52,
            y: 12,
            width: 28,
            height: 11,
        };
        assert_eq!(move_direction(bottom_right, left), Some("left"));
        assert_eq!(move_direction(bottom_right, top_right), Some("up"));
        assert_eq!(move_direction(left, left), None);
    }

    #[test]
    fn wraps_nested_content_without_flattening_it() {
        let dumped = r#"
            layout {
                tab name="one" focus=true split_direction="horizontal" {
                    pane size=1 borderless=true {
                        plugin location="zellij:tab-bar"
                    }
                    pane split_direction="vertical" {
                        pane size="70%"
                        pane command="sleep" cwd="/tmp" size="30%" {
                            args "300"
                            start_suspended true
                        }
                    }
                    pane size=2 borderless=true {
                        plugin location="zellij:status-bar"
                    }
                }
            }
        "#;
        let output = layout_with_sidebar(
            dumped,
            0,
            SidebarPosition::Left,
            KdlValue::Integer(24),
            "/tmp/workmux",
            "/tmp",
        )
        .unwrap();
        let document = KdlDocument::parse_v1(&output).unwrap();
        let tab = document
            .get("layout")
            .and_then(KdlNode::children)
            .and_then(|children| children.get("tab"))
            .unwrap();
        let children = tab.children().unwrap().nodes();
        assert_eq!(children.len(), 3);
        let wrapper = &children[1];
        assert_eq!(
            wrapper.get("split_direction").and_then(KdlValue::as_string),
            Some("vertical")
        );
        let wrapper_children = wrapper.children().unwrap().nodes();
        assert_eq!(
            wrapper_children[0].get("size"),
            Some(&KdlValue::Integer(24))
        );
        assert_eq!(
            wrapper_children[1]
                .get("split_direction")
                .and_then(KdlValue::as_string),
            Some("vertical")
        );
        assert_eq!(wrapper_children[1].children().unwrap().nodes().len(), 2);
        let retained_command = &wrapper_children[1].children().unwrap().nodes()[1];
        assert!(retained_command.get("command").is_none(), "{output}");
        assert!(retained_command.get("cwd").is_none(), "{output}");
        assert!(retained_command.children().is_none(), "{output}");
    }

    #[test]
    fn selects_active_tab_when_names_are_duplicated() {
        let dumped = r#"
            layout {
                tab name="same" {
                    pane
                }
                tab name="same" focus=true {
                    pane split_direction="vertical" {
                        pane
                        pane
                    }
                }
            }
        "#;
        let output = layout_with_sidebar(
            dumped,
            0,
            SidebarPosition::Top,
            KdlValue::Integer(4),
            "/tmp/workmux",
            "/tmp",
        )
        .unwrap();
        let document = KdlDocument::parse_v1(&output).unwrap();
        let tab = document
            .get("layout")
            .and_then(KdlNode::children)
            .and_then(|children| children.get("tab"))
            .unwrap();
        let wrapper = &tab.children().unwrap().nodes()[0];
        assert_eq!(
            wrapper.get("split_direction").and_then(KdlValue::as_string),
            Some("horizontal")
        );
        assert_eq!(
            wrapper.children().unwrap().nodes()[1]
                .get("split_direction")
                .and_then(KdlValue::as_string),
            Some("vertical")
        );
    }
}
