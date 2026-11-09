//! CLI structural tests.

use clap::CommandFactory;

/// `debug_assert` performs clap's full internal consistency validation:
/// conflicting flags, missing required values, duplicate names, etc.
#[test]
fn cli_definition_is_internally_consistent() {
    gannet_cli::Cli::command().debug_assert();
}

#[test]
fn top_level_subcommands_exist() {
    let cmd = gannet_cli::Cli::command();
    let names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    for expected in ["dev", "health", "config"] {
        assert!(names.contains(&expected), "missing subcommand {expected}");
    }
}
