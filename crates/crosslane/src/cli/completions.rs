//! Static shell-completion generation.

use clap::ValueEnum;
use clap_complete::{generate, shells};
use std::io::Write;

use crate::{cli::guide::Topic, CompletionShell};

pub fn print(shell: CompletionShell) -> std::io::Result<()> {
    let mut command = super::command();
    let binary_name = command.get_name().to_owned();
    let mut output = std::io::stdout();
    match shell {
        CompletionShell::Bash => generate(shells::Bash, &mut command, binary_name, &mut output),
        CompletionShell::Zsh => generate(shells::Zsh, &mut command, binary_name, &mut output),
        CompletionShell::Fish => {
            generate(shells::Fish, &mut command, binary_name, &mut output);
            let topics = Topic::value_variants()
                .iter()
                .map(|topic| topic.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(
                output,
                "complete -c crosslane -n \"__fish_crosslane_using_subcommand guide\" -f -a \"{topics}\""
            )?;
        }
    }
    Ok(())
}
