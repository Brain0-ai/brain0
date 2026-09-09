//! Best-effort extraction of the files a shell command **reads** into its output.
//!
//! Agents read most files through a shell (`cat .env`, `sed -n 1,40p x.rs`, `grep -rn key src/`,
//! `source .env`), not only through a dedicated `Read` tool. Those reads reach the model's
//! context exactly like a `Read` does, so leaving them out of the read set would make the DLP
//! audit blind to the most common way a secret is loaded. This module parses a command line
//! (pipes, `&&`, `;`, `$(…)`, quotes, redirections, `cd` tracking) and returns the paths passed
//! to known *reader* commands. It is deliberately conservative: unresolvable tokens (globs,
//! `$VAR`, `{}`) are dropped rather than guessed, and non-reader commands contribute nothing.
//!
//! The result is a **lower bound** of what the command read, never an upper bound: a reader
//! brain0 does not know, a script, or a remote/container command (`ssh`, `docker exec`) are
//! not followed.

use std::collections::BTreeSet;

/// Files a shell command reads, deduplicated and sorted. Relative paths are relative to the
/// command's working directory (adjusted for a leading `cd` inside the same command line);
/// `~` and `$HOME` are expanded against the current user's home directory.
#[must_use]
pub fn read_paths_from_command(cmd: &str) -> Vec<String> {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
    read_paths_from_command_with_home(cmd, home.as_deref())
}

/// [`read_paths_from_command`] with an explicit home directory (deterministic for tests).
#[must_use]
pub fn read_paths_from_command_with_home(cmd: &str, home: Option<&str>) -> Vec<String> {
    let mut out = BTreeSet::new();
    let mut base: Option<String> = None;
    for segment in segments(cmd) {
        scan_segment(&segment, home, &mut base, &mut out);
    }
    out.into_iter().collect()
}

/// Split a command line into simple commands (each a list of words), honoring quotes.
/// Separators: `|`, `||`, `&`, `&&`, `;`, newlines, `(`, `)`, backticks and `$(`.
fn segments(cmd: &str) -> Vec<Vec<String>> {
    let text = cmd.replace("\\\n", " ");
    let chars: Vec<char> = text.chars().collect();
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut in_single = false;
    let mut in_double = false;
    // Open `$(` substitutions: each entry records whether it was opened inside double quotes,
    // so the matching `)` restores that quoting state (the inner command is parsed unquoted).
    let mut subst_stack: Vec<bool> = Vec::new();
    // Heredoc tags seen on the current line (`<<EOF`, `<<-EOF`, `<<'EOF'`): their bodies start
    // on the next line and are data, not commands (`cat > f <<'EOF' … EOF` writes a file whose
    // text may look exactly like shell). Bodies are skipped up to the terminator line.
    let mut pending_heredocs: Vec<String> = Vec::new();

    let mut i = 0;
    let end_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    let end_segment = |word: &mut String,
                       in_word: &mut bool,
                       words: &mut Vec<String>,
                       segs: &mut Vec<Vec<String>>,
                       pending: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
        note_heredoc_tag(words, pending);
        if !words.is_empty() {
            segs.push(std::mem::take(words));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                word.push(c);
            }
            i += 1;
            continue;
        }
        if c == '\\' && i + 1 < chars.len() {
            word.push(chars[i + 1]);
            in_word = true;
            i += 2;
            continue;
        }
        if in_double {
            match c {
                '"' => in_double = false,
                '$' if chars.get(i + 1) == Some(&'(') => {
                    end_segment(
                        &mut word,
                        &mut in_word,
                        &mut words,
                        &mut segments,
                        &mut pending_heredocs,
                    );
                    subst_stack.push(true);
                    in_double = false;
                    i += 1;
                }
                _ => {
                    word.push(c);
                    in_word = true;
                }
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                in_word = true;
            }
            '"' => {
                in_double = true;
                in_word = true;
            }
            '\n' => {
                end_segment(
                    &mut word,
                    &mut in_word,
                    &mut words,
                    &mut segments,
                    &mut pending_heredocs,
                );
                // Skip heredoc bodies: everything up to (and including) each terminator line.
                for tag in pending_heredocs.drain(..) {
                    i += 1;
                    let mut line = String::new();
                    loop {
                        match chars.get(i) {
                            None => break,
                            Some('\n') => {
                                if line.trim_start_matches('\t') == tag {
                                    break;
                                }
                                line.clear();
                            }
                            Some(&ch) => line.push(ch),
                        }
                        i += 1;
                    }
                    if chars.get(i).is_none() {
                        break;
                    }
                }
            }
            '|' | ';' | '&' | '(' | '`' => {
                // `2>&1` / `>&2` / `<&0`: the `&` belongs to the redirection, not a separator.
                if c == '&' && (word.ends_with('>') || word.ends_with('<')) {
                    word.push(c);
                    i += 1;
                    continue;
                }
                end_segment(
                    &mut word,
                    &mut in_word,
                    &mut words,
                    &mut segments,
                    &mut pending_heredocs,
                );
            }
            ')' => {
                end_segment(
                    &mut word,
                    &mut in_word,
                    &mut words,
                    &mut segments,
                    &mut pending_heredocs,
                );
                if subst_stack.pop() == Some(true) {
                    in_double = true;
                }
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                end_segment(
                    &mut word,
                    &mut in_word,
                    &mut words,
                    &mut segments,
                    &mut pending_heredocs,
                );
                subst_stack.push(false);
                i += 1;
            }
            '<' | '>' => {
                // Redirection operators become their own words (`<`, `<<`, `<<<`, `>`, `>>`,
                // `2>`, `&>`). A digit or `&` glued in front stays attached.
                let glued = word == "2" || word == "1" || word == "&";
                if !glued {
                    end_word(&mut word, &mut in_word, &mut words);
                }
                word.push(c);
                in_word = true;
                while chars.get(i + 1) == Some(&c) {
                    word.push(c);
                    i += 1;
                }
                if word == "<<" && chars.get(i + 1) == Some(&'-') {
                    word.push('-');
                    i += 1;
                }
                if chars.get(i + 1) == Some(&'&') {
                    word.push('&');
                    i += 1;
                    if let Some(&d) = chars.get(i + 1) {
                        if d.is_ascii_digit() || d == '-' {
                            word.push(d);
                            i += 1;
                        }
                    }
                }
                end_word(&mut word, &mut in_word, &mut words);
            }
            c if c.is_whitespace() => {
                end_word(&mut word, &mut in_word, &mut words);
                note_heredoc_tag(&words, &mut pending_heredocs);
            }
            _ => {
                word.push(c);
                in_word = true;
            }
        }
        i += 1;
    }
    end_segment(
        &mut word,
        &mut in_word,
        &mut words,
        &mut segments,
        &mut pending_heredocs,
    );
    segments
}

