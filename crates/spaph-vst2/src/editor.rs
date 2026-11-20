//! Native window hosting for VST2 plugin editors (X11 on Linux, Win32 on Windows).
//!
//! The editor loop is platform-independent and tested with fakes; the window types
//! only create and pump a native window.

use spaph_types::{Result, SpaphError};
use std::ffi::c_void;
use vst::editor::Editor;

/// A native window that hosts a plugin editor.
pub trait HostWindow {
    /// Native parent handle passed to the plugin's `open` call.
    fn parent(&self) -> *mut c_void;
    /// Process pending window events. Returns `false` once the user has closed the window.
    fn pump(&mut self) -> bool;
}

/// Open the plugin editor inside `window`. Must run on the thread that owns the window.
pub fn attach(editor: &mut dyn Editor, window: &mut dyn HostWindow) -> Result<()> {
    if !editor.open(window.parent()) {
        return Err(SpaphError::Plugin("plugin editor failed to open".into()));
    }
    Ok(())
}

/// Service the window once: process its events, then give the plugin an idle call.
/// Returns `false` once the user has closed the window.
pub fn tick(editor: &mut dyn Editor, window: &mut dyn HostWindow) -> bool {
    if !window.pump() {
        return false;
    }
    editor.idle();
    true
}

/// Close the plugin editor. The window itself is destroyed when its owner drops it.
pub fn detach(editor: &mut dyn Editor) {
    editor.close();
}

/// Create a native window of the given client size for hosting an editor.
pub fn open_window(title: &str, width: i32, height: i32) -> Result<Box<dyn HostWindow>> {
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(x11::X11Window::create(title, width, height)?));
    }
    #[cfg(target_os = "windows")]
    {
        return Ok(Box::new(win32::Win32Window::create(title, width, height)?));
    }
    #[allow(unreachable_code)]
    {
        let _ = (title, width, height);
        Err(SpaphError::Plugin("VST2 editors are not supported on this platform".into()))
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use super::HostWindow;
    use spaph_types::{Result, SpaphError};
    use std::ffi::c_void;
    use xcb::Xid;

    pub struct X11Window {
        connection: xcb::Connection,
        window: xcb::x::Window,
        wm_protocols: xcb::x::Atom,
        wm_delete_window: xcb::x::Atom,
        closed: bool,
    }

    fn fail<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> SpaphError {
        move |e| SpaphError::Plugin(format!("{}: {}", what, e))
    }

    fn intern(connection: &xcb::Connection, name: &[u8]) -> Result<xcb::x::Atom> {
        let cookie = connection.send_request(&xcb::x::InternAtom { only_if_exists: false, name });
        connection
            .wait_for_reply(cookie)
            .map(|reply| reply.atom())
            .map_err(fail("cannot intern X11 atom"))
    }

    impl X11Window {
        pub fn create(title: &str, width: i32, height: i32) -> Result<Self> {
            let (connection, screen_number) =
                xcb::Connection::connect(None).map_err(fail("cannot connect to X server"))?;
            let wm_protocols = intern(&connection, b"WM_PROTOCOLS")?;
            let wm_delete_window = intern(&connection, b"WM_DELETE_WINDOW")?;

            let setup = connection.get_setup();
            let screen = setup
                .roots()
                .nth(screen_number as usize)
                .ok_or_else(|| SpaphError::Plugin("no X11 screen found".into()))?;
            let window: xcb::x::Window = connection.generate_id();

            connection
                .send_and_check_request(&xcb::x::CreateWindow {
                    depth: xcb::x::COPY_FROM_PARENT as u8,
                    wid: window,
                    parent: screen.root(),
                    x: 0,
                    y: 0,
                    width: width as u16,
                    height: height as u16,
                    border_width: 0,
                    class: xcb::x::WindowClass::InputOutput,
                    visual: screen.root_visual(),
                    value_list: &[
                        xcb::x::Cw::BackPixel(screen.white_pixel()),
                        xcb::x::Cw::EventMask(xcb::x::EventMask::EXPOSURE | xcb::x::EventMask::KEY_PRESS),
                    ],
                })
                .map_err(fail("cannot create X11 window"))?;

            connection.send_request(&xcb::x::ChangeProperty {
                mode: xcb::x::PropMode::Replace,
                window,
                property: xcb::x::ATOM_WM_NAME,
                r#type: xcb::x::ATOM_STRING,
                data: title.as_bytes(),
            });
            // Ask for a ClientMessage on close instead of letting the server kill the client.
            connection
                .send_and_check_request(&xcb::x::ChangeProperty {
                    mode: xcb::x::PropMode::Replace,
                    window,
                    property: wm_protocols,
                    r#type: xcb::x::ATOM_ATOM,
                    data: &[wm_delete_window],
                })
                .map_err(fail("cannot enable X11 close handling"))?;

            connection.send_request(&xcb::x::MapWindow { window });
            connection.flush().map_err(fail("X11 flush failed"))?;

            Ok(Self { connection, window, wm_protocols, wm_delete_window, closed: false })
        }

        fn is_delete_request(&self, event: &xcb::Event) -> bool {
            let xcb::Event::X(xcb::x::Event::ClientMessage(event)) = event else {
                return false;
            };
            let xcb::x::ClientMessageData::Data32([protocol, ..]) = event.data() else {
                return false;
            };
            event.window() == self.window
                && event.r#type() == self.wm_protocols
                && protocol == self.wm_delete_window.resource_id()
        }
    }

    impl HostWindow for X11Window {
        fn parent(&self) -> *mut c_void {
            // VST2 on X11 takes the window id as the parent handle.
            self.window.resource_id() as usize as *mut c_void
        }

        fn pump(&mut self) -> bool {
            if self.closed {
                return false;
            }
            loop {
                match self.connection.poll_for_event() {
                    Ok(Some(event)) => {
                        if self.is_delete_request(&event) {
                            self.closed = true;
                            return false;
                        }
                    }
                    Ok(None) => return true,
                    Err(e) => {
                        tracing::warn!("X11 connection error, closing editor: {}", e);
                        self.closed = true;
                        return false;
                    }
                }
            }
        }
    }

    impl Drop for X11Window {
        fn drop(&mut self) {
            self.connection.send_request(&xcb::x::DestroyWindow { window: self.window });
            let _ = self.connection.flush();
        }
    }
}

