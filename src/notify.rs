use std::process::Command;

pub fn send(title: &str, message: &str) {
    let script: String = format!(
        "display notification {} with title {}",
        quote_applescript(message),
        quote_applescript(title),
    );

    let _ = Command::new("osascript").arg("-e").arg(script).status();
}

fn quote_applescript(text: &str) -> String {
    let escaped: String = text.replace('\\', "\\\\").replace('"', "\\\"");

    format!("\"{}\"", escaped)
}
