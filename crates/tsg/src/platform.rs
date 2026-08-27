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

    /// タスクバーのボタンを点滅させて呼ぶ。
    ///
    /// **窓が前に居るときは何もしない。** 見ている人にちらつかせても
    /// 用が無いどころか邪魔になる。裏に回っているときだけ呼ぶ意味がある。
    pub fn attention<W: HasWindowHandle>(window: &W) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO, FlashWindowEx,
        };
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        let info = FLASHWINFO {
            cbSize: size_of::<FLASHWINFO>() as u32,
            hwnd,
            // TIMERNOFG: 前に出るまで点滅し続ける。見るまで消えないのが要点。
            dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        unsafe { FlashWindowEx(&info) };
    }

    /// 通知領域のアイコンの番号。**1 つしか出さない**ので固定。
    const TRAY_ID: u32 = 1;

    /// 画面の隅に知らせを出す（最小化していても届く）。
    ///
    /// **タスクバーの点滅では足りない。** 窓を最小化していると、点滅は
    /// 気づける人にしか気づけないし、何が起きたのかは分からない。
    /// エージェントが返事を待っているのは「見に行く理由」なので、
    /// どのタブが待っているかまで言う。
    ///
    /// WinRT のトーストではなく通知領域のバルーンを使う。Windows 10 以降は
    /// OS がこれをトーストとして出すので見た目は同じで、**AppUserModelID を
    /// 登録したショートカットが要らない**（入れ方が 1 行で済む）。
    /// アイコンは`hide_popup` で消すので、常駐しているように見えない。
    pub fn popup<W: HasWindowHandle>(window: &W, title: &str, body: &str) {
        use windows_sys::Win32::UI::Shell::{
            NIF_INFO, NIIF_INFO, NIM_ADD, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
        };
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        put(&mut data.szInfoTitle, title);
        put(&mut data.szInfo, body);
        // 既に出ていれば MODIFY、無ければ ADD。**どちらが先か分からない**
        // ので、両方試して通ったほうを使う（順番に依存させない）。
        let added = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        if added == 0 {
            unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
        }
    }

    /// 知らせのアイコンを片付ける。**窓が前に出たら消す。**
    ///
    /// 出しっぱなしにすると、用が済んだあとも通知領域に居座る。
    pub fn hide_popup<W: HasWindowHandle>(window: &W) {
        use windows_sys::Win32::UI::Shell::{NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }

    /// UTF-16 の固定長へ入れる。**末尾の 0 を必ず残す。**
    fn put(dst: &mut [u16], text: &str) {
        let room = dst.len().saturating_sub(1);
        let mut n = 0;
        for u in text.encode_utf16().take(room) {
            dst[n] = u;
            n += 1;
        }
        dst[n] = 0;
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

    /// 注意を引く。winit の `request_user_attention` が担うので、ここは空。
    pub fn attention<W: HasWindowHandle>(_window: &W) {}

    /// 隅に出す知らせ。**まだ書いていない。**
    ///
    /// macOS は `UNUserNotificationCenter`（署名済みバンドルが要る）、
    /// Linux は `org.freedesktop.Notifications`（D-Bus）で、Windows と
    /// 同じ 1 本の道にならない。書くまでは黙って何もしないほうが、
    /// 中途半端に片方だけ動くより読める。
    pub fn popup<W: HasWindowHandle>(_window: &W, _title: &str, _body: &str) {}

    pub fn hide_popup<W: HasWindowHandle>(_window: &W) {}
}

pub use imp::{attach_parent_console, attention, decorate, hide_popup, popup, ui_language};
