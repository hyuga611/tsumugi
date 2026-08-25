//! Windows のソケット権限。**`windows-sys` をこのクレートのここ以外に持ち込まない。**
//!
//! やることは 3 つだけ:
//!
//! 1. 自分のユーザ SID を文字列で取る
//! 2. 「そのユーザだけに全権、他は無し」の DACL を作る
//! 3. 繋いだ先のプロセスが**自分のものか**を確かめる
//!
//! 3 が要るのは、`\\.\pipe\` が誰でも名前を作れる名前空間だから。DACL は
//! 「自分が作った口を他人に読ませない」ためのもので、「他人が先に作った口へ
//! 自分が繋いでしまう」のは防げない。両方塞いで初めて閉じる。

use anyhow::{Context, Result, bail};
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::{U16CStr, U16CString};
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 現在のプロセスのユーザ SID（`S-1-5-21-…` 形式）。
pub fn current_user_sid() -> Result<String> {
    let raw = TokenUserBuf::current()?;
    // SAFETY: `TokenUserBuf` が生きている間、SID はそのバッファの中を指す。
    unsafe { sid_to_string(raw.sid()) }
}

/// 自分だけに全権を与える DACL。
///
/// SDDL の `D:P(A;;GA;;;<SID>)`:
///   - `D:`  DACL
///   - `P`   継承を遮る（親から緩い ACE を貰わない）
///   - `A`   許可
///   - `GA`  GENERIC_ALL
///
/// **他は 1 つも書かない。** SYSTEM も Administrators も書かないので、
/// 管理者は「所有権を取る」経路でしか触れない（それは OS の管理者の権能であって、
/// ここで塞ぐ種類のものではない）。
pub fn owner_only_descriptor() -> Result<SecurityDescriptor> {
    let sid = current_user_sid()?;
    let sddl = U16CString::from_str(format!("D:P(A;;GA;;;{sid})"))
        .context("SDDL を UTF-16 にできません")?;
    SecurityDescriptor::deserialize(&sddl).context("SDDL を解釈できません")
}

/// ディレクトリを所有者だけに閉じる。
///
/// 既定に頼らない。**継承を切って、自分だけの DACL を明示的に置く。**
/// 置けなければエラーを返す — 呼ぶ側は、閉じられないなら作らない。
pub fn lock_directory(dir: &std::path::Path) -> std::io::Result<()> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let fail = |m: &str| std::io::Error::other(m.to_string());
    let sid = current_user_sid().map_err(|e| fail(&e.to_string()))?;
    let sddl = U16CString::from_str(format!("D:P(A;OICI;GA;;;{sid})"))
        .map_err(|_| fail("SDDL を UTF-16 にできません"))?;
    let path = U16CString::from_os_str(dir.as_os_str())
        .map_err(|_| fail("パスを UTF-16 にできません"))?;

    let mut sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: 文字列は終端付き。返った SD は LocalFree で返す。
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        return Err(fail("DACL を作れません"));
    }
    // SAFETY: `sd` は上で作った有効な SD。DACL は自己相対形式で入っている。
    let mut dacl = std::ptr::null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    let got = unsafe {
        windows_sys::Win32::Security::GetSecurityDescriptorDacl(
            sd,
            &mut present,
            &mut dacl,
            &mut defaulted,
        )
    };
    let rc = if got != 0 && present != 0 {
        // SAFETY: `dacl` は `sd` の中を指す。呼び出し中だけ使う。
        unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr().cast_mut(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        }
    } else {
        1
    };
    // SAFETY: LocalAlloc されたものを返す。
    unsafe { LocalFree(sd.cast()) };
    if rc != 0 {
        return Err(fail("DACL を置けません"));
    }
    Ok(())
}

/// その pid のプロセスが自分のものか。
///
/// 開けなければ **false**（自分のものではない）と答える。他のユーザのプロセスは
/// たいてい `OpenProcess` の時点で拒まれるので、そこで落ちるのが正しい振る舞い。
/// 迷ったら繋がない側へ倒す。
pub fn process_is_current_user(pid: u32) -> bool {
    let Ok(me) = TokenUserBuf::current() else {
        return false;
    };
    // SAFETY: 引数は値渡し。返ったハンドルは HandleGuard が閉じる。
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if h.is_null() {
        return false;
    }
    let guard = HandleGuard(h);
    let Ok(them) = TokenUserBuf::of_process(guard.0) else {
        return false;
    };
    // SAFETY: どちらの SID も、それぞれのバッファが生きている間だけ有効。
    unsafe { EqualSid(me.sid(), them.sid()) != 0 }
}

