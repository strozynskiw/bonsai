pub(crate) fn extract_cd_target(command: &str) -> Option<String> {
    let command = command.trim();
    if let Some(target) = command.strip_prefix("cd ") {
        let target = target.trim();
        let target = target.split(';').next().unwrap_or(target);
        let target = target.split("&&").next().unwrap_or(target);
        let target = target.trim();
        if !target.is_empty() {
            return Some(target.to_string());
        }
    }
    None
}

/// Return the target and untouched remainder of a simple leading
/// `cd <target> && <command>` form.
///
/// Only a standalone `cd` with one target is accepted. This deliberately does
/// not try to normalize flags, redirects, `;`, or other shell shapes: callers
/// can then erase the directory change without changing the command's meaning.
pub(crate) fn leading_cd_and_remainder(command: &str) -> Option<(String, &str)> {
    let separator = top_level_and_separator(command)?;
    let cd_segment = &command[..separator];
    let remainder = command[separator + 2..].trim_start();
    if remainder.is_empty() {
        return None;
    }

    let tokens = tokenize_shell(cd_segment)?;
    match tokens.as_slice() {
        [program, target] if program == "cd" => Some((target.clone(), remainder)),
        _ => None,
    }
}

/// Find the first top-level `&&` shell operator without mistaking quoted or
/// command-substitution text for a separator.
fn top_level_and_separator(command: &str) -> Option<usize> {
    let bytes = command.as_bytes();
    let mut index = 0;
    let mut quote = None;
    let mut substitution_depth = 0usize;

    while let Some(&byte) = bytes.get(index) {
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
            }
            Some(b'"') => {
                if byte == b'\\' {
                    index += 1;
                } else if byte == b'"' {
                    quote = None;
                }
            }
            Some(_) => return None,
            None => match byte {
                b'\\' => index += 1,
                b'\'' | b'"' => quote = Some(byte),
                b'$' | b'<' | b'>' if bytes.get(index + 1) == Some(&b'(') => {
                    substitution_depth += 1;
                    index += 1;
                }
                b'(' if substitution_depth > 0 => substitution_depth += 1,
                b')' if substitution_depth > 0 => substitution_depth -= 1,
                b'&' if substitution_depth == 0 && bytes.get(index + 1) == Some(&b'&') => {
                    return Some(index);
                }
                _ => {}
            },
        }
        index += 1;
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandAnalysis {
    /// The whole command with leading safe env-assignments stripped. Used for
    /// display, persisting an allow-matching rule, and `cd` tracking.
    permission_command: String,
    /// Every command string that must clear the permission rules: the whole
    /// command plus each operator-delimited segment and each command-
    /// substitution body. Evaluated with `PermissionService::check_all` so a
    /// dangerous segment can't hide behind an allowed leading program.
    permission_commands: Vec<String>,
    tokens: Option<Vec<String>>,
}

impl CommandAnalysis {
    pub(crate) fn permission_command(&self) -> &str {
        &self.permission_command
    }

    pub(crate) fn permission_commands(&self) -> &[String] {
        &self.permission_commands
    }

    /// The program token (first word after any env-assignment prefix), or `None`
    /// when the command couldn't be tokenized.
    pub(crate) fn program(&self) -> Option<&str> {
        self.tokens
            .as_deref()
            .and_then(|tokens| tokens.first())
            .map(String::as_str)
    }
}

/// The single in-tree file a read-only `cat`/`head`/`tail` displayed, or `None`
/// when the command is not one of those, reads zero files, or reads several.
/// Context dedup uses this to collapse a repeated identical bash file read to a
/// pointer, the same way it does for the `read` tool. `grep` is deliberately
/// excluded: it is a search, not a file read.
pub(crate) fn single_read_path(command: &str, stdout: &str) -> Option<String> {
    let analysis = analyze_command(command);
    if !matches!(analysis.program(), Some("cat" | "head" | "tail")) {
        return None;
    }
    match extract_read_paths(&analysis, stdout).as_slice() {
        [operand] => Some(operand.path.clone()),
        _ => None,
    }
}

pub(crate) fn analyze_command(command: &str) -> CommandAnalysis {
    let tokens = tokenize_shell(command);
    let stripped_tokens = tokens.as_deref().and_then(strip_env_prefix);
    let permission_command = stripped_tokens
        .as_ref()
        .filter(|tokens| !tokens.is_empty())
        .map(|tokens| tokens.join(" "))
        .unwrap_or_else(|| command.trim().to_string());

    let mut permission_commands = Vec::new();
    push_unique(&mut permission_commands, permission_command.clone());
    collect_permission_commands(command, 0, &mut permission_commands);

    CommandAnalysis {
        permission_command,
        permission_commands,
        tokens: stripped_tokens,
    }
}

