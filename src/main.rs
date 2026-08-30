mod diagnostics;
mod config;
mod graphics;
mod x11;

use std::error::Error;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
struct ShadowCliArgs {
    enabled: bool,
    color: Option<String>,
    extent: Option<f32>,
    offset_x: Option<f32>,
    offset_y: Option<f32>,
    strength: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
struct OpacityCliArgs {
    focused: Option<f32>,
    inactive: Option<f32>,
    urgent: Option<f32>,
}

fn parse_opacity_arguments(args: &[String]) -> Result<OpacityCliArgs, Box<dyn Error>> {
    let mut parsed = OpacityCliArgs { focused: None, inactive: None, urgent: None };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--compositor-opacity-focused" => {
                index += 1;
                parsed.focused = Some(args.get(index).ok_or(
                    "--compositor-opacity-focused requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-opacity-inactive" => {
                index += 1;
                parsed.inactive = Some(args.get(index).ok_or(
                    "--compositor-opacity-inactive requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-opacity-urgent" => {
                index += 1;
                parsed.urgent = Some(args.get(index).ok_or(
                    "--compositor-opacity-urgent requires FLOAT",
                )?.parse::<f32>()?);
            }
            _ => {}
        }
        index += 1;
    }
    Ok(parsed)
}

fn apply_opacity_config(
    config: config::CompositorConfig,
    args: &OpacityCliArgs,
) -> Result<config::CompositorConfig, Box<dyn Error>> {
    if args.focused.is_none() && args.inactive.is_none() && args.urgent.is_none() {
        return Ok(config);
    }
    let defaults = config.visuals.opacity;
    Ok(config.with_opacity(
        args.focused.unwrap_or(defaults.focused),
        args.inactive.unwrap_or(defaults.inactive),
        args.urgent.unwrap_or(defaults.urgent),
    )?)
}

fn parse_shadow_arguments(args: &[String]) -> Result<ShadowCliArgs, Box<dyn Error>> {
    let mut parsed = ShadowCliArgs {
        enabled: false,
        color: None,
        extent: None,
        offset_x: None,
        offset_y: None,
        strength: None,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--compositor-shadow-enabled" => parsed.enabled = true,
            "--compositor-shadow-color" => {
                index += 1;
                parsed.color = Some(args.get(index).ok_or(
                    "--compositor-shadow-color requires RRGGBB",
                )?.clone());
            }
            "--compositor-shadow-extent" => {
                index += 1;
                parsed.extent = Some(args.get(index).ok_or(
                    "--compositor-shadow-extent requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-shadow-offset-x" => {
                index += 1;
                parsed.offset_x = Some(args.get(index).ok_or(
                    "--compositor-shadow-offset-x requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-shadow-offset-y" => {
                index += 1;
                parsed.offset_y = Some(args.get(index).ok_or(
                    "--compositor-shadow-offset-y requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-shadow-strength" => {
                index += 1;
                parsed.strength = Some(args.get(index).ok_or(
                    "--compositor-shadow-strength requires FLOAT",
                )?.parse::<f32>()?);
            }
            _ => {}
        }
        index += 1;
    }
    Ok(parsed)
}

fn apply_shadow_config(
    config: config::CompositorConfig,
    args: &ShadowCliArgs,
) -> Result<config::CompositorConfig, Box<dyn Error>> {
    if !args.enabled && args.color.is_none() && args.extent.is_none()
        && args.offset_x.is_none() && args.offset_y.is_none() && args.strength.is_none()
    {
        return Ok(config);
    }
    let defaults = config.visuals.shadow;
    let color = args.color.as_deref()
        .map(config::CompositorConfig::parse_rgb_color)
        .transpose()?
        .unwrap_or(defaults.color);
    Ok(config.with_shadow(
        args.enabled || defaults.enabled,
        color,
        args.extent.unwrap_or(defaults.extent),
        args.offset_x.unwrap_or(defaults.offset_x),
        args.offset_y.unwrap_or(defaults.offset_y),
        args.strength.unwrap_or(defaults.strength),
    )?)
}

fn scene_config_from_cli(
    base: config::CompositorConfig,
    corner_radius: Option<f32>,
    border_width: Option<f32>,
    border_color: Option<&str>,
    inactive_color: Option<&str>,
    focused_color: Option<&str>,
    urgent_color: Option<&str>,
    shadow_args: &ShadowCliArgs,
    opacity_args: &OpacityCliArgs,
) -> Result<config::CompositorConfig, Box<dyn Error>> {
    let mut config = base;
    if let Some(radius) = corner_radius {
        config.visuals.corner_radius = config::CompositorConfig::with_corner_radius(radius)?.visuals.corner_radius;
    }
    if border_width.is_some() || border_color.is_some() || inactive_color.is_some()
        || focused_color.is_some() || urgent_color.is_some()
    {
        let defaults = config.visuals.border;
        let legacy_color = border_color.map(config::CompositorConfig::parse_color).transpose()?;
        let inactive = inactive_color.map(config::CompositorConfig::parse_color).transpose()?.or(legacy_color).unwrap_or(defaults.inactive_color);
        let focused = focused_color.map(config::CompositorConfig::parse_color).transpose()?.or(legacy_color).unwrap_or(defaults.focused_color);
        let urgent = urgent_color.map(config::CompositorConfig::parse_color).transpose()?.or(legacy_color).unwrap_or(defaults.urgent_color);
        config = config.with_border_colors(border_width.unwrap_or(defaults.width), inactive, focused, urgent)?;
    }
    if shadow_args.enabled || shadow_args.color.is_some() || shadow_args.extent.is_some()
        || shadow_args.offset_x.is_some() || shadow_args.offset_y.is_some() || shadow_args.strength.is_some()
    {
        config = apply_shadow_config(config, shadow_args)?;
    }
    apply_opacity_config(config, opacity_args)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let shadow_args = parse_shadow_arguments(&raw_args)?;
    let opacity_args = parse_opacity_arguments(&raw_args)?;
    let mut args = raw_args.into_iter();
    let mut diagnostics_only = false;
    let mut capture_window = None;
    let mut compositor_probe = false;
    let mut claim_compositor = false;
    let mut compositor_capture_window = None;
    let mut compositor_hierarchy_probe_window = None;
    let mut compositor_tree_snapshot = false;
    let mut compositor_tree_watch = false;
    let mut compositor_takeover_preflight = false;
    let mut compositor_overlay_probe = None;
    let mut compositor_manual_probe = None;
    let mut compositor_scene_x11_probe = None;
    let mut config_path: Option<PathBuf> = None;
    let mut compositor_corner_radius = None;
    let mut compositor_border_width = None;
    let mut compositor_border_color = None;
    let mut compositor_border_inactive_color = None;
    let mut compositor_border_focused_color = None;
    let mut compositor_border_urgent_color = None;
    let mut compositor_shadow_enabled = false;
    let mut compositor_shadow_color = None;
    let mut compositor_shadow_extent = None;
    let mut compositor_shadow_offset_x = None;
    let mut compositor_shadow_offset_y = None;
    let mut compositor_shadow_strength = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(PathBuf::from(args.next().ok_or("--config requires PATH")?));
            }
            value if value.starts_with("--config=") => {
                let path = value.strip_prefix("--config=").unwrap_or_default();
                if path.is_empty() {
                    return Err("--config= requires PATH".into());
                }
                config_path = Some(PathBuf::from(path));
            }
            "--diagnostics" => diagnostics_only = true,
            "--compositor-probe" => compositor_probe = true,
            "--claim-compositor" => claim_compositor = true,
            "--compositor-capture" => {
                compositor_capture_window =
                    Some(args.next().ok_or("--compositor-capture requires WINDOW_ID")?)
            }
            "--compositor-hierarchy-probe" => {
                compositor_hierarchy_probe_window = Some(
                    args.next()
                        .ok_or("--compositor-hierarchy-probe requires WINDOW_ID")?,
                )
            }
            "--compositor-tree-snapshot" => compositor_tree_snapshot = true,
            "--compositor-tree-watch" => compositor_tree_watch = true,
            "--compositor-takeover-preflight" => compositor_takeover_preflight = true,
            "--compositor-overlay-probe" => {
                compositor_overlay_probe = Some(
                    args.next()
                        .ok_or("--compositor-overlay-probe requires EXPECTED_ROOT_XID")?,
                )
            }
            "--compositor-manual-probe" => {
                compositor_manual_probe = Some(
                    args.next()
                        .ok_or("--compositor-manual-probe requires EXPECTED_ROOT_XID")?,
                )
            }
            "--compositor-scene-x11-probe" => {
                compositor_scene_x11_probe = Some(
                    args.next()
                        .ok_or("--compositor-scene-x11-probe requires EXPECTED_ROOT_XID")?,
                )
            }
            "--compositor-corner-radius" => {
                compositor_corner_radius = Some(args.next().ok_or(
                    "--compositor-corner-radius requires RADIUS",
                )?.parse::<f32>()?);
            }
            "--compositor-border-width" => {
                compositor_border_width = Some(args.next().ok_or(
                    "--compositor-border-width requires WIDTH",
                )?.parse::<f32>()?);
            }
            "--compositor-border-color" => {
                compositor_border_color = Some(args.next().ok_or(
                    "--compositor-border-color requires RRGGBB or RRGGBBAA",
                )?);
            }
            "--compositor-border-inactive-color" => {
                compositor_border_inactive_color = Some(args.next().ok_or(
                    "--compositor-border-inactive-color requires RRGGBB or RRGGBBAA",
                )?);
            }
            "--compositor-border-focused-color" => {
                compositor_border_focused_color = Some(args.next().ok_or(
                    "--compositor-border-focused-color requires RRGGBB or RRGGBBAA",
                )?);
            }
            "--compositor-border-urgent-color" => {
                compositor_border_urgent_color = Some(args.next().ok_or(
                    "--compositor-border-urgent-color requires RRGGBB or RRGGBBAA",
                )?);
            }
            "--compositor-shadow-enabled" => compositor_shadow_enabled = true,
            "--compositor-shadow-color" => {
                compositor_shadow_color = Some(args.next().ok_or(
                    "--compositor-shadow-color requires RRGGBB",
                )?);
            }
            "--compositor-shadow-extent" => {
                compositor_shadow_extent = Some(args.next().ok_or(
                    "--compositor-shadow-extent requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-shadow-offset-x" => {
                compositor_shadow_offset_x = Some(args.next().ok_or(
                    "--compositor-shadow-offset-x requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-shadow-offset-y" => {
                compositor_shadow_offset_y = Some(args.next().ok_or(
                    "--compositor-shadow-offset-y requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-shadow-strength" => {
                compositor_shadow_strength = Some(args.next().ok_or(
                    "--compositor-shadow-strength requires FLOAT",
                )?.parse::<f32>()?);
            }
            "--compositor-opacity-focused"
            | "--compositor-opacity-inactive"
            | "--compositor-opacity-urgent" => {
                let _ = args.next().ok_or("opacity option requires FLOAT")?;
            }
            "--capture" => {
                capture_window = Some(args.next().ok_or("--capture requires WINDOW_ID")?)
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    let _ = (
        &compositor_shadow_enabled,
        &compositor_shadow_color,
        &compositor_shadow_extent,
        &compositor_shadow_offset_x,
        &compositor_shadow_offset_y,
        &compositor_shadow_strength,
    );
    let startup_config = match config::load_startup_config(config::StartupConfigRequest {
        explicit_path: config_path,
        environment: config::ConfigPathEnvironment::from_process(),
    })? {
        config::ConfigLoadOutcome::DefaultsBecauseMissingImplicitFile => {
            std::sync::Arc::new(config::ValidatedConfig::default())
        }
        config::ConfigLoadOutcome::Loaded { config, .. } => config,
    };
    let connection = x11::connection::X11Connection::connect()?;

    if let Some(value) = compositor_scene_x11_probe {
        if compositor_probe
            || claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
            || compositor_tree_snapshot
            || compositor_tree_watch
            || compositor_takeover_preflight
            || compositor_overlay_probe.is_some()
            || compositor_manual_probe.is_some()
        {
            return Err("--compositor-scene-x11-probe cannot be combined with another mode".into());
        }
        let config = scene_config_from_cli(
            config::CompositorConfig { visuals: startup_config.visuals, blur_enabled: startup_config.blur_enabled },
            compositor_corner_radius,
            compositor_border_width,
            compositor_border_color.as_deref(),
            compositor_border_inactive_color.as_deref(),
            compositor_border_focused_color.as_deref(),
            compositor_border_urgent_color.as_deref(),
            &shadow_args,
            &opacity_args,
        )?;
        return x11::scene::run(&connection, &value, config);
    }

    if let Some(value) = compositor_manual_probe {
        if compositor_probe
            || claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
            || compositor_tree_snapshot
            || compositor_tree_watch
            || compositor_takeover_preflight
            || compositor_overlay_probe.is_some()
        {
            return Err("--compositor-manual-probe cannot be combined with another mode".into());
        }
        return x11::manual::run(&connection, &value);
    }

    if let Some(value) = compositor_overlay_probe {
        if compositor_probe
            || claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
            || compositor_tree_snapshot
            || compositor_tree_watch
            || compositor_takeover_preflight
        {
            return Err(
                "--compositor-overlay-probe cannot be combined with another mode".into(),
            );
        }
        x11::overlay::run(&connection, &value)?;
        return Ok(());
    }

    if compositor_takeover_preflight {
        if compositor_probe
            || claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
            || compositor_tree_snapshot
            || compositor_tree_watch
        {
            return Err(
                "--compositor-takeover-preflight cannot be combined with another mode".into(),
            );
        }
        x11::preflight::run(&connection)?;
        return Ok(());
    }

    if compositor_tree_watch {
        if compositor_probe
            || claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
            || compositor_tree_snapshot
        {
            return Err("--compositor-tree-watch cannot be combined with another mode".into());
        }
        x11::tree_watch::run(&connection)?;
        return Ok(());
    }

    if compositor_tree_snapshot {
        if compositor_probe
            || claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
        {
            return Err("--compositor-tree-snapshot cannot be combined with another mode".into());
        }
        let snapshot = connection.snapshot_hierarchy()?;
        x11::tree::print_snapshot(&snapshot);
        return Ok(());
    }

    if compositor_probe {
        if claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
        {
            return Err("--compositor-probe cannot be combined with another mode".into());
        }
        x11::compositor::probe(&connection)?;
        return Ok(());
    }
    if claim_compositor {
        if diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
            || compositor_hierarchy_probe_window.is_some()
        {
            return Err("--claim-compositor cannot be combined with another mode".into());
        }
        let ownership = x11::compositor::CompositorOwnership::claim(&connection)?;
        ownership.run_event_loop(&connection)?;
        return Ok(());
    }
    if let Some(value) = compositor_hierarchy_probe_window {
        if diagnostics_only || capture_window.is_some() || compositor_capture_window.is_some() {
            return Err(
                "--compositor-hierarchy-probe cannot be combined with another mode".into(),
            );
        }
        return run_compositor_hierarchy_probe(&connection, &value);
    }
    if let Some(value) = compositor_capture_window {
        if diagnostics_only || capture_window.is_some() {
            return Err("--compositor-capture cannot be combined with another mode".into());
        }
        return run_compositor_capture(&connection, &value);
    }
    diagnostics::print_x11(&connection)?;

    if diagnostics_only {
        let graphics = graphics::egl::EglContext::diagnostics(&connection)?;
        graphics.print();
        return Ok(());
    }

    if capture_window.is_none() {
        let config = scene_config_from_cli(
            config::CompositorConfig { visuals: startup_config.visuals, blur_enabled: startup_config.blur_enabled },
            compositor_corner_radius,
            compositor_border_width,
            compositor_border_color.as_deref(),
            compositor_border_inactive_color.as_deref(),
            compositor_border_focused_color.as_deref(),
            compositor_border_urgent_color.as_deref(),
            &shadow_args,
            &opacity_args,
        )?;
        return x11::scene::run_with_root(&connection, connection.screen_root(), config);
    }

    let capture = capture_window
        .map(|value| connection.capture_window(&value))
        .transpose()?;
    let mut graphics = graphics::egl::EglContext::create(&connection)?;
    if let Some(capture) = capture {
        graphics.import_pixmap(capture)?;
    }
    graphics.print();
    graphics.render();
    graphics.swap_buffers()?;
    connection.run_event_loop(&mut graphics, None, None)?;
    Ok(())
}

fn run_compositor_hierarchy_probe(
    connection: &x11::connection::X11Connection,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let ownership = x11::compositor::CompositorOwnership::claim(connection)?;
    let _version = match connection.require_composite() {
        Ok(version) => version,
        Err(error) => {
            if let Err(cleanup_error) = ownership.release(connection) {
                eprintln!("compositor ownership cleanup failed: {cleanup_error}");
            }
            return Err(error);
        }
    };
    let info = match connection.inspect_hierarchy_probe(value) {
        Ok(info) => info,
        Err(error) => {
            if let Err(cleanup_error) = ownership.release(connection) {
                eprintln!("compositor ownership cleanup failed: {cleanup_error}");
            }
            return Err(error);
        }
    };
    x11::connection::X11Connection::print_hierarchy_probe(&info);
    if info.root != ownership.root {
        let error = format!(
            "hierarchy root does not match compositor ownership root: hierarchy 0x{:08x}, ownership 0x{:08x}",
            info.root, ownership.root
        );
        if let Err(cleanup_error) = ownership.release(connection) {
            eprintln!("compositor ownership cleanup failed: {cleanup_error}");
        }
        return Err(error.into());
    }
    let redirected = match x11::compositor::RedirectedSubwindows::redirect(
        connection,
        info.root,
    ) {
        Ok(redirected) => redirected,
        Err(error) => {
            if let Err(cleanup_error) = ownership.release(connection) {
                eprintln!("compositor ownership cleanup failed: {cleanup_error}");
            }
            return Err(error);
        }
    };

    println!("\nNameWindowPixmap probes:");
    let mut probe_error = None;
    for (label, window) in [
        ("direct root child", info.direct_root_child),
        ("requested client", info.client),
    ] {
        if let Err(error) = connection.probe_name_window_pixmap(label, window) {
            if probe_error.is_none() {
                probe_error = Some(error);
            }
            break;
        }
    }

    let unredirect_error = redirected.unredirect(connection).err();
    let release_error = ownership.release(connection).err();
    if let Some(error) = probe_error {
        if let Some(cleanup_error) = unredirect_error {
            eprintln!("CompositeUnredirectSubwindows cleanup failed: {cleanup_error}");
        }
        if let Some(cleanup_error) = release_error {
            eprintln!("compositor ownership cleanup failed: {cleanup_error}");
        }
        return Err(error);
    }
    if let Some(error) = unredirect_error {
        if let Some(cleanup_error) = release_error {
            eprintln!("compositor ownership cleanup failed: {cleanup_error}");
        }
        return Err(error);
    }
    if let Some(error) = release_error {
        return Err(error);
    }
    println!("compositor ownership released");
    Ok(())
}

fn run_compositor_capture(
    connection: &x11::connection::X11Connection,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let ownership = x11::compositor::CompositorOwnership::claim(connection)?;
    let (window, width, height, role) = match connection.prepare_capture_window(value) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Err(cleanup_error) = ownership.release(connection) {
                eprintln!("compositor ownership cleanup failed: {cleanup_error}");
            }
            return Err(error);
        }
    };
    let redirect_target = match x11::compositor::redirect_target(window, role) {
        Some(target) => target,
        None => {
            if let Err(cleanup_error) = ownership.release(connection) {
                eprintln!("compositor ownership cleanup failed: {cleanup_error}");
            }
            return Err(format!("compositor capture requires a Client window; requested role: {role:?}").into());
        }
    };

    let redirected = match x11::compositor::RedirectedWindow::redirect(connection, redirect_target) {
        Ok(redirected) => redirected,
        Err(error) => {
            if let Err(cleanup_error) = ownership.release(connection) {
                eprintln!("compositor ownership cleanup failed: {cleanup_error}");
            }
            return Err(error);
        }
    };
    let mut graphics = match graphics::egl::EglContext::create(connection) {
        Ok(graphics) => graphics,
        Err(error) => {
            cleanup_compositor_capture(connection, None, &redirected, &ownership);
            return Err(error);
        }
    };
    let capture = match connection.capture_pixmap(redirect_target, width, height) {
        Ok(capture) => capture,
        Err(error) => {
            cleanup_compositor_capture(connection, Some(&mut graphics), &redirected, &ownership);
            return Err(error);
        }
    };
    if let Err(error) = graphics.import_pixmap(capture) {
        cleanup_compositor_capture(connection, Some(&mut graphics), &redirected, &ownership);
        return Err(error);
    }
    graphics.print();
    graphics.render();
    if let Err(error) = graphics.swap_buffers() {
        cleanup_compositor_capture(connection, Some(&mut graphics), &redirected, &ownership);
        return Err(error);
    }
    let result = connection.run_event_loop(&mut graphics, Some(&ownership), Some(&redirected));
    if let Err(error) = result {
        cleanup_compositor_capture(connection, Some(&mut graphics), &redirected, &ownership);
        return Err(error);
    }
    Ok(())
}

fn cleanup_compositor_capture(
    connection: &x11::connection::X11Connection,
    graphics: Option<&mut graphics::egl::EglContext<'_>>,
    redirected: &x11::compositor::RedirectedWindow,
    ownership: &x11::compositor::CompositorOwnership,
) {
    if let Some(graphics) = graphics {
        graphics.destroy_capture();
    }
    if let Err(error) = redirected.unredirect(connection) {
        eprintln!("redirection cleanup failed: {error}");
    }
    if let Err(error) = ownership.release(connection) {
        eprintln!("compositor ownership cleanup failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_opacity_config, apply_shadow_config, parse_opacity_arguments, parse_shadow_arguments};
    use crate::config::CompositorConfig;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn shadow_cli_parser_reads_all_runtime_parameters() {
        let parsed = parse_shadow_arguments(&args(&[
            "program", "--compositor-shadow-enabled", "--compositor-shadow-color", "4C7899",
            "--compositor-shadow-extent", "18", "--compositor-shadow-offset-x", "-3",
            "--compositor-shadow-offset-y", "4", "--compositor-shadow-strength", "0.28",
        ])).unwrap();
        assert_eq!(parsed.enabled, true);
        assert_eq!(parsed.color.as_deref(), Some("4C7899"));
        assert_eq!(parsed.extent, Some(18.0));
        assert_eq!(parsed.offset_x, Some(-3.0));
        assert_eq!(parsed.offset_y, Some(4.0));
        assert_eq!(parsed.strength, Some(0.28));
    }

    #[test]
    fn shadow_cli_values_reach_shadow_config_and_defaults_stay_disabled() {
        let defaults = apply_shadow_config(
            CompositorConfig::defaults(),
            &parse_shadow_arguments(&args(&["program"])).unwrap(),
        ).unwrap();
        assert!(!defaults.visuals.shadow.enabled);
        let parsed = parse_shadow_arguments(&args(&[
            "program", "--compositor-shadow-enabled", "--compositor-shadow-color", "4C7899",
            "--compositor-shadow-extent", "18", "--compositor-shadow-offset-x", "-3",
            "--compositor-shadow-offset-y", "4", "--compositor-shadow-strength", "0.28",
        ])).unwrap();
        let config = apply_shadow_config(CompositorConfig::defaults(), &parsed).unwrap();
        assert!(config.visuals.shadow.enabled);
        assert_eq!(config.visuals.shadow.color, [0x4c, 0x78, 0x99]);
        assert_eq!(config.visuals.shadow.extent, 18.0);
        assert_eq!(config.visuals.shadow.offset_x, -3.0);
        assert_eq!(config.visuals.shadow.offset_y, 4.0);
        assert_eq!(config.visuals.shadow.strength, 0.28);
    }

    #[test]
    fn shadow_cli_offsets_accept_negative_zero_and_positive_values() {
        for (x, y) in [(-4.0, -4.0), (0.0, 0.0), (4.0, 4.0)] {
            let parsed = parse_shadow_arguments(&args(&[
                "program", "--compositor-shadow-enabled", "--compositor-shadow-extent", "18",
                "--compositor-shadow-offset-x", &x.to_string(), "--compositor-shadow-offset-y",
                &y.to_string(), "--compositor-shadow-strength", "0.28",
            ])).unwrap();
            let config = apply_shadow_config(CompositorConfig::defaults(), &parsed).unwrap();
            assert_eq!(config.visuals.shadow.offset_x, x);
            assert_eq!(config.visuals.shadow.offset_y, y);
        }
    }

    #[test]
    fn shadow_cli_invalid_values_return_clean_errors() {
        for values in [
            &["program", "--compositor-shadow-color", "ZZZZZZ"][..],
            &["program", "--compositor-shadow-enabled", "--compositor-shadow-strength", "2"][..],
            &["program", "--compositor-shadow-enabled", "--compositor-shadow-extent", "0"][..],
        ] {
            let parsed = parse_shadow_arguments(&args(&values)).unwrap();
            assert!(apply_shadow_config(CompositorConfig::defaults(), &parsed).is_err());
        }
    }

    #[test]
    fn opacity_cli_parser_and_config_use_the_real_flag_names() {
        let parsed = parse_opacity_arguments(&args(&[
            "program", "--compositor-opacity-focused", "1.0",
            "--compositor-opacity-inactive", "0.92", "--compositor-opacity-urgent", "1.0",
        ])).unwrap();
        assert_eq!(parsed.focused, Some(1.0));
        assert_eq!(parsed.inactive, Some(0.92));
        assert_eq!(parsed.urgent, Some(1.0));
        let config = apply_opacity_config(CompositorConfig::defaults(), &parsed).unwrap();
        assert_eq!(config.visuals.opacity.focused, 1.0);
        assert_eq!(config.visuals.opacity.inactive, 0.92);
        assert_eq!(config.visuals.opacity.urgent, 1.0);
    }

    #[test]
    fn opacity_cli_defaults_are_neutral_and_values_are_validated() {
        let defaults = apply_opacity_config(
            CompositorConfig::defaults(),
            &parse_opacity_arguments(&args(&["program"])).unwrap(),
        ).unwrap();
        assert_eq!(defaults.visuals.opacity.focused, 1.0);
        assert_eq!(defaults.visuals.opacity.inactive, 1.0);
        assert_eq!(defaults.visuals.opacity.urgent, 1.0);
        for value in ["-0.1", "1.1", "NaN", "inf"] {
            let parsed = parse_opacity_arguments(&args(&[
                "program", "--compositor-opacity-inactive", value,
            ])).unwrap();
            assert!(apply_opacity_config(CompositorConfig::defaults(), &parsed).is_err());
        }
    }

    #[test]
    fn normal_scene_config_uses_existing_defaults_without_probe_argument() {
        let config = super::scene_config_from_cli(
            CompositorConfig::defaults(),
            None, None, None, None, None, None,
            &super::ShadowCliArgs { enabled: false, color: None, extent: None, offset_x: None, offset_y: None, strength: None },
            &super::OpacityCliArgs { focused: None, inactive: None, urgent: None },
        ).unwrap();
        assert_eq!(config, CompositorConfig::defaults());
    }

    #[test]
    fn normal_scene_config_keeps_visual_cli_overrides_without_probe_argument() {
        let config = super::scene_config_from_cli(
            CompositorConfig::defaults(),
            Some(16.0), Some(2.0), None, Some("555555"), Some("4C7899"), Some("FF3030"),
            &super::ShadowCliArgs { enabled: false, color: None, extent: None, offset_x: None, offset_y: None, strength: None },
            &super::OpacityCliArgs { focused: None, inactive: None, urgent: None },
        ).unwrap();
        assert_eq!(config.visuals.corner_radius, 16.0);
        assert_eq!(config.visuals.border.width, 2.0);
    }
}
