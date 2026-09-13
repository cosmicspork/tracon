//! Stands in for `claude setup-token`: prints a sign-in URL the way the real
//! CLI does, waits for a pasted code, and prints a long-lived token.
//!
//! It reproduces the two things about the real command that the adapter has to
//! cope with and that a plain `println!` would hide: the URL arrives wrapped in
//! an OSC 8 hyperlink and SGR colour, and the prompt is answered by a pty in
//! raw mode, where Enter is CR. A code that arrives terminated by LF is
//! rejected rather than accepted, so a test cannot pass without the adapter
//! having translated it.

use std::io::{Read, Write};

fn main() {
    let mut out = std::io::stdout();
    let mut stdin = std::io::stdin();

    // The real command's authorize URL, shape and all: the redirect is a
    // hosted callback on Anthropic's own site, never a loopback one.
    let url = "https://claude.com/cai/oauth/authorize?code=true&client_id=fake-client&\
               response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth\
               %2Fcode%2Fcallback&scope=user%3Ainference&state=fake-state";
    let _ = writeln!(
        out,
        "\u{1b}[38;2;215;119;87mWelcome\u{1b}[9Gto\u{1b}[12GClaude\u{1b}[19GCode\u{1b}[39m\r"
    );
    let _ = writeln!(
        out,
        "\u{1b}[2G\u{1b}[1mThis will guide you through long-lived (1-year) auth token setup.\u{1b}[22m\r"
    );
    let _ = writeln!(
        out,
        "\u{1b}[2G\u{1b}[38;2;153;153;153mBrowser didn't open? Use the url below to sign in (c to copy)\u{1b}[39m\r"
    );
    let _ = writeln!(
        out,
        "\u{1b}]8;id=1t7is4t;{url}\u{7}\u{1b}[38;2;153;153;153m{url}\u{1b}[39m\u{1b}]8;;\u{7}\r"
    );
    let _ = writeln!(out, "\u{1b}[2GPaste\u{1b}[8Gcode\u{1b}[13Ghere > \r");
    let _ = out.flush();

    let mut code = String::new();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(1) => {}
            _ => std::process::exit(2),
        }
        match byte[0] {
            b'\r' => break,
            b'\n' => {
                let _ = writeln!(out, "the code arrived terminated by LF, not Enter\r");
                let _ = out.flush();
                std::process::exit(3);
            }
            other => code.push(other as char),
        }
    }
    if code.is_empty() {
        std::process::exit(4);
    }

    let _ = writeln!(out, "\u{1b}[2GYour OAuth token (valid for 364 days):\r");
    let _ = writeln!(out, "\u{1b}[1msk-ant-oat01-{code}\u{1b}[22m\r");
    let _ = writeln!(
        out,
        "\u{1b}[2GStore this token securely. You won't be able to see it again.\r"
    );
    let _ = out.flush();

    // The real command's last screen waits on a keypress; so does this, which
    // is what makes the adapter's answer to it part of the test.
    match stdin.read(&mut byte) {
        Ok(1) => {}
        _ => std::process::exit(5),
    }
}
