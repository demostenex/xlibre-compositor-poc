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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--diagnostics" => diagnostics_only = true,
            "--compositor-probe" => compositor_probe = true,
            "--claim-compositor" => claim_compositor = true,
            "--compositor-capture" => {
                compositor_capture_window =
                    Some(args.next().ok_or("--compositor-capture requires WINDOW_ID")?)
            }
            "--capture" => {
                capture_window = Some(args.next().ok_or("--capture requires WINDOW_ID")?)
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    let connection = x11::connection::X11Connection::connect()?;

    if compositor_probe {
        if claim_compositor
            || diagnostics_only
            || capture_window.is_some()
            || compositor_capture_window.is_some()
        {
            return Err("--compositor-probe cannot be combined with another mode".into());
        }
        x11::compositor::probe(&connection)?;
        return Ok(());
    }
    if claim_compositor {
        if diagnostics_only || capture_window.is_some() || compositor_capture_window.is_some() {
            return Err("--claim-compositor cannot be combined with another mode".into());
        }
        let ownership = x11::compositor::CompositorOwnership::claim(&connection)?;
        ownership.run_event_loop(&connection)?;
        return Ok(());
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
