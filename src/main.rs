mod ui;

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, ValueHint};
use hizuke::{control, engine, metadata, planner};
use metadata::{Scan, Timezone};
use planner::{DuplicateChoice, DuplicateGroup};
use ui::{Input, Output, interactive, write_json};

#[derive(Parser)]
#[command(
    version,
    subcommand_precedence_over_arg = true,
    override_usage = "hizuke [OPTIONS] [DIRECTORY]\n       hizuke <COMMAND> [OPTIONS]",
    about = "Rename photos and MP4 videos. Metadata first. Originals recoverable.",
    long_about = "Rename photos and MP4 videos to YYYY-MM-DD HH.MM.SS.ext. Just run hizuke in your media directory: review the plan, choose among exact duplicates, and confirm. Outside a terminal, the default is a read-only preview.",
    after_help = "Examples:\n  hizuke ./photos             Review and rename interactively\n  hizuke                      Use the current directory\n  hizuke ./photos --dry-run    Preview without changes\n  hizuke apply ./photos -y --duplicates keep-all\n  hizuke undo ./photos         Restore original names\n\nDefaults: current directory, no recursion, camera wall time, ask about duplicates, no deletion.\nA directory named like a command can be passed as ./preview or after --."
)]
struct Cli {
    #[command(flatten)]
    output: Output,
    #[command(flatten)]
    run: RunArgs,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show a read-only plan, without questions or filesystem writes
    #[command(visible_alias = "plan")]
    Preview(ScanArgs),
    /// Review, ask about duplicates, and apply recoverable changes
    #[command(visible_alias = "rename")]
    Apply(RunArgs),
    /// Preview and restore original names from a completed transaction
    Undo(TransactionArgs),
    /// Preview and restore original names after an interrupted operation
    Recover(TransactionArgs),
    /// Read transaction history; --transaction shows the exact file mapping
    History {
        #[arg(default_value = ".", value_hint = ValueHint::DirPath)]
        directory: PathBuf,
        #[arg(long)]
        transaction: Option<String>,
    },
    /// Generate shell completions to standard output
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Args)]
struct ScanArgs {
    /// Media directory; files stay in their current directories
    #[arg(default_value = ".", value_hint = ValueHint::DirPath)]
    directory: PathBuf,
    /// Include subdirectories, excluding links and separately managed libraries
    #[arg(short, long)]
    recursive: bool,
    /// EXIF camera time / local MP4 and mtime by default; UTC uses EXIF offsets
    #[arg(long, value_enum, default_value = "local")]
    timezone: Timezone,
    /// Exact duplicates: choose a keeper, retain all copies, or skip the group
    #[arg(long, value_enum, default_value = "ask")]
    duplicates: DuplicatePolicy,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    scan: ScanArgs,
    /// Apply without final confirmation; never answers duplicate questions
    #[arg(short, long)]
    yes: bool,
    /// Preview only; always takes precedence over --yes
    #[arg(short = 'n', long)]
    dry_run: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum DuplicatePolicy {
    Ask,
    KeepAll,
    Skip,
}

#[derive(Args)]
struct TransactionArgs {
    #[arg(default_value = ".", value_hint = ValueHint::DirPath)]
    directory: PathBuf,
    /// Transaction ID; defaults to latest eligible transaction
    #[arg(long)]
    transaction: Option<String>,
    /// Skip final confirmation
    #[arg(short, long)]
    yes: bool,
    /// Read-only preview of the restoration
    #[arg(short = 'n', long)]
    dry_run: bool,
}

fn main() {
    let cli = parse_cli(std::env::args_os()).unwrap_or_else(|error| error.exit());
    let output = cli.output.clone();
    let result = ctrlc::set_handler(|| {
        if !control::request_cancel() {
            // The journal remains recoverable even if a second interrupt forces exit.
            eprintln!(
                "\nForced stop. Run `hizuke recover` in the image directory before continuing."
            );
            std::process::exit(130);
        }
    })
    .context("cannot install interrupt handler")
    .and_then(|_| run(cli));
    if let Err(error) = result {
        if error.chain().any(|e| {
            e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        }) {
            return;
        }
        let cancelled =
            control::is_cancelled() || error.downcast_ref::<control::Cancelled>().is_some();
        let code = if cancelled { 130 } else { 1 };
        if output.json {
            let _ = write_json(
                &serde_json::json!({"schema_version": 1, "error": {"kind": if cancelled { "cancelled" } else { "operation_failed" }, "message": format!("{error:#}"), "exit_code": code}}),
            );
        } else {
            eprintln!(
                "{} {}",
                output.paint(
                    if cancelled { "cancelled:" } else { "error:" },
                    "31;1",
                    true
                ),
                ui::safe_text(&format!("{error:#}"))
            );
        }
        std::process::exit(code);
    }
}

fn parse_cli<I, T>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let mut command = Cli::command();
    let matches = command.clone().try_get_matches_from(args)?;
    // clap's args_conflicts_with_subcommands also rejects global options before a
    // subcommand. Check only operation-specific root arguments instead, so an
    // accidentally misplaced --dry-run can never be silently ignored.
    if matches.subcommand().is_some()
        && [
            "directory",
            "recursive",
            "timezone",
            "duplicates",
            "yes",
            "dry_run",
        ]
        .iter()
        .any(|id| matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine))
    {
        return Err(command.error(clap::error::ErrorKind::ArgumentConflict,
            "put the directory and operation options after the subcommand (example: hizuke apply ./photos --dry-run); --json, --quiet, --verbose and --color may appear anywhere"));
    }
    Cli::from_arg_matches(&matches)
}

