use crate::command_safety::is_safe_command::is_safe_git_command;

/// Validates that tokenized PowerShell words stay within the read-only safelist.
pub(crate) fn is_safe_powershell_words(words: &[String]) -> bool {
    if words.is_empty() {
        return false;
    }

    for word in words {
        let inner = word
            .trim_matches(|character| character == '(' || character == ')')
            .trim_start_matches('-')
            .to_ascii_lowercase();
        if matches!(
            inner.as_str(),
            "set-content"
                | "add-content"
                | "out-file"
                | "new-item"
                | "remove-item"
                | "move-item"
                | "copy-item"
                | "rename-item"
                | "start-process"
                | "stop-process"
        ) {
            return false;
        }
    }

    let command = words[0]
        .trim_matches(|character| character == '(' || character == ')')
        .trim_start_matches('-')
        .to_ascii_lowercase();
    match command.as_str() {
        "echo" | "write-output" | "write-host" | "dir" | "ls" | "get-childitem" | "gci" | "cat"
        | "type" | "gc" | "get-content" | "select-string" | "sls" | "findstr"
        | "measure-object" | "measure" | "get-location" | "gl" | "pwd" | "test-path" | "tp"
        | "resolve-path" | "rvpa" | "select-object" | "select" | "get-item" => true,
        "git" => is_safe_git_command(words),
        "rg" => is_safe_ripgrep(words),
        "set-content" | "add-content" | "out-file" | "new-item" | "remove-item" | "move-item"
        | "copy-item" | "rename-item" | "start-process" | "stop-process" => false,
        _ => false,
    }
}

fn is_safe_ripgrep(words: &[String]) -> bool {
    const UNSAFE_OPTIONS_WITH_ARGS: &[&str] = &["--pre", "--hostname-bin"];
    const UNSAFE_OPTIONS_WITHOUT_ARGS: &[&str] = &["--search-zip", "-z"];

    !words.iter().skip(1).any(|arg| {
        let arg = arg.to_ascii_lowercase();
        UNSAFE_OPTIONS_WITHOUT_ARGS.contains(&arg.as_str())
            || UNSAFE_OPTIONS_WITH_ARGS
                .iter()
                .any(|option| arg == *option || arg.starts_with(&format!("{option}=")))
    })
}