#[cfg(target_os = "windows")]
mod win32 {
    use super::HostWindow;
    use spaph_types::{Result, SpaphError};
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use winapi::shared::windef::HWND;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::winuser::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, IsWindow, LoadCursorW,
        PeekMessageW, RegisterClassExW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
        CW_USEDEFAULT, IDC_ARROW, MSG, PM_REMOVE, SW_SHOW, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct Win32Window {
        hwnd: HWND,
    }

    impl Win32Window {
        pub fn create(title: &str, width: i32, height: i32) -> Result<Self> {
            let class_name = wide("spaph_vst2_editor");
            let title = wide(title);
            unsafe {
                let instance = GetModuleHandleW(null_mut());
                let class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(DefWindowProcW),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: instance,
                    hIcon: null_mut(),
                    hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                    hbrBackground: null_mut(),
                    lpszMenuName: null_mut(),
                    lpszClassName: class_name.as_ptr(),
                    hIconSm: null_mut(),
                };
                // Registering an existing class fails harmlessly; CreateWindowExW reports real errors.
                RegisterClassExW(&class);
                let hwnd = CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    width,
                    height,
                    null_mut(),
                    null_mut(),
                    instance,
                    null_mut(),
                );
                if hwnd.is_null() {
                    return Err(SpaphError::Plugin("cannot create Win32 editor window".into()));
                }
                ShowWindow(hwnd, SW_SHOW);
                Ok(Self { hwnd })
            }
        }
    }

    impl HostWindow for Win32Window {
        fn parent(&self) -> *mut c_void {
            self.hwnd as *mut c_void
        }

        fn pump(&mut self) -> bool {
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                // The default window procedure destroys the window on WM_CLOSE.
                IsWindow(self.hwnd) != 0
            }
        }
    }

    impl Drop for Win32Window {
        fn drop(&mut self) {
            unsafe {
                if IsWindow(self.hwnd) != 0 {
                    DestroyWindow(self.hwnd);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeEditor {
        accept: bool,
        opens: usize,
        idles: usize,
        closes: usize,
        parent: usize,
    }

    impl FakeEditor {
        fn new(accept: bool) -> Self {
            Self { accept, opens: 0, idles: 0, closes: 0, parent: 0 }
        }
    }

    impl Editor for FakeEditor {
        fn size(&self) -> (i32, i32) {
            (640, 480)
        }
        fn position(&self) -> (i32, i32) {
            (0, 0)
        }
        fn open(&mut self, parent: *mut c_void) -> bool {
            self.opens += 1;
            self.parent = parent as usize;
            self.accept
        }
        fn is_open(&mut self) -> bool {
            false
        }
        fn idle(&mut self) {
            self.idles += 1;
        }
        fn close(&mut self) {
            self.closes += 1;
        }
    }

    /// Reports `true` from `pump` `alive` times, then `false` (window closed).
    struct FakeWindow {
        alive: usize,
    }

    impl HostWindow for FakeWindow {
        fn parent(&self) -> *mut c_void {
            0x1234 as *mut c_void
        }
        fn pump(&mut self) -> bool {
            if self.alive == 0 {
                false
            } else {
                self.alive -= 1;
                true
            }
        }
    }

    #[test]
    fn tick_idles_while_window_open_and_reports_close() {
        let mut editor = FakeEditor::new(true);
        let mut window = FakeWindow { alive: 2 };
        attach(&mut editor, &mut window).unwrap();
        assert!(tick(&mut editor, &mut window));
        assert!(tick(&mut editor, &mut window));
        assert!(!tick(&mut editor, &mut window), "window closed by user");
        assert_eq!(editor.opens, 1);
        assert_eq!(editor.parent, 0x1234);
        assert_eq!(editor.idles, 2, "no idle call once the window is closed");
    }

    #[test]
    fn detach_closes_the_editor() {
        let mut editor = FakeEditor::new(true);
        let mut window = FakeWindow { alive: 1 };
        attach(&mut editor, &mut window).unwrap();
        detach(&mut editor);
        assert_eq!(editor.closes, 1);
    }

    #[test]
    fn failed_attach_is_an_error_and_does_not_idle() {
        let mut editor = FakeEditor::new(false);
        let mut window = FakeWindow { alive: 5 };
        assert!(attach(&mut editor, &mut window).is_err());
        assert_eq!(editor.opens, 1);
        assert_eq!(editor.idles, 0);
        assert_eq!(editor.closes, 0);
    }

    #[test]
    fn editor_can_be_attached_again_after_detach() {
        let mut editor = FakeEditor::new(true);
        attach(&mut editor, &mut FakeWindow { alive: 1 }).unwrap();
        detach(&mut editor);
        attach(&mut editor, &mut FakeWindow { alive: 1 }).unwrap();
        detach(&mut editor);
        assert_eq!(editor.opens, 2);
        assert_eq!(editor.closes, 2);
    }
}