/// If the last two words are `<<`/`<<-` and a tag, remember the tag (quotes already stripped).
fn note_heredoc_tag(words: &[String], pending: &mut Vec<String>) {
    if let [.., op, tag] = words {
        if (op == "<<" || op == "<<-") && !pending.last().is_some_and(|t| t == tag) {
            pending.push(tag.clone());
        }
    }
}

fn is_redirection(word: &str) -> bool {
    let w = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
    w.starts_with('<') || w.starts_with('>')
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Wrapper commands that run the *next* word as the real command.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "time", "nice", "ionice", "command", "exec", "builtin", "env", "nohup",
    "stdbuf", "timeout",
];

/// How a reader command takes its file operands.
#[derive(Clone, Copy)]
enum Kind {
    /// Every positional operand is a file (`cat a b`).
    AllFiles,
    /// The first positional is a script/pattern unless one of `script_opts` supplied it
    /// (`sed 's/x/y/' file`, `grep -e pat file`, `awk '{print}' file`, `jq '.a' file`).
    ScriptThenFiles(&'static [&'static str]),
}

fn kind_of(cmd: &str) -> Option<Kind> {
    use Kind::{AllFiles, ScriptThenFiles};
    Some(match cmd {
        "cat" | "tac" | "head" | "tail" | "less" | "more" | "nl" | "strings" | "xxd"
        | "hexdump" | "hd" | "od" | "base64" | "bat" | "batcat" | "diff" | "cmp" | "comm"
        | "paste" | "join" | "source" | "." | "cut" | "sort" | "uniq" | "column" | "fold"
        | "rev" | "shuf" | "expand" | "unexpand" => AllFiles,
        "sed" => ScriptThenFiles(&["-e", "-f", "--expression", "--file"]),
        "awk" | "gawk" | "mawk" | "nawk" => ScriptThenFiles(&["-f"]),
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" | "ugrep" => {
            ScriptThenFiles(&["-e", "-f", "--regexp", "--file"])
        }
        "jq" => ScriptThenFiles(&["-f", "--from-file"]),
        _ => return None,
    })
}

