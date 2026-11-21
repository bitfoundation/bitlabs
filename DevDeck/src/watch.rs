//! Repository file watcher using ReadDirectoryChangesW on a background thread.
//! The thread blocks inside the kernel, so an idle repository costs zero CPU.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

/// The directory handle belongs to the Watcher, not the thread: it is closed only after the
/// thread has ended, so CancelIoEx never hits a closed handle whose value was reused.
pub struct Watcher {
    handle: isize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// `git_dir`: `root` is a repository's git dir kept outside its work tree (a linked
    /// worktree's or a submodule's): its entries are judged like those under `.git/`.
    pub fn start(root: &Path, hwnd: HWND, msg: u32, git_dir: bool) -> Option<Watcher> {
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let handle = h as isize;
        let stop = Arc::new(AtomicBool::new(false));
        let sc = stop.clone();
        let hwnd_val = hwnd as isize;
        let thread = std::thread::Builder::new()
            .name("watcher".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut buf = vec![0u32; 16 * 1024]; // 64 KiB, DWORD aligned
                // a stop between two reads finds no read to cancel: checked before each one too
                while !sc.load(Ordering::SeqCst) {
                    let mut returned = 0u32;
                    let h = handle as HANDLE;
                    let ok = unsafe {
                        ReadDirectoryChangesW(
                            h,
                            buf.as_mut_ptr() as *mut _,
                            (buf.len() * 4) as u32,
                            1,
                            FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE,
                            &mut returned,
                            std::ptr::null_mut(),
                            None,
                        )
                    };
                    if sc.load(Ordering::SeqCst) {
                        break;
                    }
                    if ok == 0 {
                        break;
                    }
                    let relevant = if returned == 0 { true } else { unsafe { any_relevant(buf.as_ptr() as *const u8, git_dir) } };
                    if relevant {
                        unsafe {
                            PostMessageW(hwnd_val as HWND, msg, 0, 0);
                        }
                    }
                }
            });
        match thread {
            Ok(t) => Some(Watcher { handle, stop, thread: Some(t) }),
            Err(_) => {
                unsafe { CloseHandle(h) };
                None
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(t) = self.thread.take() else { return };
        // cancel until the thread has seen `stop` (it may be between two reads); it ends within
        // microseconds, and a thread that doesn't is left with its handle rather than closing it
        for _ in 0..500 {
            unsafe {
                CancelIoEx(self.handle as HANDLE, std::ptr::null());
            }
            if t.is_finished() {
                unsafe {
                    CloseHandle(self.handle as HANDLE);
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

unsafe fn any_relevant(mut p: *const u8, git_dir: bool) -> bool {
    loop {
        let info = &*(p as *const FILE_NOTIFY_INFORMATION);
        let len = info.FileNameLength as usize / 2;
        let name = std::slice::from_raw_parts(info.FileName.as_ptr(), len);
        let mut s = String::from_utf16_lossy(name).replace('\\', "/");
        if git_dir {
            s.insert_str(0, ".git/");
        }
        if is_relevant(&s) {
            return true;
        }
        if info.NextEntryOffset == 0 {
            return false;
        }
        p = p.add(info.NextEntryOffset as usize);
    }
}

pub fn is_relevant(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    if p == ".git" {
        return false;
    }
    if let Some(inner) = p.strip_prefix(".git/") {
        // git's own transient *.lock files; work tree ones (Cargo.lock, yarn.lock) are real edits
        if inner.ends_with(".lock") {
            return false;
        }
        return inner == "index" || inner == "head" || inner == "config" || inner.starts_with("refs") || inner == "packed-refs" || inner == "merge_head" || inner.starts_with("rebase-") || inner == "fetch_head" || inner == "cherry_pick_head" || inner == "revert_head" || inner == "stash";
    }
    // a nested repository's own .git (a clone inside the work tree) churns on its git ops
    !(p.starts_with("node_modules/") || p.contains("/node_modules/") || p.contains("/.git/"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn relevance() {
        assert!(super::is_relevant("src/main.rs"));
        assert!(super::is_relevant(".git/index"));
        assert!(!super::is_relevant(".git/index.lock"));
        assert!(!super::is_relevant(".git/refs/heads/main.lock"));
        assert!(super::is_relevant("Cargo.lock"));
        assert!(super::is_relevant("web/yarn.lock"));
        assert!(!super::is_relevant(".git/objects/ab/cdef"));
        assert!(super::is_relevant(".git/refs/heads/main"));
        assert!(!super::is_relevant("node_modules/x/y.js"));
        assert!(super::is_relevant(".git/config"));
        assert!(!super::is_relevant("vendor/x/.git/objects/ab/cd"));
    }
}
