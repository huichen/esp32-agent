//! Bench provisioning over the serial console.
//!
//! The board has a web portal for WiFi credentials, but using it means joining the
//! board's own access point — which on a machine that is also talking to a model over the
//! network is a self-inflicted outage. The console is already plugged in for flashing, so
//! the same credentials can go down the same wire:
//!
//! ```text
//! provision <ssid> <password>
//! ```
//!
//! The firmware applies the line to the stored config and restarts the radio. Keeping the
//! *parsing* here rather than in the firmware means it is testable on the host, which is
//! the half a bench can actually check without a board in hand.

/// What a console line asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionCmd {
    Set {
        ssid: String,
        password: String,
    },
    /// The line was recognised but the arguments were not usable; the text is a hint for
    /// whoever is looking at the console.
    Bad(&'static str),
}

/// Parse one console line.
///
/// Returns `None` for anything that is not a provisioning line, so the caller can ignore
/// log noise and keystrokes. The password may contain spaces (the rest of the line after
/// the SSID), because a SSID with a space in it — `Home Network` — would otherwise be
/// indistinguishable; an SSID with a space still works, the last word is simply taken as
/// part of the password.
pub fn parse_line(line: &str) -> Option<ProvisionCmd> {
    let line = line.trim();
    let rest = line.strip_prefix("provision")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(ProvisionCmd::Bad("usage: provision <ssid> <password>"));
    }
    // SSID is the first whitespace-delimited word, password is everything after it.
    let Some((ssid, password)) = rest.split_once(char::is_whitespace) else {
        return Some(ProvisionCmd::Bad("usage: provision <ssid> <password>"));
    };
    let ssid = ssid.trim();
    let password = password.trim();
    if ssid.is_empty() {
        return Some(ProvisionCmd::Bad("ssid is empty"));
    }
    if password.is_empty() {
        return Some(ProvisionCmd::Bad("password is empty (an open network needs one anyway)"));
    }
    if ssid.len() > 32 {
        return Some(ProvisionCmd::Bad("ssid is longer than 32 bytes"));
    }
    if password.len() > 64 {
        return Some(ProvisionCmd::Bad("password is longer than 64 bytes"));
    }
    Some(ProvisionCmd::Set { ssid: ssid.to_string(), password: password.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provision_line_carries_both_credentials() {
        assert_eq!(
            parse_line("provision lab-5g hunter2"),
            Some(ProvisionCmd::Set { ssid: "lab-5g".into(), password: "hunter2".into() })
        );
        // Trailing whitespace and a CR from the serial line are not part of anything.
        assert_eq!(
            parse_line("  provision lab-5g hunter2  \r\n"),
            Some(ProvisionCmd::Set { ssid: "lab-5g".into(), password: "hunter2".into() })
        );
        // A password with a space keeps it.
        assert_eq!(
            parse_line("provision lab-5g two words"),
            Some(ProvisionCmd::Set { ssid: "lab-5g".into(), password: "two words".into() })
        );
    }

    #[test]
    fn anything_else_is_left_alone() {
        for line in ["", "provisioned", "provision", " hello", "I (1234) boot: ready"] {
            assert!(parse_line(line).is_none(), "`{line}` should be ignored");
        }
    }

    #[test]
    fn a_broken_line_says_what_is_wrong_instead_of_being_ignored() {
        assert_eq!(
            parse_line("provision onlyssid"),
            Some(ProvisionCmd::Bad("usage: provision <ssid> <password>"))
        );
        assert_eq!(
            parse_line("provision onlyssid "),
            Some(ProvisionCmd::Bad("usage: provision <ssid> <password>"))
        );
        let long = "x".repeat(65);
        assert!(matches!(parse_line(&format!("provision ssid {long}")), Some(ProvisionCmd::Bad(_))));
    }
}
