//! Opening a link in the browser: the one in `$BROWSER`, or the system's.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rein_llm::reqwest::Url;

/// What opens a link on Windows, where a program in WSL can reach it too.
/// No shell reads the link on the way. `cmd /c start` and PowerShell do,
/// and an MCP server chooses its own sign-in link: one with `$(…)` in it
/// ran commands through mcp-remote (CVE-2025-6514).
const RUNDLL32: &str = "rundll32.exe";
const OPENS_LINKS: &str = "url.dll,FileProtocolHandler";

/// Where Windows keeps it, for a WSL set up to leave Windows off `PATH`.
const RUNDLL32_IN_WSL: &str = "/mnt/c/Windows/System32/rundll32.exe";

/// Opens `link`, if it's a web page, and says whether it did.
///
/// `open` and `xdg-open` hand the link over and exit at once, but a browser
/// named in `$BROWSER` may run until it's closed, and rein has one thread:
/// it waits a moment for a failure, and a browser still running then is
/// showing the page. `$BROWSER` alone is tried if it's set, so that setting
/// it to `false` opens nothing.
pub(crate) async fn open(link: &str) -> bool {
    let Some(link) = web_page(link) else { return false };
    let browser = std::env::var("BROWSER").ok().filter(|browser| !browser.trim().is_empty());
    let wsl = std::env::var_os("WSL_DISTRO_NAME").is_some_and(|name| !name.is_empty());
    for way in ways(browser.as_deref(), wsl) {
        if let Some((program, first)) = way.split_first()
            && opens(program, first, link.as_str()).await
        {
            return true;
        }
    }
    false
}

/// `link` if it's a web page. Windows opens whatever it's given, a program
/// on this computer too, and a link can come from an MCP server.
fn web_page(link: &str) -> Option<Url> {
    Url::parse(link).ok().filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// How to open a link here, best first: each a program, and what it's
/// given before the link.
fn ways(browser: Option<&str>, wsl: bool) -> Vec<Vec<&str>> {
    match browser {
        // A path with a space in it, as WSL's own guides have you set:
        // `/mnt/c/Program Files/Google/Chrome/Application/chrome.exe`.
        Some(browser) if Path::new(browser).is_file() => vec![vec![browser]],
        Some(browser) => vec![browser.split_whitespace().collect()],
        // Windows' browser first: it's the one you're signed in to things
        // in, and `xdg-open` there may say it worked with none to open.
        None if wsl => vec![vec![RUNDLL32, OPENS_LINKS], vec![RUNDLL32_IN_WSL, OPENS_LINKS], vec!["xdg-open"]],
        None if cfg!(target_os = "macos") => vec![vec!["open"]],
        None if cfg!(windows) => vec![vec![RUNDLL32, OPENS_LINKS]],
        None => vec![vec!["xdg-open"]],
    }
}

/// Whether `program` opened `link`. It's given nothing to read: what you
/// type is for rein.
async fn opens(program: &str, first: &[&str], link: &str) -> bool {
    let child = tokio::process::Command::new(program).args(first).arg(link).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = child else { return false };
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(status) => status.is_ok_and(|status| status.success()),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_web_page_is_opened() {
        assert_eq!(web_page("https://a.example/sign-in?x=1&y=a|b").map(String::from).as_deref(), Some("https://a.example/sign-in?x=1&y=a|b"));
        assert!(web_page("http://127.0.0.1:1455/auth").is_some());
        for link in ["file:///C:/Windows/System32/calc.exe", "C:\\Windows\\System32\\calc.exe", "calc.exe", "-flag", "javascript:alert(1)", "ms-settings:", ""] {
            assert!(web_page(link).is_none(), "{link}");
        }
    }

    #[test]
    fn a_link_cant_leave_the_url_it_was_parsed_as() {
        let link = web_page("https://a.example/x y\"z?q=\" & calc").unwrap();
        assert!(!link.as_str().contains([' ', '"']), "spaces and quotes would split it into arguments: {link}");
    }

    #[test]
    fn wsl_tries_windows_then_the_linux_way() {
        let way = ways(None, true);
        assert_eq!(way[0], ["rundll32.exe", "url.dll,FileProtocolHandler"], "{way:?}");
        assert_eq!(way[1], ["/mnt/c/Windows/System32/rundll32.exe", "url.dll,FileProtocolHandler"], "{way:?}");
        assert_eq!(way[way.len() - 1], ["xdg-open"], "{way:?}");
    }

    #[test]
    fn a_browser_youve_named_is_the_only_one_tried() {
        assert_eq!(ways(Some("false"), true), [["false"]], "tests rely on this to open nothing");
        assert_eq!(ways(Some("firefox --new-tab"), false), [["firefox", "--new-tab"]]);
    }

    #[test]
    fn a_browser_in_a_folder_with_a_space_is_one_program() {
        let dir = std::env::temp_dir().join(format!("rein-browser-{}", std::process::id()));
        let path = dir.join("Program Files/chrome.exe");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(ways(Some(path), true), [[path]]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
