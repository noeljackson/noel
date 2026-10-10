//! Clipboard helpers without a GUI runtime or temporary files.
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const MAX_CLIPBOARD_BYTES: u64 = 20 * 1024 * 1024;

pub async fn image_bytes() -> Result<Vec<u8>> {
    if let Ok(command) = std::env::var("PI_CLIPBOARD_IMAGE_COMMAND") {
        return read_command(&command).await;
    }
    for command in image_commands() {
        if let Ok(bytes) = read_command(command).await {
            if image::guess_format(&bytes).is_ok() {
                return Ok(bytes);
            }
        }
    }
    Err(anyhow!("no clipboard image available; use /image <path> [prompt] or install wl-clipboard/xclip on Linux"))
}

fn image_commands() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        // AppKit reads the pasteboard's TIFF/PNG representation and encodes PNG
        // on stdout. Swift ships with the Xcode command-line tools.
        &["swift -e 'import AppKit; import Foundation; let p = NSPasteboard.general; guard let d = p.data(forType: .png) ?? p.data(forType: .tiff), let b = NSBitmapImageRep(data: d), let png = b.representation(using: .png, properties: [:]) else { exit(1) }; FileHandle.standardOutput.write(png)'"]
    } else if cfg!(target_os = "windows") {
        &["powershell.exe -NoProfile -STA -Command \"Add-Type -AssemblyName System.Windows.Forms; $image = [System.Windows.Forms.Clipboard]::GetImage(); if ($null -eq $image) { exit 1 }; $stream = New-Object System.IO.MemoryStream; $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png); [Console]::OpenStandardOutput().Write($stream.ToArray(), 0, $stream.Length)\""]
    } else {
        &[
            "wl-paste --no-newline --type image/png",
            "wl-paste --no-newline --type image/jpeg",
            "wl-paste --no-newline --type image/webp",
            "wl-paste --no-newline --type image/gif",
            "xclip -selection clipboard -t image/png -o",
            "xclip -selection clipboard -t image/jpeg -o",
            "xclip -selection clipboard -t image/webp -o",
            "xclip -selection clipboard -t image/gif -o",
        ]
    }
}

async fn read_command(command: &str) -> Result<Vec<u8>> {
    let mut child = if cfg!(target_os = "windows") {
        let mut child = Command::new("cmd.exe");
        child.args(["/C", command]);
        child
    } else {
        let mut child = Command::new("sh");
        child.args(["-c", command]);
        child
    }
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true)
    .spawn()?;
    let read = async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("clipboard stdout unavailable"))?
            .take(MAX_CLIPBOARD_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > MAX_CLIPBOARD_BYTES {
            return Err(anyhow!("clipboard image exceeds 20 MiB"));
        }
        if !child.wait().await?.success() || bytes.is_empty() {
            return Err(anyhow!("clipboard image command returned no image"));
        }
        Ok(bytes)
    };
    // Cold Swift startup can take a few seconds; all helpers remain bounded.
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .map_err(|_| anyhow!("clipboard image command timed out"))?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clipboard_helpers_return_binary_and_report_failures() {
        assert_eq!(read_command("printf '\\211PNG'").await.unwrap(), b"\x89PNG");
        assert!(read_command("exit 1").await.is_err());
        assert!(read_command("printf ''").await.is_err());
        assert!(read_command("head -c 20971521 /dev/zero")
            .await
            .unwrap_err()
            .to_string()
            .contains("20 MiB"));
    }
}

/// Local helpers first, terminal OSC 52 fallback for SSH/headless sessions.
/// OSC 52 cannot acknowledge success; report the request, not verified copying.
pub async fn copy_text(text: &str) -> Result<String> {
    if let Ok(command) = std::env::var("PI_CLIPBOARD_COMMAND") {
        write_command(&command, text).await?;
        return Ok(format!("copied to clipboard via {command}"));
    }
    let commands: &[&str] = if cfg!(target_os = "macos") {
        &["pbcopy"]
    } else if cfg!(target_os = "windows") {
        &["clip.exe"]
    } else {
        &[
            "wl-copy",
            "xclip -selection clipboard",
            "xsel --clipboard --input",
        ]
    };
    for command in commands {
        if write_command(command, text).await.is_ok() {
            return Ok(format!("copied to clipboard via {command}"));
        }
    }
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(osc52(text)?.as_bytes())?;
    stdout.flush()?;
    Ok("clipboard requested via OSC 52 (terminal must allow it)".into())
}

fn osc52(text: &str) -> Result<String> {
    // Common terminal limits are roughly 100 KiB encoded. Never silently truncate.
    if text.len() > 74_000 {
        return Err(anyhow!("selection too large for terminal clipboard; configure PI_CLIPBOARD_COMMAND or use /export"));
    }
    Ok(format!(
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    ))
}

async fn write_command(command: &str, text: &str) -> Result<()> {
    let mut child = if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", command]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    }
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .kill_on_drop(true)
    .spawn()?;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("clipboard stdin unavailable"))?;
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin);
        if !child.wait().await?.success() {
            return Err(anyhow!("clipboard command failed"));
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("clipboard command timed out"))?
}

#[cfg(test)]
mod text_tests {
    use super::*;

    #[test]
    fn osc52_encodes_utf8_and_control_bytes_without_terminal_injection() {
        let text = "hello 界\n\x1b]52;c;bad\x07";
        let sequence = osc52(text).unwrap();
        assert_eq!(sequence.matches('\x1b').count(), 1);
        let encoded = sequence
            .strip_prefix("\x1b]52;c;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            text.as_bytes()
        );
        assert!(osc52(&"x".repeat(74_001)).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn text_helpers_are_bounded_and_report_failure() {
        write_command("cat > /dev/null", "hello 界").await.unwrap();
        assert!(write_command("exit 1", "hello").await.is_err());
        assert!(write_command("sleep 5", "hello")
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }
}
