use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOLDERID_Programs, FileOpenDialog, IFileOpenDialog, IShellLinkW, KF_FLAG_DEFAULT,
    SHGetKnownFolderPath, SIGDN_FILESYSPATH, ShellLink,
};
use windows::core::{HRESULT, HSTRING, Interface, PWSTR, w};

pub struct ComApartment;

impl ComApartment {
    pub fn enter() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .context("initializing COM")?;
        Ok(Self)
    }

    pub fn programs_folder(&self) -> Result<PathBuf> {
        let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None) }
            .context("locating the Start menu programs folder")?;
        Self::take_string(raw).map(PathBuf::from)
    }

    pub fn pick_executable(&self, title: &str) -> Result<Option<PathBuf>> {
        let dialog: IFileOpenDialog =
            unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                .context("creating the file dialog")?;
        let filters = [COMDLG_FILTERSPEC {
            pszName: w!("Game executable (*.exe)"),
            pszSpec: w!("*.exe"),
        }];
        unsafe {
            dialog.SetTitle(&HSTRING::from(title))?;
            dialog.SetFileTypes(&filters)?;
        }
        match unsafe { dialog.Show(Some(HWND::default())) } {
            Err(error) if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => {
                return Ok(None);
            }
            outcome => outcome.context("showing the file dialog")?,
        }
        let item = unsafe { dialog.GetResult() }.context("reading the selected file")?;
        let raw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }?;
        Self::take_string(raw).map(|path| Some(PathBuf::from(path)))
    }

    pub fn create_shortcut(&self, shortcut: &Shortcut) -> Result<()> {
        let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
            .context("creating a shell link")?;
        unsafe {
            link.SetPath(&HSTRING::from(shortcut.target.as_path()))?;
            link.SetArguments(&HSTRING::from(shortcut.arguments.as_str()))?;
            link.SetWorkingDirectory(&HSTRING::from(shortcut.working_directory.as_path()))?;
            link.cast::<IPersistFile>()?
                .Save(&HSTRING::from(shortcut.location.as_path()), true)?;
        }
        Ok(())
    }

    fn take_string(raw: PWSTR) -> Result<String> {
        let text = unsafe { raw.to_string() };
        unsafe { CoTaskMemFree(Some(raw.0 as *const _)) };
        Ok(text?)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub struct Shortcut {
    location: PathBuf,
    target: PathBuf,
    arguments: String,
    working_directory: PathBuf,
}

impl Shortcut {
    pub fn new(folder: &Path, name: &str, target: &Path, arguments: &str) -> Self {
        Self {
            location: folder.join(format!("{name}.lnk")),
            target: target.to_path_buf(),
            arguments: arguments.to_owned(),
            working_directory: target.parent().unwrap_or(folder).to_path_buf(),
        }
    }
}