/// Bounds how deep we descend into nested command substitutions when gathering
/// permission strings. The bodies surfaced before the limit are still checked;
/// this only stops runaway recursion on pathological input.
const MAX_SUBSTITUTION_DEPTH: usize = 8;

/// Collect the permission-check strings for `command`: the env-stripped whole,
/// each operator-delimited segment, and — recursively — the body of every
/// command substitution it contains.
fn collect_permission_commands(command: &str, depth: usize, out: &mut Vec<String>) {
    let (outer, bodies) = extract_substitutions(command);

    if let Some(whole) = permission_string(&outer) {
        push_unique(out, whole);
    }
    for segment in split_command_segments(&outer) {
        push_unique(out, segment);
    }

    if depth < MAX_SUBSTITUTION_DEPTH {
        for body in bodies {
            collect_permission_commands(&body, depth + 1, out);
        }
    }
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !value.is_empty() && !out.contains(&value) {
        out.push(value);
    }
}

/// Env-stripped, re-joined form of a single command (no operator splitting).
/// Returns `None` for an empty/blank command.
fn permission_string(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    let joined = tokenize_shell(command)
        .and_then(|tokens| strip_env_prefix(&tokens))
        .filter(|tokens| !tokens.is_empty())
        .map(|tokens| tokens.join(" "))
        .unwrap_or_else(|| trimmed.to_string());
    Some(joined)
}

/// Split a command into its shell segments at the command separators `;`,
/// `&&`, `||`, `|`, `&` (redirections stay within their segment), env-stripping
/// each. Returns an empty vec when the command can't be tokenized (e.g.
/// unbalanced quotes) — the caller still holds the whole-command string.
fn split_command_segments(command: &str) -> Vec<String> {
    let Some(tokens) = tokenize_shell(command) else {
        return Vec::new();
    };
    let mut segments = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for token in tokens {
        // `2>&1` tokenizes as `2`, `>`, `&`, `1`. The ampersand is a file
        // descriptor duplication in that position, not a background-command
        // separator; retaining it avoids classifying the trailing descriptor as
        // an independent unknown executable.
        if token == "&"
            && current
                .last()
                .is_some_and(|previous| matches!(previous.as_str(), ">" | ">>" | ">|"))
        {
            current.push(token);
            continue;
        }
        if is_command_separator(&token) {
            finish_segment(&mut current, &mut segments);
        } else {
            current.push(token);
        }
    }
    finish_segment(&mut current, &mut segments);
    segments
}

fn is_command_separator(token: &str) -> bool {
    matches!(token, ";" | "&&" | "||" | "|" | "&")
}

fn finish_segment(current: &mut Vec<String>, segments: &mut Vec<String>) {
    if current.is_empty() {
        return;
    }
    let tokens = std::mem::take(current);
    let joined = strip_env_prefix(&tokens)
        .filter(|tokens| !tokens.is_empty())
        .unwrap_or(tokens)
        .join(" ");
    if !joined.is_empty() {
        segments.push(joined);
    }
}

/// Replace each command substitution (`$(...)`, `` `...` ``, `<(...)`, `>(...)`)
/// with a neutral placeholder token and return the placeholder-substituted
/// outer command together with the extracted substitution bodies. Single-quoted
/// spans are treated as literal text (no substitution), matching the shell.
fn extract_substitutions(command: &str) -> (String, Vec<String>) {
    let chars: Vec<char> = command.chars().collect();
    let mut outer = String::new();
    let mut bodies = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let c = chars[i];

        if c == '\\' && !in_single {
            outer.push(c);
            if let Some(&next) = chars.get(i + 1) {
                outer.push(next);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            outer.push(c);
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            outer.push(c);
            i += 1;
            continue;
        }

        if !in_single {
            let opens_paren_subst = matches!(c, '$' | '<' | '>') && chars.get(i + 1) == Some(&'(');
            if opens_paren_subst {
                let body_start = i + 2;
                let mut depth = 1usize;
                let mut j = body_start;
                while j < chars.len() {
                    match chars[j] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                bodies.push(chars[body_start..j.min(chars.len())].iter().collect());
                outer.push_str(" __subst__ ");
                i = j + 1;
                continue;
            }
            if c == '`'
                && let Some(rel) = chars[i + 1..].iter().position(|&x| x == '`')
            {
                let close = i + 1 + rel;
                bodies.push(chars[i + 1..close].iter().collect());
                outer.push_str(" __subst__ ");
                i = close + 1;
                continue;
            }
        }

        outer.push(c);
        i += 1;
    }

    (outer, bodies)
}

pub(super) fn tokenize_shell(command: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            '\\' if !in_single_quote => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ch if ch.is_whitespace() && !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            ch if is_control_char(ch) && !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                if ch == '>' && chars.peek().is_some_and(|next| *next == '|') {
                    let next = chars.next().unwrap_or('|');
                    tokens.push(format!("{ch}{next}"));
                } else if matches!(ch, '&' | '|' | '>')
                    && chars.peek().is_some_and(|next| *next == ch)
                {
                    let next = chars.next().unwrap_or(ch);
                    tokens.push(format!("{ch}{next}"));
                } else {
                    tokens.push(ch.to_string());
                }
            }
            _ => current.push(ch),
        }
    }

    if in_single_quote || in_double_quote {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    Some(tokens)
}

