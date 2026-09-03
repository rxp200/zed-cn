#![cfg(target_os = "windows")]

use std::{cell::RefCell, os::windows::ffi::OsStringExt, path::PathBuf};

use windows::{
    Win32::{
        Foundation::{
            CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_INVALIDARG, E_NOTIMPL, ERROR_INSUFFICIENT_BUFFER,
            GetLastError, HINSTANCE, MAX_PATH,
        },
        Globalization::u_strlen,
        System::{
            Com::{
                DVASPECT_CONTENT, FORMATETC, IBindCtx, IClassFactory, IClassFactory_Impl,
                IDataObject, TYMED_HGLOBAL,
            },
            LibraryLoader::GetModuleFileNameW,
            Ole::CF_HDROP,
            Registry::HKEY,
            SystemServices::DLL_PROCESS_ATTACH,
        },
        UI::{
            Shell::{
                CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, Common::ITEMIDLIST,
                DragFinish, DragQueryFileW, ECF_DEFAULT, ECS_ENABLED, GCS_HELPTEXTW, GCS_VERBW,
                HDROP, IContextMenu, IContextMenu_Impl, IEnumExplorerCommand, IExplorerCommand,
                IExplorerCommand_Impl, IShellExtInit, IShellExtInit_Impl, IShellItemArray,
                SHGetPathFromIDListW, SHStrDupW, SIGDN_FILESYSPATH,
            },
            WindowsAndMessaging::{AppendMenuW, HMENU, MF_BYPOSITION, MF_STRING},
        },
    },
    core::{BOOL, GUID, HRESULT, HSTRING, Interface, PCWSTR, PSTR, Ref, Result, implement},
};

// Command mask passed in `CMINVOKECOMMANDINFO::fMask` when the struct is actually
// a `CMINVOKECOMMANDINFOEX` carrying wide-string members.
const CMIC_MASK_UNICODE: u32 = 0x0004_0000;

static mut DLL_INSTANCE: HINSTANCE = HINSTANCE(std::ptr::null_mut());

#[unsafe(no_mangle)]
extern "system" fn DllMain(
    hinstdll: HINSTANCE,
    fdwreason: u32,
    _lpvreserved: *mut core::ffi::c_void,
) -> bool {
    if fdwreason == DLL_PROCESS_ATTACH {
        unsafe { DLL_INSTANCE = hinstdll };
    }

    true
}

#[implement(IExplorerCommand, IShellExtInit, IContextMenu)]
struct ExplorerCommandInjector {
    paths: RefCell<Vec<String>>,
}

impl ExplorerCommandInjector {
    fn new() -> Self {
        Self {
            paths: RefCell::new(Vec::new()),
        }
    }
}

#[allow(non_snake_case)]
impl IExplorerCommand_Impl for ExplorerCommandInjector_Impl {
    fn GetTitle(&self, _: Ref<IShellItemArray>) -> Result<windows_core::PWSTR> {
        let command_description =
            retrieve_command_description().unwrap_or(HSTRING::from("Open with Zed"));
        unsafe { SHStrDupW(&command_description) }
    }

    fn GetIcon(&self, _: Ref<IShellItemArray>) -> Result<windows_core::PWSTR> {
        let Some(zed_exe) = get_zed_exe_path() else {
            return Err(E_FAIL.into());
        };
        unsafe { SHStrDupW(&HSTRING::from(zed_exe)) }
    }

    fn GetToolTip(&self, _: Ref<IShellItemArray>) -> Result<windows_core::PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<windows_core::GUID> {
        Ok(GUID::zeroed())
    }

    fn GetState(&self, _: Ref<IShellItemArray>, _: BOOL) -> Result<u32> {
        Ok(ECS_ENABLED.0 as _)
    }

    fn Invoke(&self, psiitemarray: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> Result<()> {
        let items = psiitemarray.ok()?;
        let Some(zed_exe) = get_zed_exe_path() else {
            return Ok(());
        };

        let count = unsafe { items.GetCount()? };
        for idx in 0..count {
            let item = unsafe { items.GetItemAt(idx)? };
            let item_path = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)?.to_string()? };
            #[allow(clippy::disallowed_methods, reason = "no async context in sight..")]
            std::process::Command::new(&zed_exe)
                .arg(&item_path)
                .spawn()
                .map_err(|_| E_INVALIDARG)?;
        }

