use anyhow::{Context, Result, anyhow};
use tracing::debug;

use crate::cmd::Cmd;
use crate::config::{Config, SidebarPosition};
use crate::multiplexer::zellij::{ZellijBackend, ZellijSidebarPane, ZellijSidebarTab};

const SIDEBAR_PANE_NAME: &str = "workmux-sidebar";

fn is_sidebar_pane(title: &str) -> bool {
    title == SIDEBAR_PANE_NAME
}

fn backend(instance_id: &str) -> ZellijBackend {
    ZellijBackend::for_session(instance_id)
}

fn pane_extent(pane: &ZellijSidebarPane, position: SidebarPosition) -> Option<u16> {
    match position {
        SidebarPosition::Left => pane.pane_columns,
        SidebarPosition::Top => pane.pane_rows,
    }
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

fn resize_args(
    position: SidebarPosition,
    current: u16,
    target: u16,
) -> Option<(&'static str, &'static str)> {
    let direction = match position {
        SidebarPosition::Left => "right",
        SidebarPosition::Top => "down",
    };
    match current.cmp(&target) {
        std::cmp::Ordering::Less => Some(("increase", direction)),
        std::cmp::Ordering::Greater => Some(("decrease", direction)),
        std::cmp::Ordering::Equal => None,
    }
}

fn opposite_resize(resize: &str) -> &'static str {
    if resize == "increase" {
        "decrease"
    } else {
        "increase"
    }
}

fn resize_sidebar(
    instance_id: &str,
    tab_id: u32,
    pane_id: &str,
    position: SidebarPosition,
    config: &Config,
) -> Result<()> {
    for _ in 0..64 {
        let tabs = backend(instance_id).sidebar_tabs()?;
        let Some(tab) = tabs.into_iter().find(|tab| tab.tab_id == tab_id) else {
            return Ok(());
        };
        let Some(pane) = tab.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return Ok(());
        };
        let Some(total_extent) = tab_extent(&tab, position) else {
            return Ok(());
        };
        let Some(current_extent) = pane_extent(pane, position) else {
            return Ok(());
        };
        let maximum = total_extent.saturating_sub(1).max(1);
        let target_extent =
            super::effective_size_for(config, position, total_extent).clamp(1, maximum);
        let Some((resize, direction)) = resize_args(position, current_extent, target_extent) else {
            return Ok(());
        };
        let old_difference = current_extent.abs_diff(target_extent);

        Cmd::new("zellij")
            .args(&["--session", instance_id])
            .args(&["action", "resize", "--pane-id", pane_id, resize, direction])
            .run()
            .with_context(|| format!("failed to resize Zellij sidebar pane {pane_id}"))?;

        let updated_tabs = backend(instance_id).sidebar_tabs()?;
        let updated_extent = updated_tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .and_then(|tab| tab.panes.iter().find(|pane| pane.pane_id == pane_id))
            .and_then(|pane| pane_extent(pane, position));
        let Some(updated_extent) = updated_extent else {
            return Ok(());
        };
        let new_difference = updated_extent.abs_diff(target_extent);
        if new_difference >= old_difference {
            if new_difference > old_difference {
                let _ = Cmd::new("zellij")
                    .args(&["--session", instance_id])
                    .args(&[
                        "action",
                        "resize",
                        "--pane-id",
                        pane_id,
                        opposite_resize(resize),
                        direction,
                    ])
                    .run();
            }
            return Ok(());
        }
    }
    Ok(())
}

fn create_sidebar_in_tab(
    instance_id: &str,
    tab: &ZellijSidebarTab,
    position: SidebarPosition,
    config: &Config,
    resize_existing: bool,
) -> Result<()> {
    if let Some(sidebar) = tab.panes.iter().find(|pane| is_sidebar_pane(&pane.title)) {
        if resize_existing {
            resize_sidebar(instance_id, tab.tab_id, &sidebar.pane_id, position, config)?;
        }
        return Ok(());
    }
    let Some(target) = tab
        .panes
        .iter()
        .find(|pane| pane.is_focused)
        .or_else(|| tab.panes.first())
    else {
        return Ok(());
    };

    let exe = std::env::current_exe()?;
    let exe_str = exe.to_str().ok_or_else(|| anyhow!("exe path not UTF-8"))?;
    let (split_direction, move_direction) = match position {
        SidebarPosition::Left => ("right", "left"),
        SidebarPosition::Top => ("down", "up"),
    };
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| anyhow!("current directory path not UTF-8"))?;

    debug!(
        tab_id = tab.tab_id,
        target_pane = target.pane_id,
        ?position,
        "creating zellij sidebar"
    );

    Cmd::new("zellij")
        .args(&["--session", instance_id])
        .args(&[
            "action",
            "new-pane",
            "--tab-id",
            &tab.tab_id.to_string(),
            "--direction",
            split_direction,
            "--name",
            SIDEBAR_PANE_NAME,
            "--close-on-exit",
            "--cwd",
            cwd_str,
            "--",
            exe_str,
            "_sidebar-run",
        ])
        .run()
        .with_context(|| format!("failed to create sidebar in Zellij tab {}", tab.tab_id))?;

    for _ in 0..5 {
        if let Some(pane) = backend(instance_id)
            .sidebar_tabs()?
            .into_iter()
            .find(|candidate| candidate.tab_id == tab.tab_id)
            .and_then(|candidate| {
                candidate
                    .panes
                    .into_iter()
                    .find(|pane| is_sidebar_pane(&pane.title))
            })
        {
            let mut pane = pane;
            for _ in 0..tab.panes.len().max(1) {
                let at_edge = match position {
                    SidebarPosition::Left => pane.pane_x == Some(0),
                    SidebarPosition::Top => pane.pane_y == Some(0),
                };
                if at_edge {
                    break;
                }
                let _ = Cmd::new("zellij")
                    .args(&["--session", instance_id])
                    .args(&[
                        "action",
                        "move-pane",
                        "--pane-id",
                        &pane.pane_id,
                        move_direction,
                    ])
                    .run();
                let Some(updated) = backend(instance_id)
                    .sidebar_tabs()?
                    .into_iter()
                    .find(|candidate| candidate.tab_id == tab.tab_id)
                    .and_then(|candidate| {
                        candidate
                            .panes
                            .into_iter()
                            .find(|candidate| candidate.pane_id == pane.pane_id)
                    })
                else {
                    break;
                };
                if updated.pane_x == pane.pane_x && updated.pane_y == pane.pane_y {
                    break;
                }
                pane = updated;
            }
            if tab.active {
                let _ = Cmd::new("zellij")
                    .args(&["--session", instance_id])
                    .args(&["action", "focus-pane-id", &target.pane_id])
                    .run();
            }
            resize_sidebar(instance_id, tab.tab_id, &pane.pane_id, position, config)?;
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    Err(anyhow!(
        "Zellij created a sidebar in tab {} but did not report its pane",
        tab.tab_id
    ))
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
    let mut errors = Vec::new();
    for tab in &tabs {
        if let Err(error) =
            create_sidebar_in_tab(instance_id, tab, position, &config, resize_existing)
        {
            errors.push(format!("tab {}: {error}", tab.tab_id));
        }
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
    fn resize_uses_the_sidebar_content_border() {
        assert_eq!(
            resize_args(SidebarPosition::Left, 40, 25),
            Some(("decrease", "right"))
        );
        assert_eq!(
            resize_args(SidebarPosition::Top, 2, 4),
            Some(("increase", "down"))
        );
        assert_eq!(resize_args(SidebarPosition::Left, 25, 25), None);
    }
}
