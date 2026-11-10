use serde::Serialize;
use serde_json::Value;

/// How much damage an action could do. Ordered from least to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Reads state without changing it.
    Observe,
    /// A contained change: an ordinary file in the home directory, or an app
    /// in its own VM.
    Modify,
    /// Anything else, including every command not proven read-only.
    Privileged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Ask,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Policy {
    /// Run `Modify` actions without asking. `Privileged` actions always ask.
    pub auto_approve_modify: bool,
}

impl Policy {
    pub fn verdict(self, risk: Risk) -> Verdict {
        match risk {
            Risk::Observe => Verdict::Allow,
            Risk::Modify if self.auto_approve_modify => Verdict::Allow,
            Risk::Modify | Risk::Privileged => Verdict::Ask,
        }
    }
}

/// Asks the user whether an action may run.
pub trait Approver {
    fn approve(&self, tool: &str, args: &Value, risk: Risk) -> bool;
}

/// Programs with no option that writes a file, changes state or runs another
/// program. `journalctl` is the exception and is checked separately.
const READ_ONLY: &[&str] = &[
    "cat",
    "df",
    "du",
    "echo",
    "free",
    "grep",
    "head",
    "id",
    "journalctl",
    "ls",
    "lsblk",
    "lscpu",
    "lspci",
    "lsusb",
    "ps",
    "pwd",
    "stat",
    "tail",
    "uname",
    "uptime",
    "wc",
    "whoami",
];

/// The only long options `journalctl` may be given; others can rotate or
/// delete logs, and GNU option parsing accepts abbreviations of them.
const JOURNALCTL_LONG_OPTIONS: &[&str] = &[
    "--boot",
    "--dmesg",
    "--grep",
    "--lines",
    "--no-pager",
    "--output",
    "--priority",
    "--reverse",
    "--since",
    "--system",
    "--unit",
    "--until",
    "--user",
];

/// Characters with no special meaning to the shell. A command containing
/// anything else is never rated `Observe`, so quoting, expansion, globbing,
/// redirection and substitution cannot hide what runs.
const PLAIN_PUNCTUATION: &[char] = &['_', ' ', '.', '/', ':', '=', '@', '%', '+', ',', '-'];
const PIPE: char = '|';

/// System files that hold credentials.
const SECRET_PATHS: &[&str] = &["/etc/shadow", "/etc/gshadow", "/etc/sudoers", "/environ"];

/// Rates a shell command line.
///
/// A command is `Observe` only when it is a pipeline of allow-listed
/// read-only programs written with plain characters. Everything else is
/// `Privileged`: the classifier does not try to understand shell syntax, so
/// it never guesses that an unfamiliar command is harmless.
pub fn classify_command(command: &str) -> Risk {
    let is_plain = command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || PLAIN_PUNCTUATION.contains(&c) || c == PIPE);
    if is_plain && command.split(PIPE).all(is_read_only) {
        Risk::Observe
    } else {
        Risk::Privileged
    }
}

fn is_read_only(segment: &str) -> bool {
    let mut words = segment.split(' ').filter(|word| !word.is_empty());
    // The program is matched by bare name, so a look-alike in another
    // directory does not qualify; `run_command` resolves it on a fixed PATH.
    let Some(program) = words.next().filter(|name| READ_ONLY.contains(name)) else {
        return false;
    };
    words.all(|argument| {
        !is_sensitive_path(argument) && (program != "journalctl" || is_safe_journalctl(argument))
    })
}

fn is_safe_journalctl(argument: &str) -> bool {
    let option = argument.split('=').next().unwrap_or(argument);
    !option.starts_with("--") || JOURNALCTL_LONG_OPTIONS.contains(&option)
}