fn is_control_char(ch: char) -> bool {
    matches!(ch, '|' | '&' | ';' | '<' | '>')
}

fn strip_env_prefix(tokens: &[String]) -> Option<Vec<String>> {
    let mut index = 0;
    while let Some(token) = tokens.get(index).filter(|token| is_env_assignment(token)) {
        if !is_safe_env_assignment(token) {
            return None;
        }
        index += 1;
    }

    if tokens.get(index).is_some_and(|token| token == "env")
        && tokens
            .get(index + 1)
            .is_some_and(|token| is_env_assignment(token))
    {
        index += 1;
        while let Some(token) = tokens.get(index).filter(|token| is_env_assignment(token)) {
            if !is_safe_env_assignment(token) {
                return None;
            }
            index += 1;
        }
    }

    Some(tokens[index..].to_vec())
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_safe_env_assignment(token: &str) -> bool {
    let Some((_name, value)) = token.split_once('=') else {
        return false;
    };
    !env_assignment_value_may_execute(value)
}

fn env_assignment_value_may_execute(value: &str) -> bool {
    value.contains("$(") || value.contains('`') || value.contains("<(") || value.contains(">(")
}

/// A file a bash command read, tagged with whether the read covered the whole
/// file. `cat` shows the entire file (full); `head`/`tail`/`grep` show only a
/// window or the matching lines (partial), so a later whole-file `write` must
/// not treat them as if the model saw everything (P4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadOperand {
    pub(crate) path: String,
    pub(crate) full_coverage: bool,
}

pub(crate) fn extract_read_paths(analysis: &CommandAnalysis, stdout: &str) -> Vec<ReadOperand> {
    let Some(tokens) = analysis.tokens.as_deref() else {
        return Vec::new();
    };
    if tokens.iter().any(|token| is_control_operator(token)) {
        return Vec::new();
    }

    let Some(program) = tokens.first().map(String::as_str) else {
        return Vec::new();
    };

    let (paths, full_coverage) = match program {
        "cat" => (extract_simple_file_operands(&tokens[1..]), true),
        "head" | "tail" => (extract_head_tail_file_operands(&tokens[1..]), false),
        "grep" => (extract_grep_file_operands(&tokens[1..], stdout), false),
        _ => (Vec::new(), true),
    };
    paths
        .into_iter()
        .map(|path| ReadOperand {
            path,
            full_coverage,
        })
        .collect()
}

fn is_control_operator(token: &str) -> bool {
    matches!(
        token,
        "|" | "||" | "&&" | ";" | "<" | ">" | ">>" | ">|" | "&"
    )
}

fn extract_simple_file_operands(tokens: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut after_options = false;
    for token in tokens {
        if token == "--" {
            after_options = true;
            continue;
        }
        if !after_options && token.starts_with('-') {
            continue;
        }
        paths.push(token.clone());
    }
    paths
}

fn extract_head_tail_file_operands(tokens: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut after_options = false;
    let mut skip_next = false;

    for token in tokens {
        if skip_next {
            skip_next = false;
            continue;
        }
        if token == "--" {
            after_options = true;
            continue;
        }
        if !after_options {
            match token.as_str() {
                "-n" | "-c" | "--lines" | "--bytes" => {
                    skip_next = true;
                    continue;
                }
                _ if token.starts_with("--lines=") || token.starts_with("--bytes=") => continue,
                _ if token.starts_with('-') => continue,
                _ if token.starts_with('+') => continue,
                _ => {}
            }
        }
        paths.push(token.clone());
    }

    paths
}