fn run(cli: Cli) -> Result<()> {
    let output = cli.output;
    let mut input = Input::new();
    match cli.command {
        None => {
            let apply = !cli.run.dry_run
                && (cli.run.yes || (interactive() && io::stdout().is_terminal() && !output.json));
            scan_and_run(cli.run.scan, apply, cli.run.yes, &output, &mut input)
        }
        Some(Commands::Preview(args)) => scan_and_run(args, false, false, &output, &mut input),
        Some(Commands::Apply(args)) => {
            scan_and_run(args.scan, !args.dry_run, args.yes, &output, &mut input)
        }
        Some(Commands::Undo(args)) => restore(args, false, &output, &mut input),
        Some(Commands::Recover(args)) => restore(args, true, &output, &mut input),
        Some(Commands::History {
            directory,
            transaction,
        }) => {
            if let Some(id) = transaction {
                let detail = engine::inspect(&directory, &id)?;
                if output.json {
                    write_json(&detail)?;
                } else {
                    print_restore(&detail, &output, false)?;
                }
            } else {
                let entries = engine::history(&directory)?;
                if output.json {
                    write_json(&entries)?;
                } else if !output.quiet {
                    let mut out = io::stdout().lock();
                    if entries.is_empty() {
                        writeln!(out, "No transactions.")?;
                    }
                    for entry in entries {
                        writeln!(
                            out,
                            "{}  {:12}  {} file(s)",
                            entry.id, entry.status, entry.operations
                        )?;
                    }
                }
            }
            Ok(())
        }
        Some(Commands::Completions { shell }) => {
            let mut completion = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "hizuke", &mut completion);
            io::stdout().lock().write_all(&completion)?;
            Ok(())
        }
    }
}

