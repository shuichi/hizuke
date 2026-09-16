# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_hizuke_global_optspecs
    string join \n json q/quiet v/verbose color= r/recursive timezone= duplicates= y/yes n/dry-run h/help V/version
end

function __fish_hizuke_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_hizuke_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_hizuke_using_subcommand
    set -l cmd (__fish_hizuke_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c hizuke -n "__fish_hizuke_needs_command" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_needs_command" -l timezone -d 'Camera wall time / local mtime by default; UTC needs a valid EXIF offset' -r -f -a "local\t'Preserve EXIF camera wall time; use the system local timezone for mtime'
utc\t'Convert EXIF with its matching offset to UTC; use UTC for mtime'"
complete -c hizuke -n "__fish_hizuke_needs_command" -l duplicates -d 'Exact duplicates: choose a keeper, retain all copies, or skip the group' -r -f -a "ask\t''
keep-all\t''
skip\t''"
complete -c hizuke -n "__fish_hizuke_needs_command" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_needs_command" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_needs_command" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_needs_command" -s r -l recursive -d 'Include subdirectories, excluding links and separately managed libraries'
complete -c hizuke -n "__fish_hizuke_needs_command" -s y -l yes -d 'Apply without final confirmation; never answers duplicate questions'
complete -c hizuke -n "__fish_hizuke_needs_command" -s n -l dry-run -d 'Preview only; always takes precedence over --yes'
complete -c hizuke -n "__fish_hizuke_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c hizuke -n "__fish_hizuke_needs_command" -s V -l version -d 'Print version'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "preview" -d 'Show a read-only plan, without questions or filesystem writes'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "plan" -d 'Show a read-only plan, without questions or filesystem writes'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "apply" -d 'Review, ask about duplicates, and apply recoverable changes'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "rename" -d 'Review, ask about duplicates, and apply recoverable changes'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "undo" -d 'Preview and restore original names from a completed transaction'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "recover" -d 'Preview and restore original names after an interrupted operation'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "history" -d 'Read transaction history; --transaction shows the exact file mapping'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "completions" -d 'Generate shell completions to standard output'
complete -c hizuke -n "__fish_hizuke_needs_command" -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -l timezone -d 'Camera wall time / local mtime by default; UTC needs a valid EXIF offset' -r -f -a "local\t'Preserve EXIF camera wall time; use the system local timezone for mtime'
utc\t'Convert EXIF with its matching offset to UTC; use UTC for mtime'"
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -l duplicates -d 'Exact duplicates: choose a keeper, retain all copies, or skip the group' -r -f -a "ask\t''
keep-all\t''
skip\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -s r -l recursive -d 'Include subdirectories, excluding links and separately managed libraries'
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand preview" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -l timezone -d 'Camera wall time / local mtime by default; UTC needs a valid EXIF offset' -r -f -a "local\t'Preserve EXIF camera wall time; use the system local timezone for mtime'
utc\t'Convert EXIF with its matching offset to UTC; use UTC for mtime'"
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -l duplicates -d 'Exact duplicates: choose a keeper, retain all copies, or skip the group' -r -f -a "ask\t''
keep-all\t''
skip\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -s r -l recursive -d 'Include subdirectories, excluding links and separately managed libraries'
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand plan" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -l timezone -d 'Camera wall time / local mtime by default; UTC needs a valid EXIF offset' -r -f -a "local\t'Preserve EXIF camera wall time; use the system local timezone for mtime'
utc\t'Convert EXIF with its matching offset to UTC; use UTC for mtime'"
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -l duplicates -d 'Exact duplicates: choose a keeper, retain all copies, or skip the group' -r -f -a "ask\t''
keep-all\t''
skip\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -s r -l recursive -d 'Include subdirectories, excluding links and separately managed libraries'
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -s y -l yes -d 'Apply without final confirmation; never answers duplicate questions'
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -s n -l dry-run -d 'Preview only; always takes precedence over --yes'
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand apply" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -l timezone -d 'Camera wall time / local mtime by default; UTC needs a valid EXIF offset' -r -f -a "local\t'Preserve EXIF camera wall time; use the system local timezone for mtime'
utc\t'Convert EXIF with its matching offset to UTC; use UTC for mtime'"
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -l duplicates -d 'Exact duplicates: choose a keeper, retain all copies, or skip the group' -r -f -a "ask\t''
keep-all\t''
skip\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -s r -l recursive -d 'Include subdirectories, excluding links and separately managed libraries'
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -s y -l yes -d 'Apply without final confirmation; never answers duplicate questions'
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -s n -l dry-run -d 'Preview only; always takes precedence over --yes'
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand rename" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -l transaction -d 'Transaction ID; defaults to latest eligible transaction' -r
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -s y -l yes -d 'Skip final confirmation'
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -s n -l dry-run -d 'Read-only preview of the restoration'
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand undo" -s h -l help -d 'Print help'
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -l transaction -d 'Transaction ID; defaults to latest eligible transaction' -r
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -s y -l yes -d 'Skip final confirmation'
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -s n -l dry-run -d 'Read-only preview of the restoration'
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand recover" -s h -l help -d 'Print help'
complete -c hizuke -n "__fish_hizuke_using_subcommand history" -l transaction -r
complete -c hizuke -n "__fish_hizuke_using_subcommand history" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand history" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand history" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand history" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand history" -s h -l help -d 'Print help'
complete -c hizuke -n "__fish_hizuke_using_subcommand completions" -l color -d 'Color policy; auto respects NO_COLOR and terminal detection' -r -f -a "auto\t''
always\t''
never\t''"
complete -c hizuke -n "__fish_hizuke_using_subcommand completions" -l json -d 'Emit JSON; suppress progress and color'
complete -c hizuke -n "__fish_hizuke_using_subcommand completions" -s q -l quiet -d 'Suppress routine output (questions and errors remain visible)'
complete -c hizuke -n "__fish_hizuke_using_subcommand completions" -s v -l verbose -d 'Also show unchanged files and full diagnostic details'
complete -c hizuke -n "__fish_hizuke_using_subcommand completions" -s h -l help -d 'Print help'
complete -c hizuke -n "__fish_hizuke_using_subcommand help; and not __fish_seen_subcommand_from preview apply undo recover history completions help" -f -a "preview" -d 'Show a read-only plan, without questions or filesystem writes'
complete -c hizuke -n "__fish_hizuke_using_subcommand help; and not __fish_seen_subcommand_from preview apply undo recover history completions help" -f -a "apply" -d 'Review, ask about duplicates, and apply recoverable changes'
complete -c hizuke -n "__fish_hizuke_using_subcommand help; and not __fish_seen_subcommand_from preview apply undo recover history completions help" -f -a "undo" -d 'Preview and restore original names from a completed transaction'
complete -c hizuke -n "__fish_hizuke_using_subcommand help; and not __fish_seen_subcommand_from preview apply undo recover history completions help" -f -a "recover" -d 'Preview and restore original names after an interrupted operation'
complete -c hizuke -n "__fish_hizuke_using_subcommand help; and not __fish_seen_subcommand_from preview apply undo recover history completions help" -f -a "history" -d 'Read transaction history; --transaction shows the exact file mapping'
complete -c hizuke -n "__fish_hizuke_using_subcommand help; and not __fish_seen_subcommand_from preview apply undo recover history completions help" -f -a "completions" -d 'Generate shell completions to standard output'
complete -c hizuke -n "__fish_hizuke_using_subcommand help; and not __fish_seen_subcommand_from preview apply undo recover history completions help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