        Ok(())
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as _)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[allow(non_snake_case)]
impl IShellExtInit_Impl for ExplorerCommandInjector_Impl {
    fn Initialize(
        &self,
        pidlfolder: *const ITEMIDLIST,
        pdtobj: Ref<'_, IDataObject>,
        _hkeyprogid: HKEY,
    ) -> Result<()> {
        let mut paths: Vec<String> = Vec::new();

        // Prefer the selection from the data object (CF_HDROP).
        if let Ok(data_object) = pdtobj.ok() {
            let formatetc = FORMATETC {
                cfFormat: CF_HDROP.0,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            };
            if let Ok(medium) = unsafe { data_object.GetData(&formatetc) } {
                let hdrop = HDROP(unsafe { medium.u.hGlobal }.0);
                let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, None) };
                for index in 0..count {
                    let mut buffer = vec![0u16; 32768];
                    let length = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) };
                    buffer.truncate(length as usize);
                    if length > 0 {
                        paths.push(String::from_utf16_lossy(&buffer));
                    }
                }
                unsafe { DragFinish(hdrop) };
            }
        }

        // Background menus have no selection; fall back to the folder itself.
        if paths.is_empty() && !pidlfolder.is_null() {
            let mut buffer = [0u16; 260];
            if unsafe { SHGetPathFromIDListW(pidlfolder, &mut buffer) }.as_bool() {
                let length = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
                paths.push(String::from_utf16_lossy(&buffer[..length]));
            }
        }

        *self.paths.borrow_mut() = paths;
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IContextMenu_Impl for ExplorerCommandInjector_Impl {
    fn QueryContextMenu(
        &self,
        hmenu: HMENU,
        _indexmenu: u32,
        idcmdfirst: u32,
        idcmdlast: u32,
        uflags: u32,
    ) -> HRESULT {
        if uflags & CMF_DEFAULTONLY != 0 || idcmdfirst >= idcmdlast {
            return HRESULT(0);
        }

        let title = retrieve_command_description().unwrap_or(HSTRING::from("Open with Zed"));
        let title: Vec<u16> = title
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            AppendMenuW(
                hmenu,
                MF_STRING | MF_BYPOSITION,
                idcmdfirst as usize,
                PCWSTR(title.as_ptr()),
            )
        }
        .ok();
        HRESULT(1)
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> Result<()> {
        if pici.is_null() || !invoked_our_command(unsafe { &*pici }) {
            return Ok(());
        }

        let Some(zed_exe) = get_zed_exe_path() else {
            return Ok(());
        };
        let paths = self.paths.borrow();
        if paths.is_empty() {
            return Ok(());
        }

        #[allow(clippy::disallowed_methods, reason = "no async context in sight..")]
        std::process::Command::new(&zed_exe)
            .args(paths.iter())
            .spawn()
            .map_err(|_| E_INVALIDARG)?;
        Ok(())
    }

    fn GetCommandString(
        &self,
        idcmd: usize,
        utype: u32,
        _preserved: *const u32,
        pszname: PSTR,
        cchmax: u32,
    ) -> Result<()> {
        if idcmd != 0 {
            return Err(E_INVALIDARG.into());
        }
        match utype {
            GCS_VERBW => write_menu_string(pszname, cchmax, "OpenWithZed"),
            GCS_HELPTEXTW => write_menu_string(pszname, cchmax, "Open the selected items in Zed"),
            _ => Err(E_NOTIMPL.into()),
        }
    }
}

fn invoked_our_command(info: &CMINVOKECOMMANDINFO) -> bool {
    let verb = info.lpVerb.0 as usize;
    if verb >> 16 == 0 {
        // Invoked by the command id: the low word is the offset from `idCmdFirst`.
        return (verb & 0xFFFF) == 0;
    }
    if info.fMask & CMIC_MASK_UNICODE != 0 {
        let info =
            unsafe { &*(info as *const CMINVOKECOMMANDINFO as *const CMINVOKECOMMANDINFOEX) };
        if !info.lpVerbW.0.is_null() {
            return wide_str_eq(info.lpVerbW.0, "OpenWithZed");
        }
    }
    if !info.lpVerb.0.is_null() {
        let verb = unsafe { std::ffi::CStr::from_ptr(info.lpVerb.0 as *const i8) };
        return verb.to_bytes() == b"OpenWithZed";
    }
    false
}

fn wide_str_eq(ptr: *const u16, expected: &str) -> bool {
    let expected: Vec<u16> = expected.encode_utf16().collect();
    let mut index = 0;
    unsafe {
        while index < expected.len() {
            if *ptr.add(index) != expected[index] {
                return false;
            }
            index += 1;
        }
        *ptr.add(index) == 0
    }
}

fn write_menu_string(pszname: PSTR, cchmax: u32, text: &str) -> Result<()> {
    if cchmax == 0 || pszname.0.is_null() {
        return Ok(());
    }
    let text: Vec<u16> = text.encode_utf16().collect();
    let count = text.len().min(cchmax as usize - 1);
    unsafe {
        let dst = pszname.0 as *mut u16;
        for (index, c) in text[..count].iter().enumerate() {
            *dst.add(index) = *c;
        }
        *dst.add(count) = 0;
    }
    Ok(())
}