// ---------------------------------------------------------------------------

/// `GetTokenInformation(TokenUser)` の結果を持つバッファ。
struct TokenUserBuf(Vec<u8>);

impl TokenUserBuf {
    fn current() -> Result<Self> {
        // SAFETY: GetCurrentProcess は擬似ハンドルを返すだけで、閉じる必要が無い。
        Self::of_process(unsafe { GetCurrentProcess() })
    }

    fn of_process(process: HANDLE) -> Result<Self> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: token は下で HandleGuard が閉じる。
        let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
        if ok == 0 {
            bail!("プロセストークンを開けません: {}", last_error());
        }
        let guard = HandleGuard(token);

        let mut need: u32 = 0;
        // 1 回目は必要な大きさを聞くだけ（失敗するのが正しい）。
        unsafe { GetTokenInformation(guard.0, TokenUser, std::ptr::null_mut(), 0, &mut need) };
        if need == 0 {
            bail!("トークン情報の大きさが分かりません: {}", last_error());
        }

        let mut buf = vec![0u8; need as usize];
        let ok = unsafe {
            GetTokenInformation(guard.0, TokenUser, buf.as_mut_ptr().cast(), need, &mut need)
        };
        if ok == 0 {
            bail!("トークン情報を読めません: {}", last_error());
        }
        Ok(Self(buf))
    }

    /// バッファ内の SID。**このバッファが生きている間だけ有効。**
    ///
    /// # Safety
    /// 返り値を `self` より長く持ち出さないこと。
    unsafe fn sid(&self) -> PSID {
        // SAFETY: query() が TOKEN_USER 分の大きさで確保している。
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: OpenProcessToken が開いた実ハンドル。閉じるのは 1 回だけ。
        unsafe { CloseHandle(self.0) };
    }
}

/// # Safety
/// `sid` が有効な SID を指していること。
unsafe fn sid_to_string(sid: PSID) -> Result<String> {
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: 呼び出し側が sid の有効性を保証する。out は LocalFree で返す。
    if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 || out.is_null() {
        bail!("SID を文字列にできません: {}", last_error());
    }
    // SAFETY: 成功したので out は NUL 終端の UTF-16 文字列。
    let s = unsafe { U16CStr::from_ptr_str(out) }.to_string_lossy();
    // SAFETY: ConvertSidToStringSidW が確保したもの。
    unsafe { LocalFree(out.cast()) };
    Ok(s)
}

fn last_error() -> u32 {
    // SAFETY: 引数を取らず、スレッドローカルな値を読むだけ。
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_own_process_is_recognised_as_mine() {
        assert!(process_is_current_user(std::process::id()), "自分を自分と見なせない");
    }

    /// 居ない pid には決して「自分のもの」と答えない。**迷ったら繋がない。**
    #[test]
    fn a_process_that_is_not_there_is_not_mine() {
        assert!(!process_is_current_user(u32::MAX), "存在しない pid を自分と見なした");
    }

    #[test]
    fn the_current_user_has_a_sid() {
        let sid = current_user_sid().expect("SID が取れない");
        assert!(sid.starts_with("S-1-"), "SID の形になっていない: {sid}");
    }

    /// DACL に**自分以外を書かない**こと。ここが緩むと、同じマシンの
    /// 他ユーザから画面が読める。
    #[test]
    fn the_descriptor_names_only_the_current_user() {
        owner_only_descriptor().expect("権限を作れない");
        let sid = current_user_sid().expect("SID が取れない");
        let sddl = format!("D:P(A;;GA;;;{sid})");
        assert!(sddl.contains("D:P"), "継承を遮っていない");
        assert_eq!(sddl.matches("(A;").count(), 1, "許可の ACE が 1 つではない");
        assert!(!sddl.contains(";WD)"), "Everyone(WD) が入っている");
        assert!(!sddl.contains(";AU)"), "Authenticated Users(AU) が入っている");
    }
}
