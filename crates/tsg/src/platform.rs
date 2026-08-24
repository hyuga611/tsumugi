//! OS 固有の見た目と起動まわり。**`windows-sys` をここ以外に持ち込まない。**
//!
//! 端末エミュレータは「他のプログラムから起動される」ものなので、
//! 起動したときにコンソールの黒窓が付いてくるかどうかは機能の一部として扱う。

#[cfg(windows)]
mod imp {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

    /// タイトルバーを暗くする。
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    /// 背景の合成方法（Windows 11 22H2 以降）。
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    /// 一時ウィンドウ向けのアクリル（＝背景をぼかす）。
    const DWMSBT_TRANSIENTWINDOW: i32 = 3;
    /// 合成しない。
    const DWMSBT_NONE: i32 = 1;

    /// OS の表示言語。`ja-JP` のような文字列。
    pub fn ui_language() -> Option<String> {
        use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
        let mut buf = [0u16; 85];
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 1 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..(n as usize - 1)]))
    }

    /// 親のコンソールへ出力を繋ぐ。
    ///
    /// GUI サブシステムにしたので、Explorer やショートカットから起動しても
    /// 黒い窓が付いてこない。代わりにターミナルから `--help` を叩いたときに
    /// 何も出なくなるので、親のコンソールがあれば拾う。
    pub fn attach_parent_console() {
        // main の先頭で呼ぶこと。std が無効なハンドルを掴んだ後では遅い。
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }

    fn hwnd_of<W: HasWindowHandle>(w: &W) -> Option<HWND> {
        match w.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
            _ => None,
        }
    }

    /// ウィンドウの見た目を OS 側で整える。効かない環境では黙って何も起きない。
    pub fn decorate<W: HasWindowHandle>(window: &W, blur: bool) {
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        unsafe {
            let dark: i32 = 1;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                std::ptr::from_ref(&dark).cast(),
                size_of::<i32>() as u32,
            );

            // 22H2 未満では未知の属性として弾かれるだけ。失敗しても続行する。
            let backdrop: i32 = if blur {
                DWMSBT_TRANSIENTWINDOW
            } else {
                DWMSBT_NONE
            };
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                std::ptr::from_ref(&backdrop).cast(),
                size_of::<i32>() as u32,
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use raw_window_handle::HasWindowHandle;

    pub fn attach_parent_console() {}

    /// OS の表示言語。Unix は環境変数で決まる。
    pub fn ui_language() -> Option<String> {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
            if let Some(v) = std::env::var_os(key).and_then(|v| v.into_string().ok())
                && !v.is_empty()
                && v != "C"
                && v != "POSIX"
            {
                return Some(v);
            }
        }
        None
    }

    /// macOS / Linux のぼかしはコンポジタ側の仕事。
    /// ウィンドウを透過にするところまでは winit が担う。
    pub fn decorate<W: HasWindowHandle>(_window: &W, _blur: bool) {}
}

pub use imp::{attach_parent_console, decorate, ui_language};
