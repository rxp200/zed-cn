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
                IDataObject, STGMEDIUM, TYMED_HGLOBAL,
            },
            LibraryLoader::GetModuleFileNameW,
            Ole::{CF_HDROP, ReleaseStgMedium},
            Registry::HKEY,
            SystemServices::DLL_PROCESS_ATTACH,
        },
        UI::{
            Shell::{
                CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, Common::ITEMIDLIST,
                DragQueryFileW, ECF_DEFAULT, ECS_ENABLED, GCS_HELPTEXTW, GCS_VERBW, HDROP,
                IContextMenu, IContextMenu_Impl, IEnumExplorerCommand, IExplorerCommand,
                IExplorerCommand_Impl, IShellExtInit, IShellExtInit_Impl, IShellItemArray,
                SHGetPathFromIDListW, SHStrDupW, SIGDN_FILESYSPATH,
            },
            WindowsAndMessaging::{HMENU, InsertMenuW, MF_BYPOSITION, MF_STRING},
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

struct OwnedStorageMedium(STGMEDIUM);

impl Drop for OwnedStorageMedium {
    fn drop(&mut self) {
        // GetData may retain ownership through pUnkForRelease; freeing hGlobal
        // directly would bypass that provider's release contract.
        unsafe { ReleaseStgMedium(&mut self.0) };
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
                let medium = OwnedStorageMedium(medium);
                if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
                    return Err(E_INVALIDARG.into());
                }
                let hdrop = HDROP(unsafe { medium.0.u.hGlobal }.0);
                let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, None) };
                for index in 0..count {
                    let mut buffer = vec![0u16; 32768];
                    let length = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) };
                    buffer.truncate(length as usize);
                    if length > 0 {
                        paths.push(String::from_utf16_lossy(&buffer));
                    }
                }
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
        indexmenu: u32,
        idcmdfirst: u32,
        idcmdlast: u32,
        uflags: u32,
    ) -> HRESULT {
        query_context_menu(
            indexmenu,
            idcmdfirst,
            idcmdlast,
            uflags,
            |position, command_id| {
                let title =
                    retrieve_command_description().unwrap_or(HSTRING::from("Open with Zed"));
                let title: Vec<u16> = title
                    .to_string_lossy()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                unsafe {
                    InsertMenuW(
                        hmenu,
                        position,
                        MF_STRING | MF_BYPOSITION,
                        command_id as usize,
                        PCWSTR(title.as_ptr()),
                    )
                }
            },
        )
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> Result<()> {
        if !unsafe { invoked_our_command(pici) } {
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

fn query_context_menu(
    position: u32,
    first_command_id: u32,
    last_command_id: u32,
    flags: u32,
    insert: impl FnOnce(u32, u32) -> Result<()>,
) -> HRESULT {
    // Shell command-ID bounds are inclusive; this extension consumes exactly one ID.
    if flags & CMF_DEFAULTONLY != 0 || first_command_id > last_command_id {
        return HRESULT(0);
    }
    match insert(position, first_command_id) {
        Ok(()) => HRESULT(1),
        Err(error) => error.code(),
    }
}

// The COM caller must supply readable storage of cbSize bytes and valid,
// terminated strings for non-integer verbs. Sizes cannot validate arbitrary pointers.
unsafe fn invoked_our_command(pointer: *const CMINVOKECOMMANDINFO) -> bool {
    if pointer.is_null() {
        return false;
    }
    let size = unsafe { std::ptr::read_unaligned(pointer.cast::<u32>()) } as usize;
    if size != std::mem::size_of::<CMINVOKECOMMANDINFO>()
        && size != std::mem::size_of::<CMINVOKECOMMANDINFOEX>()
    {
        return false;
    }
    let info = unsafe { std::ptr::read_unaligned(pointer) };
    if info.fMask & CMIC_MASK_UNICODE != 0 {
        if size != std::mem::size_of::<CMINVOKECOMMANDINFOEX>() {
            return false;
        }
        let extended = unsafe { std::ptr::read_unaligned(pointer.cast::<CMINVOKECOMMANDINFOEX>()) };
        let verb = extended.lpVerbW.0 as usize;
        if verb >> 16 == 0 {
            return verb == 0;
        }
        return wide_str_eq(extended.lpVerbW.0, "OpenWithZed");
    }
    let verb = info.lpVerb.0 as usize;
    if verb >> 16 == 0 {
        return verb == 0;
    }
    let verb = unsafe { std::ffi::CStr::from_ptr(info.lpVerb.0.cast()) };
    verb.to_bytes() == b"OpenWithZed"
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        mem::ManuallyDrop,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use windows::Win32::{
        Foundation::{GlobalFree, HGLOBAL},
        System::{
            Com::STGMEDIUM_0,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalFlags},
        },
    };

    #[test]
    fn query_context_menu_accepts_inclusive_ranges_and_preserves_position() {
        for (first, last) in [(0, 0), (42, 42), (42, 43), (u32::MAX, u32::MAX)] {
            for position in [0, 3, u32::MAX] {
                let mut calls = 0;
                let result = query_context_menu(position, first, last, 0, |index, command_id| {
                    calls += 1;
                    assert_eq!(index, position);
                    assert_eq!(command_id, first);
                    Ok(())
                });
                assert_eq!(calls, 1);
                assert_eq!(result, HRESULT(1));
            }
        }
    }

    #[test]
    fn query_context_menu_skips_reversed_ranges_and_default_only() {
        for (first, last, flags) in [
            (1, 0, 0),
            (u32::MAX, 0, 0),
            (42, 42, CMF_DEFAULTONLY),
            (0, u32::MAX, CMF_DEFAULTONLY | 0x10),
        ] {
            let mut called = false;
            let result = query_context_menu(3, first, last, flags, |_, _| {
                called = true;
                Ok(())
            });
            assert!(!called);
            assert_eq!(result, HRESULT(0));
        }
    }

    #[test]
    fn query_context_menu_propagates_insertion_failure() {
        for failure in [E_FAIL, HRESULT::from_win32(1401)] {
            let mut calls = 0;
            let result = query_context_menu(3, 42, 42, 0, |_, _| {
                calls += 1;
                Err(failure.into())
            });
            assert_eq!(calls, 1);
            assert_eq!(result, failure);
            assert!(result.is_err());
        }
    }

    #[test]
    fn command_rejects_null_short_and_unicode_base_structures() {
        assert!(!unsafe { invoked_our_command(std::ptr::null()) });
        let size = 4u32;
        assert!(!unsafe { invoked_our_command((&size as *const u32).cast()) });
        let info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            fMask: CMIC_MASK_UNICODE,
            ..Default::default()
        };
        assert!(!unsafe { invoked_our_command(&info) });
    }

    #[test]
    fn command_matches_ansi_strings_and_integer_offsets() {
        let mut info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            lpVerb: windows::core::PCSTR(c"OpenWithZed".as_ptr().cast()),
            ..Default::default()
        };
        assert!(unsafe { invoked_our_command(&info) });
        info.lpVerb = windows::core::PCSTR(c"Other".as_ptr().cast());
        assert!(!unsafe { invoked_our_command(&info) });
        for offset in [0usize, 1, 65535] {
            info.lpVerb = windows::core::PCSTR(offset as *const u8);
            assert_eq!(unsafe { invoked_our_command(&info) }, offset == 0);
        }
    }

    #[test]
    fn unicode_verb_takes_precedence_and_integer_offsets_are_not_dereferenced() {
        let verb: Vec<u16> = "OpenWithZed\0".encode_utf16().collect();
        let mut info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE,
            lpVerb: windows::core::PCSTR(c"Other".as_ptr().cast()),
            lpVerbW: PCWSTR(verb.as_ptr()),
            ..Default::default()
        };
        assert!(unsafe { invoked_our_command((&info as *const CMINVOKECOMMANDINFOEX).cast()) });
        for offset in [0usize, 1, 65535] {
            info.lpVerbW = PCWSTR(offset as *const u16);
            assert_eq!(
                unsafe { invoked_our_command((&info as *const CMINVOKECOMMANDINFOEX).cast()) },
                offset == 0
            );
        }
        let other: Vec<u16> = "Other\0".encode_utf16().collect();
        info.lpVerb = windows::core::PCSTR::null();
        info.lpVerbW = PCWSTR(other.as_ptr());
        assert!(!unsafe { invoked_our_command((&info as *const CMINVOKECOMMANDINFOEX).cast()) });
    }

    const GMEM_INVALID_HANDLE: u32 = 0x8000;

    #[implement(IClassFactory)]
    struct MediumOwner {
        drops: Arc<AtomicUsize>,
    }

    impl Drop for MediumOwner {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl IClassFactory_Impl for MediumOwner_Impl {
        fn CreateInstance(
            &self,
            _: Ref<windows_core::IUnknown>,
            _: *const GUID,
            _: *mut *mut core::ffi::c_void,
        ) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn LockServer(&self, _: BOOL) -> Result<()> {
            Ok(())
        }
    }

    fn storage(handle: HGLOBAL, owner: Option<windows_core::IUnknown>) -> OwnedStorageMedium {
        OwnedStorageMedium(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: handle },
            pUnkForRelease: ManuallyDrop::new(owner),
        })
    }

    #[test]
    fn releases_caller_owned_global_memory() -> Result<()> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, 64) }?;
        drop(storage(handle, None));
        assert_eq!(unsafe { GlobalFlags(handle) }, GMEM_INVALID_HANDLE);
        Ok(())
    }

    #[test]
    fn releases_provider_reference_without_freeing_provider_storage() -> Result<()> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, 64) }?;
        let drops = Arc::new(AtomicUsize::new(0));
        let owner: IClassFactory = MediumOwner {
            drops: drops.clone(),
        }
        .into();
        let medium = storage(handle, Some(owner.cast()?));
        drop(owner);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(medium);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_ne!(unsafe { GlobalFlags(handle) }, GMEM_INVALID_HANDLE);
        unsafe { GlobalFree(Some(handle)) }?;
        Ok(())
    }

    #[test]
    fn releases_provider_reference_on_early_error() -> Result<()> {
        fn fail(medium: OwnedStorageMedium) -> Result<()> {
            let _medium = medium;
            Err(E_INVALIDARG.into())
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let owner: IClassFactory = MediumOwner {
            drops: drops.clone(),
        }
        .into();
        let medium = storage(HGLOBAL::default(), Some(owner.cast()?));
        drop(owner);
        assert!(fail(medium).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        Ok(())
    }
}