fn scan_and_run(
    args: ScanArgs,
    apply: bool,
    yes: bool,
    output: &Output,
    input: &mut Input,
) -> Result<()> {
    if apply {
        for entry in engine::history(&args.directory)? {
            ensure!(
                !matches!(
                    entry.status.as_str(),
                    "preparing" | "applying" | "undoing" | "recovering"
                ),
                "transaction {} is unfinished; run `hizuke recover` from {:?} before applying another plan",
                entry.id,
                args.directory
            );
        }
    }
    let scan = {
        let progress = output.progress("Reading photos, MP4 videos, and dates");
        metadata::scan_with_progress(
            &args.directory,
            args.recursive,
            args.timezone,
            &|done, total| progress.update(done, total),
        )?
    };
    let groups = {
        let _progress = output.progress("Comparing duplicate candidates");
        planner::duplicate_groups(&scan)?
    };
    let mut choices = BTreeMap::new();
    for group in &groups {
        control::check_cancelled()?;
        let choice = match args.duplicates {
            DuplicatePolicy::KeepAll => DuplicateChoice::KeepAll,
            DuplicatePolicy::Skip => DuplicateChoice::Skip,
            DuplicatePolicy::Ask if !apply => DuplicateChoice::Pending,
            DuplicatePolicy::Ask => {
                ensure!(
                    interactive(),
                    "exact duplicates require an interactive choice; --yes does not select a keeper. Run in a terminal, or explicitly use --duplicates keep-all or --duplicates skip. No files were changed"
                );
                ask_duplicate(group, &scan, input)?
            }
        };
        choices.insert(group.id, choice);
    }
    let plan = planner::build_plan(&scan, &groups, &choices)?;
    if !apply {
        return if output.json {
            write_json(&plan)
        } else {
            output.plan(&plan, false)
        };
    }
    if !output.json {
        output.plan(&plan, true)?;
    }
    ensure!(plan.pending_groups == 0, "duplicate choices are unresolved");
    if plan.operations.is_empty() {
        if output.json {
            write_json(
                &serde_json::json!({"schema_version":1,"transaction": null, "changed": 0, "plan": plan}),
            )?;
        } else if !output.quiet {
            writeln!(io::stdout().lock(), "No changes needed.")?;
        }
        return Ok(());
    }
    confirm(
        &format!(
            "Apply these changes? ({} rename, {} archive)",
            plan.summary.rename, plan.summary.quarantine
        ),
        yes,
        input,
    )?;
    {
        let progress = output.progress("Checking that files are unchanged");
        for (index, image) in scan.images.iter().enumerate() {
            ensure!(
                metadata::fingerprint(&scan.root.join(&image.path))? == image.fingerprint,
                "file changed since scanning: {:?}; no files were changed",
                image.path
            );
            progress.update(index + 1, scan.images.len());
        }
    }
    let transaction = {
        let progress = output.progress("Renaming files");
        engine::apply_with_progress(&scan.root, &plan.operations, &|done, total| {
            progress.update(done, total)
        })?
    };
    if output.json {
        write_json(
            &serde_json::json!({"schema_version":1,"transaction": transaction, "changed": plan.operations.len(), "plan": plan}),
        )?;
    } else if let Some(id) = transaction
        && !output.quiet
    {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "{} {} change(s). Transaction: {id}",
            output.paint("Done.", "32;1", false),
            plan.operations.len()
        )?;
        writeln!(
            out,
            "To restore, run `hizuke undo --transaction {id}` from the image directory."
        )?;
    }
    Ok(())
}

fn restore(args: TransactionArgs, recover: bool, output: &Output, input: &mut Input) -> Result<()> {
    let entries = engine::history(&args.directory)?;
    let eligible = |status: &str| {
        if recover {
            matches!(status, "preparing" | "applying" | "undoing" | "recovering")
        } else {
            status == "committed"
        }
    };
    let entry = entries
        .iter()
        .rev()
        .find(|entry| {
            args.transaction
                .as_ref()
                .map_or_else(|| eligible(&entry.status), |id| entry.id == *id)
        })
        .context(if recover {
            "no incomplete transaction to recover"
        } else {
            "no committed transaction to undo"
        })?;
    ensure!(
        eligible(&entry.status),
        "transaction {} is {}; this command cannot restore it",
        entry.id,
        entry.status
    );
    let detail = engine::inspect(&args.directory, &entry.id)?;
    if args.dry_run {
        return if output.json {
            write_json(&detail)
        } else {
            print_restore(&detail, output, false)
        };
    }
    if !output.json {
        print_restore(&detail, output, true)?;
    }
    confirm(
        &format!(
            "Restore these original names? ({} file(s))",
            entry.operations
        ),
        args.yes,
        input,
    )?;
    let id = {
        let _progress = output.progress("Restoring original names");
        if recover {
            engine::recover(&args.directory, Some(&entry.id))?
        } else {
            engine::undo(&args.directory, Some(&entry.id))?
        }
    };
    if output.json {
        write_json(
            &serde_json::json!({"schema_version":1,"transaction":id,"restored":entry.operations,"status":"restored"}),
        )?;
    } else if !output.quiet {
        writeln!(
            io::stdout().lock(),
            "Restored {} original name(s). Transaction: {id}",
            entry.operations
        )?;
    }
    Ok(())
}

