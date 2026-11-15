//! Sharing a picture or a clip: it is saved first (Pictures/OmaCCTV, Videos/OmaCCTV), then
//! it can be copied to the clipboard (paste it into a chat), sent with LocalSend or shown
//! in the file manager.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use gtk::glib;

/// Longest clip offered (Frigate cuts it on request; ~40 MB per minute for a 4 MP camera).
pub const MAX_CLIP: f64 = 10.0 * 60.0;

/// Where a picture (`video` = false) or clip of camera `cam` at `t` is saved.
pub fn out_path(video: bool, cam: &str, t: f64, ext: &str) -> PathBuf {
    let kind = if video { glib::UserDirectory::Videos } else { glib::UserDirectory::Pictures };
    let base = glib::user_special_dir(kind).unwrap_or_else(|| glib::home_dir().join(if video { "Videos" } else { "Pictures" }));
    let dir = base.join("OmaCCTV");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = crate::timeline::fmt(t, "%Y-%m-%d_%H-%M-%S");
    let safe: String = cam.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let mut p = dir.join(format!("{safe}_{stamp}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{safe}_{stamp}-{n}.{ext}"));
        n += 1;
    }
    p
}

/// "0:42", "12:05".
pub fn duration_label(secs: f64) -> String {
    let s = secs.round().max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn has(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

fn wl_copy(mime: &str, data: &[u8]) -> Result<(), String> {
    let mut c = Command::new("wl-copy")
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("wl-copy: {e}"))?;
    c.stdin.take().ok_or("wl-copy: no stdin")?.write_all(data).map_err(|e| e.to_string())?;
    // wl-copy forks to keep serving the clipboard; the parent exits right away.
    c.wait().map_err(|e| e.to_string())?;
    Ok(())
}

/// Copies a picture (as PNG, which chats and browsers paste) or a clip (as a file, the way a
/// file manager copies it) to the clipboard.
pub fn copy(path: &Path, video: bool, display: &gtk::gdk::Display) -> Result<(), String> {
    use gtk::prelude::*;
    if video {
        let uri = gtk::gio::File::for_path(path).uri();
        if has("wl-copy") {
            return wl_copy("text/uri-list", format!("{uri}\r\n").as_bytes());
        }
        let file = gtk::gio::File::for_path(path);
        let list = gtk::gdk::FileList::from_array(&[file]);
        return display
            .clipboard()
            .set_content(Some(&gtk::gdk::ContentProvider::for_value(&list.to_value())))
            .map_err(|e| e.to_string());
    }
    let tex = gtk::gdk::Texture::from_filename(path).map_err(|e| e.to_string())?;
    if has("wl-copy") {
        return wl_copy("image/png", &tex.save_to_png_bytes());
    }
    display.clipboard().set_texture(&tex);
    Ok(())
}

/// The command that sends files with LocalSend, if there is one.
pub fn localsend() -> Option<Vec<String>> {
    if has("omarchy-menu-share") {
        Some(vec!["omarchy-menu-share".into(), "file".into()])
    } else if has("localsend") {
        Some(vec!["localsend".into(), "--headless".into(), "send".into()])
    } else {
        None
    }
}

pub fn send(path: &Path) -> Result<(), String> {
    let cmd = localsend().ok_or("LocalSend is not installed")?;
    Command::new(&cmd[0])
        .args(&cmd[1..])
        .arg(path)
        .stdin(Stdio::null())
        .spawn()
        .map(|mut c| {
            std::thread::spawn(move || c.wait());
        })
        .map_err(|e| format!("{}: {e}", cmd[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(duration_label(42.4), "0:42");
        assert_eq!(duration_label(725.0), "12:05");
    }
}
