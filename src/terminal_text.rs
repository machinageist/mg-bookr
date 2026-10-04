// Author: Jeff
// Date: 2026-09-24
// Description: Remove terminal-active characters from untrusted display text
// Notes: Keep ordinary Unicode while rejecting control and bidi formatting characters

// Remove terminal controls and invisible directional formatting from display text
pub fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|character| !terminal_active_control(*character))
        .collect()
}

// Identify characters that can control a terminal or spoof displayed order
fn terminal_active_control(character: char) -> bool {
    matches!(
        character,
        '\u{0000}'..='\u{001f}'
            | '\u{007f}'..='\u{009f}'
            | '\u{061c}'
            | '\u{200e}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{206f}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_controls_and_bidi_formatting_are_removed() {
        let esc = char::from_u32(27).expect("ESC");
        let bel = char::from_u32(7).expect("BEL");
        let text = format!("safe{esc}]8;;https://evil.test{bel}{esc}[2J\u{202e}ok");
        let cleaned = sanitize_terminal_text(&text);
        assert_eq!(cleaned, "safe]8;;https://evil.test[2Jok");
    }
}
