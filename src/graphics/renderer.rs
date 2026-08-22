use std::ffi::CStr;

pub fn load<F>(loader: F)
where
    F: FnMut(&'static str) -> *const std::ffi::c_void,
{
    gl::load_with(loader);
}

pub fn resize(width: i32, height: i32) {
    unsafe { gl::Viewport(0, 0, width.max(1), height.max(1)); }
}

pub fn render() {
    unsafe {
        gl::ClearColor(0.08, 0.12, 0.20, 1.0);
        gl::Clear(gl::COLOR_BUFFER_BIT);
    }
}

pub fn string(value: u32) -> String {
    unsafe {
        let pointer = gl::GetString(value);
        if pointer.is_null() { return "unavailable".to_owned(); }
        CStr::from_ptr(pointer.cast()).to_string_lossy().into_owned()
    }
}