/// How many following words an option consumes for `cmd` (0 = a plain flag).
fn takes_args(cmd: &str, opt: &str) -> usize {
    if opt.starts_with("--") && opt.contains('=') {
        return 0;
    }
    let one: &[&str] = match cmd {
        "head" | "tail" => &["-n", "-c", "--lines", "--bytes"],
        "strings" => &["-n", "-t", "--bytes"],
        "xxd" => &["-l", "-s", "-c", "-g", "-o"],
        "od" => &["-j", "-N", "-t", "-w", "-A", "-S"],
        "bat" | "batcat" => &[
            "-r",
            "-l",
            "-H",
            "--line-range",
            "--language",
            "--highlight-line",
            "--style",
            "--theme",
            "--map-syntax",
        ],
        "diff" => &[
            "-U",
            "-C",
            "-I",
            "-x",
            "-X",
            "-S",
            "--unified",
            "--context",
            "--exclude",
        ],
        "sed" => &["-e", "-f", "-l", "--expression", "--file", "--line-length"],
        "awk" | "gawk" | "mawk" | "nawk" => &["-f", "-v", "-F", "-W"],
        "grep" | "egrep" | "fgrep" | "ugrep" => &[
            "-e",
            "-f",
            "-m",
            "-A",
            "-B",
            "-C",
            "-d",
            "-D",
            "--regexp",
            "--file",
            "--include",
            "--exclude",
            "--exclude-dir",
            "--exclude-from",
            "--max-count",
            "--context",
            "--color",
            "--colour",
            "--label",
            "--binary-files",
            "--directories",
            "--devices",
        ],
        "rg" => &[
            "-e",
            "-f",
            "-m",
            "-A",
            "-B",
            "-C",
            "-g",
            "-t",
            "-T",
            "-j",
            "-E",
            "-M",
            "-r",
            "-d",
            "--regexp",
            "--file",
            "--glob",
            "--iglob",
            "--type",
            "--type-not",
            "--type-add",
            "--max-count",
            "--max-depth",
            "--context",
            "--before-context",
            "--after-context",
            "--encoding",
            "--pre",
            "--pre-glob",
            "--path-separator",
            "--threads",
            "--replace",
            "--sort",
            "--sortr",
            "--color",
            "--colors",
            "--max-filesize",
            "--max-columns",
            "--dfa-size-limit",
            "--regex-size-limit",
            "--engine",
            "--ignore-file",
            "--context-separator",
            "--field-context-separator",
            "--field-match-separator",
        ],
        "ag" | "ack" => &[
            "-A",
            "-B",
            "-C",
            "-G",
            "-m",
            "--ignore",
            "--ignore-dir",
            "--pager",
        ],
        "jq" => &["-f", "-L", "--from-file", "--indent"],
        "cut" => &[
            "-d",
            "-f",
            "-c",
            "-b",
            "--delimiter",
            "--fields",
            "--characters",
            "--bytes",
        ],
        "sort" => &[
            "-k",
            "-t",
            "-o",
            "-S",
            "-T",
            "--key",
            "--field-separator",
            "--output",
            "--buffer-size",
        ],
        "uniq" => &[
            "-f",
            "-s",
            "-w",
            "--skip-fields",
            "--skip-chars",
            "--check-chars",
        ],
        "fold" => &["-w", "--width"],
        "shuf" => &["-n", "-o", "--head-count", "--output", "--random-source"],
        "comm" | "join" | "paste" => &["--output-delimiter", "-d", "-j", "-t", "-1", "-2", "-o"],
        _ => &[],
    };
    if one.contains(&opt) {
        return 1;
    }
    let two: &[&str] = match cmd {
        "jq" => &["--arg", "--argjson", "--slurpfile", "--rawfile"],
        _ => &[],
    };
    usize::from(two.contains(&opt)) * 2
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Expand `~`, `~/…`, `$HOME`, `${HOME}`; leave anything else untouched.
fn expand_home(word: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return word.to_owned();
    };
    if word == "~" {
        return home.to_owned();
    }
    if let Some(rest) = word.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if let Some(rest) = word.strip_prefix("${HOME}") {
        return format!("{home}{rest}");
    }
    if let Some(rest) = word.strip_prefix("$HOME") {
        if rest.is_empty() || rest.starts_with('/') {
            return format!("{home}{rest}");
        }
    }
    word.to_owned()
}

