mod diagnostics;
mod graphics;
mod x11;

use std::error::Error;

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
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
    while let Some(arg) = args.next() {
        match arg.as_str() {
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
            "--capture" => {
                capture_window = Some(args.next().ok_or("--capture requires WINDOW_ID")?)
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    let connection = x11::connection::X11Connection::connect()?;

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
