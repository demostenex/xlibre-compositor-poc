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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--diagnostics" => diagnostics_only = true,
            "--compositor-probe" => compositor_probe = true,
            "--claim-compositor" => claim_compositor = true,
            "--capture" => {
                capture_window = Some(args.next().ok_or("--capture requires WINDOW_ID")?)
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    let connection = x11::connection::X11Connection::connect()?;

    if compositor_probe {
        if claim_compositor || diagnostics_only || capture_window.is_some() {
            return Err("--compositor-probe cannot be combined with another mode".into());
        }
        x11::compositor::probe(&connection)?;
        return Ok(());
    }
    if claim_compositor {
        if diagnostics_only || capture_window.is_some() {
            return Err("--claim-compositor cannot be combined with another mode".into());
        }
        let ownership = x11::compositor::CompositorOwnership::claim(&connection)?;
        ownership.run_event_loop(&connection)?;
        return Ok(());
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
    connection.run_event_loop(&mut graphics)?;
    Ok(())
}