/// Reject operands that are not a resolvable path: stdin, option-like, globs, unexpanded
/// variables, `find -exec {}`, pager `+N` positions.
fn usable_path(word: &str) -> bool {
    !(word.is_empty()
        || word == "-"
        || word == "--"
        || word.starts_with('-')
        || word.starts_with('+')
        || word.contains(['*', '?', '[', '{', '}', '$', '`']))
}

fn join_base(base: Option<&str>, path: &str) -> String {
    let path = path.strip_prefix("./").unwrap_or(path);
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    match base {
        Some(b) if !path.starts_with('/') && !is_windows_abs(path) => {
            if path == "." {
                b.to_owned()
            } else {
                format!("{}/{}", b.trim_end_matches('/'), path)
            }
        }
        _ => path.to_owned(),
    }
}

fn is_windows_abs(path: &str) -> bool {
    path.as_bytes().get(1) == Some(&b':') && path.len() > 2
}

fn scan_segment(
    words: &[String],
    home: Option<&str>,
    base: &mut Option<String>,
    out: &mut BTreeSet<String>,
) {
    // Redirections first: `< file` is a read wherever it appears; `> file`, heredocs and
    // here-strings are not files being read.
    let mut plain: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        if is_redirection(w) {
            let op = w.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
            if op == "<" {
                if let Some(target) = words.get(i + 1) {
                    push_path(target, home, base.as_deref(), out);
                }
            }
            // Every redirection consumes its operand (`<file` glued forms never occur: the
            // tokenizer always separates the operator), except fd duplications like `2>&1`.
            let has_operand = !w.contains('&') || w.ends_with('&');
            i += if has_operand { 2 } else { 1 };
            continue;
        }
        plain.push(w.clone());
        i += 1;
    }

    // Leading env assignments and wrappers (`FOO=1 sudo env BAR=2 cat x`).
    let mut start = 0;
    loop {
        while start < plain.len() && is_env_assignment(&plain[start]) {
            start += 1;
        }
        if start < plain.len() && WRAPPERS.contains(&basename(&plain[start])) {
            start += 1;
            // The wrapper's own flags (`sudo -n`, `env -i`, `timeout 5`).
            while start < plain.len()
                && (plain[start].starts_with('-')
                    || plain[start].chars().all(|c| c.is_ascii_digit()))
            {
                start += 1;
            }
            continue;
        }
        break;
    }
    let Some(first) = plain.get(start) else {
        return;
    };
    let mut cmd = basename(first);
    let mut args: &[String] = &plain[start + 1..];

    // `cd` changes what later relative operands in the same command line mean.
    if cmd == "cd" {
        match args.first().map(String::as_str) {
            None | Some("-") => {}
            Some(dir) => {
                let dir = expand_home(dir, home);
                if usable_path(&dir) {
                    *base = Some(join_base(base.as_deref(), &dir));
                }
            }
        }
        return;
    }

    // `git show rev:path` / `git cat-file -p rev:path` read from history; `git grep` from the
    // tracked tree — both put file content in front of the model.
    if cmd == "git" {
        match args.first().map(String::as_str) {
            Some("show" | "cat-file") => {
                for a in &args[1..] {
                    if a.starts_with('-') {
                        continue;
                    }
                    if let Some((_, path)) = a.split_once(':') {
                        push_path(path, home, base.as_deref(), out);
                    }
                }
                return;
            }
            Some("grep") => {
                cmd = "grep";
                args = &args[1..];
            }
            _ => return,
        }
    }

    let Some(kind) = kind_of(cmd) else {
        return;
    };
    // In-place sed is an edit (nothing reaches the model); jq `--args` turns positionals into
    // values, not files.
    if cmd == "sed"
        && args.iter().any(|a| {
            a == "-i" || a.starts_with("-i") && !a.starts_with("-i-") || a.starts_with("--in-place")
        })
    {
        return;
    }
    if cmd == "jq" && args.iter().any(|a| a == "--args" || a == "--jsonargs") {
        return;
    }

    let script_opts: &[&str] = match kind {
        Kind::AllFiles => &[],
        Kind::ScriptThenFiles(opts) => opts,
    };
    let mut positionals: Vec<&str> = Vec::new();
    let mut script_given = false;
    let mut opts_done = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if !opts_done && a == "--" {
            opts_done = true;
            i += 1;
            continue;
        }
        if !opts_done && a.starts_with('-') && a.len() > 1 {
            let key = a.split_once('=').map_or(a, |(k, _)| k);
            if script_opts.contains(&key) {
                script_given = true;
            }
            i += 1 + takes_args(cmd, a);
            continue;
        }
        positionals.push(a);
        i += 1;
    }
    let files: &[&str] = match kind {
        Kind::AllFiles => &positionals,
        Kind::ScriptThenFiles(_) if script_given => &positionals,
        Kind::ScriptThenFiles(_) => positionals.get(1..).unwrap_or(&[]),
    };
    for f in files {
        push_path(f, home, base.as_deref(), out);
    }
}

