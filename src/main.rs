mod diagnostics;
mod graphics;
mod x11;

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let diagnostics_only = std::env::args().any(|arg| arg == "--diagnostics");
    let connection = x11::connection::X11Connection::connect()?;
    diagnostics::print_x11(&connection)?;

    if diagnostics_only {
        let graphics = graphics::egl::EglContext::diagnostics(&connection)?;
        graphics.print();
        return Ok(());
    }

    let mut graphics = graphics::egl::EglContext::create(connection)?;
    graphics.print();
    graphics.render();
    graphics.swap_buffers()?;
    graphics.run_event_loop()?;
    Ok(())
}
