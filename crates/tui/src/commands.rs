//! The slash-command registry: what `/` completion offers and what
//! Enter executes. Adding a command is one entry here plus one arm in
//! `App::run_command` — the completion menu inherits both.

/// One slash command: the name (without the `/`) and what it does.
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
}

/// The registry, sorted by name — the completion list inherits this
/// order, so it stays stable.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "exit",
        description: "quit the session",
    },
    Command {
        name: "keybinds",
        description: "list the registered keybindings and what they do",
    },
    Command {
        name: "model",
        description: "pick a model for this session (ctrl-s saves the default)",
    },
    Command {
        name: "name",
        description: "name this session; bare /name shows the current name",
    },
    Command {
        name: "new",
        description: "start a new session for this directory",
    },
    Command {
        name: "quit",
        description: "alias for /exit",
    },
    Command {
        name: "resume",
        description: "switch to another session from this directory",
    },
    Command {
        name: "settings",
        description: "toggle session policy for this session (ctrl-s saves)",
    },
    Command {
        name: "thinking",
        description: "show or set the thinking effort (/thinking high, auto = provider decides)",
    },
    Command {
        name: "tasks",
        description: "list background tasks; /tasks <id> shows one live",
    },
    Command {
        name: "tools",
        description: "list the registered tools",
    },
];

/// Commands whose name starts with `typed` (lowercased, without the
/// leading `/`). An empty `typed` matches everything — typing `/`
/// alone lists the whole menu.
pub fn matching(typed: &str) -> Vec<&'static Command> {
    let typed = typed.to_lowercase();
    COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(&typed))
        .collect()
}

/// Exact-name lookup for dispatch.
pub fn find_exact(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prefix_lists_everything() {
        assert_eq!(matching("").len(), COMMANDS.len());
    }

    #[test]
    fn prefix_matches_case_insensitively() {
        assert!(matching("MO").iter().any(|command| command.name == "model"));
        assert!(matching("zz").is_empty());
    }

    #[test]
    fn find_exact_requires_the_full_name() {
        assert!(find_exact("model").is_some());
        assert!(find_exact("mod").is_none());
        assert!(find_exact("").is_none());
    }

    #[test]
    fn registry_names_are_well_formed() {
        for command in COMMANDS {
            assert!(!command.name.is_empty());
            assert!(!command.name.contains(' '));
            assert!(command.description.len() > 8);
        }
    }
}