/// Whether a path points at credentials or settings: any hidden file or
/// directory, or a known secret system file.
pub(crate) fn is_sensitive_path(path: &str) -> bool {
    let has_hidden_component = path
        .split('/')
        .any(|part| part.starts_with('.') && part != "." && part != "..");
    has_hidden_component || SECRET_PATHS.iter().any(|secret| path.contains(secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_rated(expected: Risk, commands: &[&str]) {
        for command in commands {
            assert_eq!(classify_command(command), expected, "{command:?}");
        }
    }

    #[test]
    fn rates_plain_inspection_commands_as_observe() {
        assert_rated(
            Risk::Observe,
            &[
                "ls -la /etc",
                "ps aux",
                "df -h /home",
                "cat /etc/os-release",
                "uname -r",
                "journalctl -u sshd -n 50 --no-pager",
                "journalctl --since=today",
            ],
        );
    }

    #[test]
    fn rates_a_read_only_pipeline_as_observe() {
        assert_rated(Risk::Observe, &["ps aux | grep ollama | wc -l", "ls|wc -l"]);
    }

    #[test]
    fn rates_every_other_program_as_privileged() {
        assert_rated(
            Risk::Privileged,
            &[
                "touch notes.txt",
                "rm notes.txt",
                "git status",
                "sudo ls",
                "systemctl restart sshd",
                "perl -e 1",
                "find / -delete",
                "nvidia-smi -f out",
                "ls | xargs rm",
            ],
        );
    }

    #[test]
    fn rates_an_empty_command_as_privileged() {
        assert_rated(Risk::Privileged, &["", "  ", "ls |", "| ls"]);
    }

    #[test]
    fn does_not_trust_a_program_named_by_path() {
        assert_rated(Risk::Privileged, &["./ls", "/tmp/x/cat a", "/usr/bin/ls"]);
    }

    #[test]
    fn rates_anything_with_shell_syntax_as_privileged() {
        assert_rated(
            Risk::Privileged,
            &[
                "echo hi > notes.txt",
                "ls; reboot",
                "ls && reboot",
                "ls & reboot",
                "ls\nreboot",
                "echo $(reboot)",
                "echo `reboot`",
                "cat <(reboot)",
                "(ls)",
                "{ls,-l}",
                "s=ls; $s",
                "$'ls'",
                "\"ls\"",
                "'ls'",
                "l\\s",
                "ls *",
                "ls ?",
                "ls [a]",
                "ls ~",
                "FOO=1 ls",
                "ls\treboot",
                "ls #x",
                "ls !x",
            ],
        );
    }

    #[test]
    fn rates_reading_credentials_and_hidden_files_as_privileged() {
        assert_rated(
            Risk::Privileged,
            &[
                "cat /home/u/.ssh/id_ed25519",
                "cat .bashrc",
                "grep -r token .config",
                "cat /etc/shadow",
                "cat /proc/1/environ",
                "head /home/u/docs/../.netrc",
            ],
        );
    }

    #[test]
    fn allows_the_current_and_parent_directory() {
        assert_rated(Risk::Observe, &["ls .", "ls ..", "ls ./src", "du -sh ../other"]);
    }

    #[test]
    fn rates_journal_maintenance_as_privileged() {
        assert_rated(
            Risk::Privileged,
            &[
                "journalctl --vacuum-size=1M",
                "journalctl --rotate",
                "journalctl --vac=1",
                "journalctl --flush",
            ],
        );
    }

    #[test]
    fn orders_risk_from_observe_to_privileged() {
        assert!(Risk::Observe < Risk::Modify);
        assert!(Risk::Modify < Risk::Privileged);
    }

    #[test]
    fn allows_observation_and_asks_for_the_rest_by_default() {
        let policy = Policy::default();

        assert_eq!(policy.verdict(Risk::Observe), Verdict::Allow);
        assert_eq!(policy.verdict(Risk::Modify), Verdict::Ask);
        assert_eq!(policy.verdict(Risk::Privileged), Verdict::Ask);
    }

    #[test]
    fn never_auto_approves_privileged_actions() {
        let policy = Policy {
            auto_approve_modify: true,
        };

        assert_eq!(policy.verdict(Risk::Modify), Verdict::Allow);
        assert_eq!(policy.verdict(Risk::Privileged), Verdict::Ask);
    }
}