#[implement(IClassFactory)]
struct ExplorerCommandInjectorFactory;

impl IClassFactory_Impl for ExplorerCommandInjectorFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<windows_core::IUnknown>,
        riid: *const windows_core::GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if ppvobject.is_null() || riid.is_null() {
            return Err(windows::Win32::Foundation::E_POINTER.into());
        }

        unsafe {
            *ppvobject = std::ptr::null_mut();
        }

        if punkouter.is_none() {
            let factory: IExplorerCommand = ExplorerCommandInjector::new().into();
            unsafe { factory.query(riid, ppvobject).ok() }
        } else {
            Err(E_INVALIDARG.into())
        }
    }

    fn LockServer(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
}

const MODULE_ID: GUID = cfg_select! {
    feature = "stable" => { GUID::from_u128(0x6a1f6b13_3b82_48a1_9e06_7bb0a6d0bffd) },
    feature = "preview" => { GUID::from_u128(0xaf8e85ea_fb20_4db2_93cf_56513c1ec697) },
    feature = "nightly" => { GUID::from_u128(0x266f2cfe_1653_42af_b55c_fe3590c83871) },
    _ => { GUID::from_u128(0x685f4d49_6718_4c55_b271_ebb5c6a48d6f) },
};

// CLSIDs used when the DLL is registered directly via `shellex\ContextMenuHandlers`
// (the fallback when the appx package cannot be installed, e.g. on unsigned builds).
// They are distinct from the appx `MODULE_ID`s so both registrations can coexist.
#[cfg(all(feature = "stable", not(feature = "preview"), not(feature = "nightly")))]
const CONTEXT_MENU_CLSID: GUID = GUID::from_u128(0xef6eda23_89b3_435f_816e_af20ff984938);
#[cfg(all(feature = "preview", not(feature = "stable"), not(feature = "nightly")))]
const CONTEXT_MENU_CLSID: GUID = GUID::from_u128(0x755ad97e_00bf_4696_962f_6c62113db47a);
#[cfg(all(feature = "nightly", not(feature = "stable"), not(feature = "preview")))]
const CONTEXT_MENU_CLSID: GUID = GUID::from_u128(0xe15a7999_ced2_428f_bb19_09567a90d65b);

// Make cargo clippy happy
#[cfg(all(feature = "nightly", feature = "stable", feature = "preview"))]
const CONTEXT_MENU_CLSID: GUID = GUID::from_u128(0x8f23789a_9c6f_4e6c_a03f_ec8aa631da95);

#[unsafe(no_mangle)]
extern "system" fn DllGetClassObject(
    class_id: *const GUID,
    iid: *const GUID,
    out: *mut *mut std::ffi::c_void,
) -> HRESULT {
    if out.is_null() || class_id.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }

    unsafe {
        *out = std::ptr::null_mut();
    }
    let class_id = unsafe { *class_id };
    if class_id == MODULE_ID || class_id == CONTEXT_MENU_CLSID {
        let instance: IClassFactory = ExplorerCommandInjectorFactory {}.into();
        unsafe { instance.query(iid, out) }
    } else {
        CLASS_E_CLASSNOTAVAILABLE
    }
}

fn get_zed_install_folder() -> Option<PathBuf> {
    let mut buf = vec![0u16; MAX_PATH as usize];
    unsafe { GetModuleFileNameW(Some(DLL_INSTANCE.into()), &mut buf) };

    while unsafe { GetLastError() } == ERROR_INSUFFICIENT_BUFFER {
        buf = vec![0u16; buf.len() * 2];
        unsafe { GetModuleFileNameW(Some(DLL_INSTANCE.into()), &mut buf) };
    }
    let len = unsafe { u_strlen(buf.as_ptr()) };
    let path: PathBuf = std::ffi::OsString::from_wide(&buf[..len as usize])
        .into_string()
        .ok()?
        .into();
    Some(path.parent()?.parent()?.to_path_buf())
}

#[inline]
fn get_zed_exe_path() -> Option<String> {
    get_zed_install_folder().map(|path| path.join("Zed.exe").to_string_lossy().into_owned())
}

#[inline]
fn retrieve_command_description() -> Result<HSTRING> {
    // These keys are written by the installer (see zed.iss) as
    // `Software\Classes\{#RegValueName}ContextMenu\Title`.
    const REG_PATH: &str = cfg_select! {
        feature = "stable" => { r#"Software\Classes\ZedContextMenu"# },
        feature = "preview" => { r#"Software\Classes\ZedPreviewContextMenu"# },
        feature = "nightly" => { r#"Software\Classes\ZedNightlyContextMenu"# },
        _ => { r#"Software\Classes\ZedDevContextMenu"# },
    };

    let key = windows_registry::CURRENT_USER.open(REG_PATH)?;
    key.get_hstring("Title")
}