fn extract_grep_file_operands(tokens: &[String], stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut after_options = false;
    let mut has_pattern_source = false;
    let mut pattern_consumed = false;
    let mut recursive = false;
    let mut index = 0;

    while index < tokens.len() {
        let token = &tokens[index];
        if !after_options {
            if token == "--" {
                after_options = true;
                index += 1;
                continue;
            }
            if token == "-R" || token == "-r" || token == "--recursive" {
                recursive = true;
                index += 1;
                continue;
            }
            if let Some(cluster) = token.strip_prefix('-')
                && !cluster.is_empty()
                && !cluster.starts_with('-')
                && grep_short_cluster_has_only_flag_options(cluster)
            {
                if cluster.contains('R') || cluster.contains('r') {
                    recursive = true;
                }
                index += 1;
                continue;
            }
            if token == "-f" || token == "--file" {
                if let Some(path) = tokens.get(index + 1) {
                    paths.push(path.clone());
                    has_pattern_source = true;
                    index += 2;
                    continue;
                }
                return Vec::new();
            }
            if let Some(path) = token.strip_prefix("-f").filter(|path| !path.is_empty()) {
                paths.push(path.to_string());
                has_pattern_source = true;
                index += 1;
                continue;
            }
            if let Some(path) = token.strip_prefix("--file=") {
                paths.push(path.to_string());
                has_pattern_source = true;
                index += 1;
                continue;
            }
            if token == "-e" || token == "--regexp" {
                if tokens.get(index + 1).is_none() {
                    return Vec::new();
                }
                has_pattern_source = true;
                index += 2;
                continue;
            }
            if let Some(_pattern) = token
                .strip_prefix("-e")
                .filter(|pattern| !pattern.is_empty())
            {
                has_pattern_source = true;
                index += 1;
                continue;
            }
            if token.starts_with("--regexp=") {
                has_pattern_source = true;
                index += 1;
                continue;
            }
            if grep_option_requires_arg(token) {
                if tokens.get(index + 1).is_none() {
                    return Vec::new();
                }
                index += 2;
                continue;
            }
            if token.starts_with('-') && token != "-" {
                index += 1;
                continue;
            }
        }

        if !has_pattern_source && !pattern_consumed {
            pattern_consumed = true;
            index += 1;
            continue;
        }

        paths.push(token.clone());
        index += 1;
    }

    if recursive {
        paths.extend(extract_grep_output_paths(stdout));
    }

    paths
}

fn grep_short_cluster_has_only_flag_options(cluster: &str) -> bool {
    !cluster.is_empty()
        && cluster.chars().all(|ch| {
            matches!(
                ch,
                'E' | 'F'
                    | 'G'
                    | 'P'
                    | 'R'
                    | 'r'
                    | 'I'
                    | 'i'
                    | 'v'
                    | 'w'
                    | 'x'
                    | 'c'
                    | 'l'
                    | 'L'
                    | 'n'
                    | 'H'
                    | 'h'
                    | 'o'
                    | 'q'
                    | 's'
                    | 'a'
                    | 'b'
                    | 'u'
                    | 'U'
                    | 'Z'
                    | 'z'
                    | 'y'
            )
        })
}

fn grep_option_requires_arg(token: &str) -> bool {
    matches!(
        token,
        "-m" | "--max-count"
            | "-A"
            | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "-D"
            | "--devices"
            | "-d"
            | "--directories"
            | "--include"
            | "--exclude"
            | "--exclude-dir"
            | "--label"
    )
}

