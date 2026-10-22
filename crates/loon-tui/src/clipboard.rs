//! Native clipboard helpers with closed stdin and a bounded wait; OSC 52 is the fallback.
use tokio::{io::AsyncWriteExt, process::Command};
pub async fn copy(text: &str) -> bool {
    for (command, args) in [
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("pbcopy", vec![]),
    ] {
        let Ok(mut child) = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        else {
            continue;
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let Some(mut input) = child.stdin.take() else {
                return false;
            };
            if input.write_all(text.as_bytes()).await.is_err() {
                return false;
            }
            drop(input); // clipboard tools wait for EOF before exiting
            matches!(child.wait().await,Ok(status) if status.success())
        })
        .await
        .unwrap_or(false);
        if result {
            return true;
        }
    }
    false
}
