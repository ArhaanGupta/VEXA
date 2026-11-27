//! API key storage. On Windows the key lives in Windows Credential Manager (encrypted to the
//! user's login), never in a plain file. Elsewhere it is read from ANTHROPIC_API_KEY.

use anyhow::Result;

#[cfg(windows)]
const TARGET: &str = "Vexa/AnthropicApiKey";

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub fn load_api_key() -> Option<String> {
    use windows::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };
    use windows::core::PCWSTR;
    let target = wide(TARGET);
    unsafe {
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred).ok()?;
        let c = &*cred;
        let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        let key = String::from_utf8(bytes.to_vec()).ok();
        CredFree(cred as *const _);
        key.filter(|k| !k.trim().is_empty())
    }
}

#[cfg(windows)]
pub fn save_api_key(key: &str) -> Result<()> {
    use windows::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
    };
    use windows::core::PWSTR;
    let mut target = wide(TARGET);
    let mut user = wide("Vexa");
    let mut blob = key.trim().as_bytes().to_vec();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0)? };
    Ok(())
}

#[cfg(not(windows))]
pub fn load_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn save_api_key(_key: &str) -> Result<()> {
    anyhow::bail!("set the ANTHROPIC_API_KEY environment variable instead")
}