fn print_restore(detail: &engine::TransactionDetail, output: &Output, stderr: bool) -> Result<()> {
    if output.quiet {
        return Ok(());
    }
    let mut out: Box<dyn Write> = if stderr {
        Box::new(io::stderr().lock())
    } else {
        Box::new(io::stdout().lock())
    };
    writeln!(out, "Transaction: {} ({})", detail.id, detail.status)?;
    for entry in &detail.entries {
        control::check_cancelled()?;
        writeln!(
            out,
            "RESTORE {:?} -> {:?}{}",
            entry.current,
            entry.original,
            if entry.quarantined {
                " [archived duplicate]"
            } else {
                ""
            }
        )?;
        if let Some(alternate) = &entry.alternate {
            writeln!(
                out,
                "        interrupted move: file may also be at {alternate:?}; recovery verifies its identity"
            )?;
        }
    }
    writeln!(
        out,
        "{} file(s). Existing unrelated files will never be overwritten.",
        detail.entries.len()
    )?;
    Ok(())
}

fn confirm(prompt: &str, yes: bool, input: &mut Input) -> Result<()> {
    control::check_cancelled()?;
    if yes {
        return Ok(());
    }
    ensure!(
        interactive(),
        "confirmation requires a terminal; inspect with --dry-run, then use --yes for non-interactive execution. No image files were changed"
    );
    eprint!("{prompt} [y/N] ");
    io::stderr().flush()?;
    let answer = input.line()?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(control::Cancelled).context("no image files were changed")
    }
}

fn ask_duplicate(
    group: &DuplicateGroup,
    scan: &Scan,
    input: &mut Input,
) -> Result<DuplicateChoice> {
    eprintln!(
        "\nExact duplicate group {} ({} each):",
        group.id,
        ui::human_bytes(group.size)
    );
    for (index, path) in group.files.iter().enumerate() {
        control::check_cancelled()?;
        let image_index = scan
            .images
            .binary_search_by(|image| image.path.cmp(path))
            .map_err(|_| anyhow::anyhow!("missing duplicate image"))?;
        let image = &scan.images[image_index];
        eprintln!(
            "  {}) {:?}  [{}; {}]",
            index + 1,
            path,
            image.timestamp,
            image.time_source
        );
    }
    eprintln!(
        "Choose the file to keep here. Other copies are archived, never deleted; undo restores them."
    );
    loop {
        eprint!(
            "Keep [1-{}], [a]ll, [s]kip group, [q]uit: ",
            group.files.len()
        );
        io::stderr().flush()?;
        let answer = input.line()?.trim().to_ascii_lowercase();
        match answer.as_str() {
            "a" | "all" => return Ok(DuplicateChoice::KeepAll),
            "s" | "skip" => return Ok(DuplicateChoice::Skip),
            "" | "q" | "quit" => return Err(control::Cancelled).context("no files were changed"),
            _ => {
                if let Ok(index) = answer.parse::<usize>()
                    && (1..=group.files.len()).contains(&index)
                {
                    return Ok(DuplicateChoice::Keep(index - 1));
                }
            }
        }
        eprintln!("Enter a listed number, a, s, or q.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }
    #[test]
    fn defaults_and_global_options_parse() {
        let c = parse_cli(["hizuke"]).unwrap();
        assert!(c.command.is_none());
        assert_eq!(c.run.scan.directory, PathBuf::from("."));
        assert!(!c.run.yes && !c.run.dry_run && !c.run.scan.recursive);
        assert!(parse_cli(["hizuke", "--json", "preview", "."]).is_ok());
        assert!(parse_cli(["hizuke", ".", "--yes", "--dry-run"]).is_ok());
        assert!(
            parse_cli(["hizuke", "--", "preview"])
                .unwrap()
                .command
                .is_none()
        );
        assert!(parse_cli(["hizuke", "--dry-run", "apply", ".", "--yes"]).is_err());
    }
}
