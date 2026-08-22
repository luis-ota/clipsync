//! X11 clipboard backend implemented with `xclip`.

use std::io::Read;
use std::process::{Command, Stdio};

use super::{ClipboardManager, ClipboardSnapshot, MIME_TEXT, MIME_TEXT_PLAIN};
use crate::error::{Error, Result};

pub(super) const MAX_BYTES: usize = 25 * 1024 * 1024;

pub(super) fn read(preferred_mimes: &[String]) -> Result<Option<ClipboardSnapshot>> {
    let targets = Command::new("xclip")
        .args(["-selection", "clipboard", "-target", "TARGETS", "-o"])
        .output()
        .map_err(|e| Error::Clipboard(format!("falha executando xclip TARGETS: {e}")))?;
    if !targets.status.success() {
        return Ok(None);
    }

    let available = String::from_utf8_lossy(&targets.stdout);
    let Some(mime) = preferred_mimes
        .iter()
        .find(|mime| target_available(mime, &available))
    else {
        return Ok(None);
    };

    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard", "-target", mime, "-o"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Clipboard(format!("falha executando xclip {mime}: {e}")))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut oversized = false;
    if let Some(stdout) = child.stdout.as_mut() {
        loop {
            let read = stdout
                .read(&mut chunk)
                .map_err(|e| Error::Clipboard(format!("falha lendo xclip {mime}: {e}")))?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.len() > MAX_BYTES {
                oversized = true;
                break;
            }
        }
    }
    if oversized {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::Clipboard(format!(
            "conteúdo X11 excede o limite de {MAX_BYTES} bytes"
        )));
    }
    let status = child
        .wait()
        .map_err(|e| Error::Clipboard(format!("falha aguardando xclip {mime}: {e}")))?;
    if !status.success() || bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClipboardManager::snapshot(mime, bytes)))
}

pub(super) fn write(mime: &str, bytes: &[u8]) -> Result<()> {
    if !super::is_supported_image_mime(mime) && !is_text_mime(mime) {
        return Err(Error::Protocol(format!("MIME X11 não suportado: {mime}")));
    }
    if bytes.len() > MAX_BYTES {
        return Err(Error::Clipboard(format!(
            "conteúdo X11 excede o limite de {MAX_BYTES} bytes"
        )));
    }
    let target = match mime {
        MIME_TEXT | MIME_TEXT_PLAIN => "UTF8_STRING",
        _ => mime,
    };
    let mut command = Command::new("xclip");
    command.args(["-selection", "clipboard", "-target", target, "-i"]);
    super::run_backend_tool(&mut command, bytes, "xclip")
}

fn target_available(mime: &str, targets: &str) -> bool {
    targets.lines().map(str::trim).any(|target| {
        target.eq_ignore_ascii_case(mime)
            || (mime == MIME_TEXT && matches!(target, "UTF8_STRING" | "text/plain"))
            || (mime == MIME_TEXT_PLAIN && target == "UTF8_STRING")
    })
}

fn is_text_mime(mime: &str) -> bool {
    matches!(mime, MIME_TEXT | MIME_TEXT_PLAIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{MIME_JPEG, MIME_PNG};

    #[test]
    fn selects_only_advertised_targets_and_maps_utf8_text() {
        let targets = "TARGETS\nUTF8_STRING\nimage/png\n";
        assert!(target_available(MIME_TEXT, targets));
        assert!(target_available(MIME_TEXT_PLAIN, targets));
        assert!(target_available(MIME_PNG, targets));
        assert!(!target_available(MIME_JPEG, targets));
    }

    #[test]
    fn rejects_unlisted_mime() {
        assert!(!target_available("image/gif", "TARGETS\nimage/png\n"));
    }
}