fn extract_grep_output_paths(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in stdout.lines() {
        let candidate = line.split_once(':').map_or(line, |(head, _)| head).trim();
        if !candidate.is_empty() {
            paths.push(candidate.to_string());
        }
        let whole_line = line.trim();
        if !whole_line.is_empty() && whole_line != candidate {
            paths.push(whole_line.to_string());
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Permission, PermissionService};

    fn read_operands(command: &str) -> Vec<ReadOperand> {
        extract_read_paths(&analyze_command(command), "")
    }

    #[test]
    fn cat_marks_full_coverage_but_head_tail_grep_mark_partial() {
        // P4: `cat` shows the whole file (a later write is safe), while
        // head/tail/grep only expose a window or matching lines, so they must
        // not satisfy the whole-file write guard.
        assert_eq!(
            read_operands("cat src/main.rs"),
            vec![ReadOperand {
                path: "src/main.rs".to_string(),
                full_coverage: true,
            }]
        );
        for partial in ["head -5 src/main.rs", "tail -5 src/main.rs"] {
            assert_eq!(
                read_operands(partial),
                vec![ReadOperand {
                    path: "src/main.rs".to_string(),
                    full_coverage: false,
                }],
                "{partial} should be partial coverage"
            );
        }
        assert_eq!(
            read_operands("grep needle src/main.rs"),
            vec![ReadOperand {
                path: "src/main.rs".to_string(),
                full_coverage: false,
            }]
        );
    }

    #[test]
    fn test_extract_cd_target() {
        assert_eq!(extract_cd_target("cd /tmp"), Some("/tmp".to_string()));
        assert_eq!(extract_cd_target("cd /tmp && ls"), Some("/tmp".to_string()));
        assert_eq!(extract_cd_target("cd /tmp; ls"), Some("/tmp".to_string()));
        assert_eq!(extract_cd_target("ls"), None);
    }

    #[test]
    fn leading_cd_and_remainder_accepts_only_simple_and_chains() {
        assert_eq!(
            leading_cd_and_remainder("cd '/tmp/project dir' && cargo fmt --all"),
            Some(("/tmp/project dir".to_string(), "cargo fmt --all"))
        );
        assert_eq!(
            leading_cd_and_remainder("cd /tmp/project && printf 'a && b'"),
            Some(("/tmp/project".to_string(), "printf 'a && b'"))
        );
        assert_eq!(leading_cd_and_remainder("cd /tmp/project; cargo fmt"), None);
        assert_eq!(
            leading_cd_and_remainder("cd -P /tmp/project && cargo fmt"),
            None
        );
        assert_eq!(leading_cd_and_remainder("printf ok && cargo fmt"), None);
    }

    #[test]
    fn command_analysis_strips_leading_env_assignments_for_permission_checks() {
        let service = PermissionService::new();
        let cargo = analyze_command("FOO=1 BAR='two words' cargo test --locked");
        let dangerous = analyze_command("FOO=1 rm -rf /");
        let env_form = analyze_command("env FOO=1 cargo test --locked");
        let unsafe_assignment = analyze_command("X=$(touch${IFS}.pwned) cargo --version");
        let unsafe_env_form = analyze_command("env X=$(touch${IFS}.pwned) cargo --version");

        assert_eq!(cargo.permission_command(), "cargo test --locked");
        assert_eq!(env_form.permission_command(), "cargo test --locked");
        assert_eq!(
            unsafe_assignment.permission_command(),
            "X=$(touch${IFS}.pwned) cargo --version"
        );
        assert_eq!(
            unsafe_env_form.permission_command(),
            "env X=$(touch${IFS}.pwned) cargo --version"
        );
        // Build/test commands intentionally fall through to `Ask`; the active
        // autonomy ceiling, not a built-in Allow rule, decides whether their
        // Low risk can run without a modal.
        assert_eq!(service.check(cargo.permission_command()), Permission::Ask);
        assert_eq!(
            service.check(dangerous.permission_command()),
            Permission::Deny
        );
    }

    #[test]
    fn compound_and_substitution_commands_are_checked_per_segment() {
        let service = PermissionService::new();
        let check = |cmd: &str| service.check_all(analyze_command(cmd).permission_commands());

        // A denied segment can't hide behind an allowed leading program.
        assert_eq!(check("echo ok && rm -rf ~"), Permission::Deny);
        assert_eq!(check("ls; rm -rf ~"), Permission::Deny);
        // Command-substitution bodies are checked too.
        assert_eq!(check("echo $(rm -rf ~)"), Permission::Deny);
        assert_eq!(check("echo `rm -rf ~`"), Permission::Deny);
        // A single-quoted substitution is literal text, so the echo is allowed.
        assert_eq!(check("echo '$(rm -rf ~)'"), Permission::Allow);
        // A confirm-required segment downgrades an otherwise-allowed chain.
        assert_eq!(check("ls && rm file.txt"), Permission::Ask);
        // Plain allowed commands stay allowed.
        assert_eq!(check("ls -la"), Permission::Allow);
        assert_eq!(check("git status"), Permission::Allow);
    }

    #[test]
    fn permission_commands_expose_segments_and_substitution_bodies() {
        let chained = analyze_command("echo x && rm -rf ~");
        assert!(
            chained
                .permission_commands()
                .iter()
                .any(|c| c == "rm -rf ~")
        );

        let substituted = analyze_command("echo $(rm -rf ~)");
        assert!(
            substituted
                .permission_commands()
                .iter()
                .any(|c| c == "rm -rf ~")
        );
    }
}