fn push_path(word: &str, home: Option<&str>, base: Option<&str>, out: &mut BTreeSet<String>) {
    let expanded = expand_home(word, home);
    if !usable_path(&expanded) {
        return;
    }
    let path = join_base(base, &expanded);
    if !path.is_empty() {
        out.insert(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads(cmd: &str) -> Vec<String> {
        read_paths_from_command_with_home(cmd, Some("/home/dev"))
    }

    #[test]
    fn plain_readers_take_every_operand() {
        assert_eq!(reads("cat .env"), vec![".env"]);
        assert_eq!(reads("cat -n a.txt b.txt"), vec!["a.txt", "b.txt"]);
        assert_eq!(reads("head -n 20 src/main.rs"), vec!["src/main.rs"]);
        assert_eq!(reads("tail -n +5 -c 100 log.txt"), vec!["log.txt"]);
        assert_eq!(reads("head -20 x"), vec!["x"]);
        assert_eq!(reads("less +F server.log"), vec!["server.log"]);
        assert_eq!(reads("diff -U 3 a b"), vec!["a", "b"]);
    }

    #[test]
    fn sed_scripts_are_not_files_and_in_place_is_not_a_read() {
        assert_eq!(reads("sed -n 40,116p crates/x.rs"), vec!["crates/x.rs"]);
        assert_eq!(reads("sed -n '1,50p' file"), vec!["file"]);
        assert_eq!(reads("sed -e 's/a/b/' -e 's/c/d/' file"), vec!["file"]);
        assert_eq!(reads("sed --expression='s/a/b/' file"), vec!["file"]);
        assert!(reads("sed -i 's/a/b/' file").is_empty());
        assert!(reads("sed -i.bak 's/a/b/' file").is_empty());
        assert!(reads("sed --in-place 's/a/b/' file").is_empty());
    }

    #[test]
    fn grep_family_skips_the_pattern_and_keeps_targets() {
        assert_eq!(reads("grep -rn \"api_key\" src/"), vec!["src"]);
        assert_eq!(reads("grep -e KEY -A 3 .env"), vec![".env"]);
        assert_eq!(
            reads("rg --type rust -g '*.rs' secret crates"),
            vec!["crates"]
        );
        assert_eq!(reads("grep -c token ."), vec!["."]);
        assert_eq!(reads("git grep AKIA -- config/"), vec!["config"]);
        // Combined short flags are flags, not arg-consuming options.
        assert_eq!(reads("grep -rni foo lib"), vec!["lib"]);
    }

    #[test]
    fn awk_and_jq_treat_the_first_positional_as_program() {
        assert_eq!(
            reads("awk -F: '{print $1}' /etc/passwd"),
            vec!["/etc/passwd"]
        );
        assert_eq!(reads("awk -f prog.awk data.txt"), vec!["data.txt"]);
        assert_eq!(reads("jq -r '.token' creds.json"), vec!["creds.json"]);
        assert_eq!(reads("jq --arg k v '.a' f.json"), vec!["f.json"]);
        assert!(reads("jq -n --args '$ARGS' a b").is_empty());
    }

    #[test]
    fn sourcing_and_input_redirection_are_reads() {
        assert_eq!(reads("source .env"), vec![".env"]);
        assert_eq!(reads("set -a; . ./.env; set +a"), vec![".env"]);
        assert_eq!(reads("python3 tool.py < input.json"), vec!["input.json"]);
        assert_eq!(
            reads("while read l; do echo $l; done < secrets/list"),
            vec!["secrets/list"]
        );
        assert!(reads("cat <<'EOF' > out.txt\nhello\nEOF").is_empty());
        // Heredoc bodies are data: shell-looking text inside them is not executed.
        assert!(reads("cat > run.sh <<'EOF'\ncat .env\nsource ~/.aws/credentials\nEOF").is_empty());
        assert_eq!(
            reads("cat > run.sh <<'EOF'\ncat .env\nEOF\ncat after.txt"),
            vec!["after.txt"]
        );
        assert_eq!(
            reads("python3 - <<EOF\nprint(open('.env').read())\nEOF\nhead -1 real.txt"),
            vec!["real.txt"]
        );
        assert!(reads("cat <<-EOT\n\tcat key.pem\n\tEOT").is_empty());
        assert!(reads("cat <<'A' <<'B'\ncat a\nA\ncat b\nB").is_empty());
        // An unterminated heredoc swallows the rest of the input rather than guessing.
        assert!(reads("cat <<'EOF'\ncat .env").is_empty());
        assert!(reads("cat <<< \"$X\"").is_empty());
        assert!(reads("cmd 2>&1 >/dev/null").is_empty());
    }

    #[test]
    fn pipes_substitutions_and_wrappers_are_followed() {
        assert_eq!(reads("cat .env | grep -v '^#'"), vec![".env"]);
        assert_eq!(reads("export $(cat .env | xargs)"), vec![".env"]);
        assert_eq!(
            reads("echo \"$(cat ~/.aws/credentials)\""),
            vec!["/home/dev/.aws/credentials"]
        );
        assert_eq!(reads("KEY=`cat key.pem`"), vec!["key.pem"]);
        assert_eq!(
            reads("echo \"token: $(cat \"my key.pem\") end\" | tee out"),
            vec!["my key.pem"]
        );
        assert_eq!(reads("sudo cat /etc/shadow"), vec!["/etc/shadow"]);
        assert_eq!(reads("FOO=1 env BAR=2 cat x && ls"), vec!["x"]);
        assert_eq!(reads("/usr/bin/cat x"), vec!["x"]);
    }

    #[test]
    fn cd_rebases_later_relative_operands() {
        assert_eq!(reads("cd /tmp && cat x"), vec!["/tmp/x"]);
        assert_eq!(reads("cd sub; cat x"), vec!["sub/x"]);
        assert_eq!(
            reads("cd ~/.ssh && cat id_rsa"),
            vec!["/home/dev/.ssh/id_rsa"]
        );
        assert_eq!(reads("cat a; cd b; cat c"), vec!["a", "b/c"]);
    }

    #[test]
    fn home_and_variables() {
        assert_eq!(reads("cat ~/.netrc"), vec!["/home/dev/.netrc"]);
        assert_eq!(reads("cat $HOME/.npmrc"), vec!["/home/dev/.npmrc"]);
        assert_eq!(reads("cat ${HOME}/.pypirc"), vec!["/home/dev/.pypirc"]);
        // Unresolvable operands are dropped, never guessed.
        assert!(reads("cat $FILE").is_empty());
        assert!(reads("cat *.pem").is_empty());
        assert!(reads(r"find . -name '*.key' -exec cat {} \;").is_empty());
        assert_eq!(
            read_paths_from_command_with_home("cat ~/x", None),
            vec!["~/x"]
        );
    }

    #[test]
    fn quoting_keeps_paths_with_spaces_and_separators_intact() {
        assert_eq!(reads("cat 'my file.txt'"), vec!["my file.txt"]);
        assert_eq!(reads("cat \"a|b.txt\""), vec!["a|b.txt"]);
        assert_eq!(reads("cat my\\ file"), vec!["my file"]);
        assert_eq!(reads("grep 'a|b' notes.md"), vec!["notes.md"]);
    }

    #[test]
    fn non_readers_and_remote_commands_contribute_nothing() {
        assert!(reads("ls -la .env").is_empty());
        assert!(reads("cp .env backup").is_empty());
        assert!(reads("wc -l .env").is_empty());
        assert!(reads("ssh host cat .env").is_empty());
        assert!(reads("docker exec c cat /run/secrets/x").is_empty());
        assert!(reads("python3 - \"$f\" <<'EOF'\nprint(1)\nEOF").is_empty());
        assert!(reads("git status").is_empty());
        assert!(reads("").is_empty());
    }

    #[test]
    fn git_show_reads_a_path_from_history() {
        assert_eq!(reads("git show HEAD:.env"), vec![".env"]);
        assert_eq!(
            reads("git show HEAD~2:config/prod.pem"),
            vec!["config/prod.pem"]
        );
        assert_eq!(reads("git cat-file -p abc123:secrets/k"), vec!["secrets/k"]);
        assert!(reads("git show HEAD --stat").is_empty());
    }
}
