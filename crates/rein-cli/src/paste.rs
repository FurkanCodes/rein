//! Ctrl-V: an image from the clipboard, to go with your message. It's
//! checked and kept as an image a tool gives is, and the box says where it
//! goes in the message: `[Image #1]`.
//!
//! On macOS the clipboard is asked for a PNG, or a TIFF made one (what a
//! screenshot copied, or an image copied from a browser, holds), by a
//! script macOS runs itself, so no library of rein's reads it: Claude
//! Code's asked AppleScript for PNG alone, and missed what Chrome copies.
//! On Linux, `wl-paste` or `xclip` is asked. Over SSH the clipboard is on
//! the other computer, and rein says so.

use std::path::Path;
use std::time::Duration;

use rein_llm::Image;

use crate::terminal::Terminal;

/// How long asking the clipboard may take.
const ASKING: Duration = Duration::from_secs(5);

/// What macOS is asked, in JavaScript for Automation: the clipboard's PNG,
/// or its TIFF as a PNG, written to the file it's given; `none` without.
/// A pasteboard's name after the file reads that one instead, for tests,
/// which mustn't touch yours.
#[cfg(target_os = "macos")]
const MACOS: &str = r#"
ObjC.import('AppKit');
function run(argv) {
    const board = argv.length > 1 ? $.NSPasteboard.pasteboardWithName(argv[1]) : $.NSPasteboard.generalPasteboard;
    let data = board.dataForType($.NSPasteboardTypePNG);
    if (data.isNil()) {
        const tiff = board.dataForType($.NSPasteboardTypeTIFF);
        if (tiff.isNil()) return 'none';
        const image = $.NSBitmapImageRep.imageRepWithData(tiff);
        if (image.isNil()) return 'none';
        data = image.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $());
    }
    return data.writeToFileAtomically(argv[0], true) ? 'png' : 'none';
}
"#;

/// The image on the clipboard: its bytes, or why there's none to be had.
async fn clipboard(scratch: &Path) -> Result<Vec<u8>, String> {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return Err("the clipboard is on the computer you're connecting from: put the image in the project and name it with @".into());
    }
    tokio::time::timeout(ASKING, ask(scratch)).await.map_err(|_| "the clipboard took too long to answer".to_owned())?
}

#[cfg(target_os = "macos")]
async fn ask(scratch: &Path) -> Result<Vec<u8>, String> {
    ask_board(scratch, None).await
}

/// The image on the pasteboard `board`, or the clipboard.
#[cfg(target_os = "macos")]
async fn ask_board(scratch: &Path, board: Option<&str>) -> Result<Vec<u8>, String> {
    let file = scratch.join(format!("pasted-{}.png", std::process::id()));
    let out = tokio::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", MACOS])
        .arg(&file)
        .args(board)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("can't ask macOS for the clipboard: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout);
    let bytes = if said.trim() == "png" { std::fs::read(&file).ok() } else { None };
    let _ = std::fs::remove_file(&file);
    bytes.ok_or_else(|| "there's no image on the clipboard".to_owned())
}

#[cfg(not(target_os = "macos"))]
async fn ask(_scratch: &Path) -> Result<Vec<u8>, String> {
    let tries: [(&str, &[&str]); 2] = [("wl-paste", &["--type", "image/png"]), ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"])];
    for (program, args) in tries {
        let Ok(out) = tokio::process::Command::new(program).args(args).kill_on_drop(true).output().await else { continue };
        if out.status.success() && !out.stdout.is_empty() {
            return Ok(out.stdout);
        }
        return Err("there's no image on the clipboard".into());
    }
    Err("rein reads the clipboard with wl-paste or xclip, and neither is here".into())
}

/// Ctrl-V: the clipboard's image, checked and kept in `images`, waiting in
/// the box as `[Image #n]` to go with your message; or why there's none.
pub(crate) async fn paste(terminal: &mut Terminal, images: &Path) {
    let n = terminal.pasted.len() + 1;
    let label = format!("[Image #{n}]");
    let scratch = std::env::temp_dir();
    let prepared = match clipboard(&scratch).await {
        Ok(bytes) => {
            let (what, folder) = (format!("the image you pasted as {label}"), images.to_owned());
            tokio::task::spawn_blocking(move || rein_agent::image::prepare(&bytes, &what, &folder)).await.unwrap_or_else(|e| Err(e.to_string()))
        }
        Err(why) => Err(why),
    };
    match prepared {
        Ok(image) => {
            terminal.input.insert(&format!("{label} "));
            terminal.input.notice(&format!("{label}: {}×{}, goes with your message", image.width, image.height));
            terminal.pasted.push((label, image));
        }
        Err(why) => terminal.input.notice(&format!("nothing pasted: {why}")),
    }
}

/// The pasted images whose labels are still in `message`, in order: what
/// goes with it. The rest were taken out of the box.
pub(crate) fn sent_with(message: &str, pasted: Vec<(String, Image)>) -> Vec<Image> {
    pasted.into_iter().filter(|(label, _)| message.contains(label.as_str())).map(|(_, image)| image).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screenshot copied holds a TIFF: it comes out a PNG. On a
    /// pasteboard of the test's own, never your clipboard.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_tiff_on_the_pasteboard_comes_out_a_png() {
        let board = format!("rein-test-{}", std::process::id());
        let put = format!(
            "ObjC.import('AppKit'); const rep = $.NSBitmapImageRep.alloc.initWithBitmapDataPlanesPixelsWidePixelsHighBitsPerSampleSamplesPerPixelHasAlphaIsPlanarColorSpaceNameBytesPerRowBitsPerPixel(null, 3, 2, 8, 3, false, false, $.NSDeviceRGBColorSpace, 0, 0); \
             const board = $.NSPasteboard.pasteboardWithName('{board}'); board.clearContents; board.setDataForType(rep.TIFFRepresentation, $.NSPasteboardTypeTIFF); 'ok'"
        );
        let out = std::process::Command::new("/usr/bin/osascript").args(["-l", "JavaScript", "-e", &put]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok", "{}", String::from_utf8_lossy(&out.stderr));
        let png = ask_board(&std::env::temp_dir(), Some(&board)).await.unwrap();
        assert_eq!(rein_agent::image::kind_of(&png), Some(rein_llm::ImageKind::Png));
        let image = rein_agent::image::prepare(&png, "pasted", &std::env::temp_dir().join(format!("rein-paste-{}", std::process::id()))).unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        let release = format!("ObjC.import('AppKit'); $.NSPasteboard.pasteboardWithName('{board}').releaseGlobally; 'ok'");
        let _ = std::process::Command::new("/usr/bin/osascript").args(["-l", "JavaScript", "-e", &release]).output();
        assert!(ask_board(&std::env::temp_dir(), Some(&format!("{board}-empty"))).await.unwrap_err().contains("no image"), "an empty one has none");
    }

    #[test]
    fn only_the_images_still_in_the_message_go_with_it() {
        let image = |about: &str| Image { kind: rein_llm::ImageKind::Png, data: None, file: "/k/a.png".into(), width: 1, height: 1, about: about.into() };
        let pasted = vec![("[Image #1]".to_owned(), image("one")), ("[Image #2]".to_owned(), image("two"))];
        let sent: Vec<String> = sent_with("look at [Image #2] please", pasted).into_iter().map(|image| image.about).collect();
        assert_eq!(sent, ["two"]);
    }
}
