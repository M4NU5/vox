//! Rewrites text into the words a speech engine should say.
//!
//! Selected text is full of things a reader says one way and a speech engine
//! says another, or not at all: URLs, file paths, code identifiers, markdown,
//! numbers, dates, money, versions, units, abbreviations, symbols and emoji.
//! [`normalise`] spells them out as plain words before the text is chunked.
//! Acronyms are left alone: Kokoro already spells out words in capitals.
//!
//! It is rule-based and English-only (see [`applies`]). Every rule group
//! earned its place on the pronunciation eval in `tests/pronunciation`: each
//! [`Rule`] can be switched off there, and each one lowers Kokoro's word
//! error rate.
//!
//! Line breaks are kept, because the chunker uses them to find paragraphs.

use std::sync::OnceLock;

use regex::{Captures, Regex};

use crate::settings::{Provider, Settings};

/// A compiled regex, built once.
macro_rules! re {
    ($pattern:expr) => {{
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new($pattern).expect("valid regex"))
    }};
}

/// Groups of rules, in the order they run. The eval switches them off one at
/// a time to check each still earns its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Headings, lists, tables, emphasis, links, inline code and HTML tags.
    Markdown,
    /// Emoji and pictographs, which engines either skip or name.
    Emoji,
    /// URLs and email addresses.
    Urls,
    /// File paths and file names.
    Paths,
    /// e.g., i.e., etc., vs., Dr., and dotted initialisms like U.S.
    Abbreviations,
    /// Dates, clock times and years.
    Dates,
    /// Amounts of money.
    Money,
    /// Version numbers.
    Versions,
    /// Units after a number.
    Units,
    /// Every other number: ordinals, fractions, ranges, decimals.
    Numbers,
    /// Code identifiers and operators.
    Code,
    /// Symbols such as &, →, ±, ©.
    Symbols,
}

impl Rule {
    pub const ALL: [Rule; 12] = [
        Rule::Markdown,
        Rule::Emoji,
        Rule::Urls,
        Rule::Paths,
        Rule::Abbreviations,
        Rule::Dates,
        Rule::Money,
        Rule::Versions,
        Rule::Units,
        Rule::Numbers,
        Rule::Code,
        Rule::Symbols,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Rule::Markdown => "markdown",
            Rule::Emoji => "emoji",
            Rule::Urls => "urls",
            Rule::Paths => "paths",
            Rule::Abbreviations => "abbreviations",
            Rule::Dates => "dates",
            Rule::Money => "money",
            Rule::Versions => "versions",
            Rule::Units => "units",
            Rule::Numbers => "numbers",
            Rule::Code => "code",
            Rule::Symbols => "symbols",
        }
    }
}

/// Whether the reading should be normalised: the rules are for English, so
/// only English Kokoro and Piper voices get them. Cloud voices and the system
/// voice do their own text handling.
pub fn applies(settings: &Settings) -> bool {
    match settings.provider {
        Provider::Kokoro => matches!(settings.kokoro_voice.chars().next(), Some('a' | 'b')),
        Provider::Piper => settings.piper_voice.starts_with("en_"),
        _ => false,
    }
}

/// Rewrite `text` for reading aloud.
pub fn normalise(text: &str) -> String {
    normalise_with(text, &Rule::ALL)
}

/// [`normalise`] with only the rules in `rules`.
pub fn normalise_with(text: &str, rules: &[Rule]) -> String {
    let on = |rule| rules.contains(&rule);
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let raw: Vec<&str> = text.split('\n').collect();
    let mut lines: Vec<String> = Vec::new();
    for (i, line) in raw.iter().enumerate() {
        let mut s = line.to_string();
        if on(Rule::Markdown) {
            match markdown_line(&s, raw.get(i + 1).copied()) {
                Some(l) => s = l,
                // Keep the line break: it may be the paragraph's only one.
                None => {
                    lines.push(String::new());
                    continue;
                }
            }
            s = markdown_inline(&s);
        }
        if on(Rule::Emoji) {
            s = strip_emoji(&s);
        }
        if on(Rule::Urls) {
            s = urls(&s);
        }
        if on(Rule::Paths) {
            s = paths(&s);
        }
        if on(Rule::Abbreviations) {
            s = abbreviations(&s);
        }
        if on(Rule::Dates) {
            s = dates(&s);
        }
        if on(Rule::Money) {
            s = money(&s);
        }
        if on(Rule::Versions) {
            s = versions(&s);
        }
        if on(Rule::Units) {
            s = units(&s);
        }
        if on(Rule::Numbers) {
            s = numbers(&s);
        }
        if on(Rule::Code) {
            s = code(&s);
        }
        if on(Rule::Symbols) {
            s = symbols(&s);
        }
        lines.push(tidy(&s));
    }
    lines.join("\n")
}

// --- markdown ----------------------------------------------------------------

/// Line-level markdown. `None` blanks the line (fences, rules, table borders).
/// `next` is the line after, to tell whether a list item wraps onto it.
fn markdown_line(line: &str, next: Option<&str>) -> Option<String> {
    let t = line.trim();
    if t.starts_with("```") || t.starts_with("~~~") {
        return None;
    }
    if t.len() >= 3 && t.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) {
        return None;
    }
    // Table rows: cells become a list, separator rows vanish.
    if t.starts_with('|') && t.ends_with('|') && t.len() > 1 {
        let cells: Vec<&str> = t[1..t.len() - 1].split('|').map(str::trim).collect();
        if cells
            .iter()
            .all(|c| c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
        {
            return None;
        }
        return Some(end_sentence(&cells.join(", ")));
    }
    let mut s = t.to_string();
    let mut item = false;
    let (heading, bullet, task) = (
        re!(r"^#{1,6}\s+"),
        re!(r"^[-*+•◦▪‣·–]\s+"),
        re!(r"^\[[ xX]\]\s+"),
    );
    // Block quotes, headings, bullets and task boxes.
    loop {
        if let Some(rest) = s.strip_prefix('>') {
            s = rest.trim_start().to_string();
            continue;
        }
        if let Some(m) = heading.find(&s) {
            s = s[m.end()..].trim_end_matches(['#', ' ']).to_string();
            item = true;
            continue;
        }
        if let Some(m) = bullet.find(&s) {
            s = s[m.end()..].to_string();
            item = true;
            continue;
        }
        if let Some(m) = task.find(&s) {
            s = s[m.end()..].to_string();
            item = true;
            continue;
        }
        break;
    }
    if re!(r"^\d{1,3}[.)]\s+\S").is_match(&s) {
        item = true;
    }
    let wraps = next.is_some_and(|n| !starts_block(n));
    Some(if item && !wraps { end_sentence(&s) } else { s })
}

/// Whether a line starts a new block rather than continuing the one above.
fn starts_block(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty()
        || t.starts_with(['#', '>', '|', '`', '~'])
        || re!(r"^(?:[-*+•◦▪‣·–]|\d{1,3}[.)])\s").is_match(t)
}

/// Give a heading or list item a full stop, so it doesn't run on.
fn end_sentence(s: &str) -> String {
    let s = s.trim_end();
    match s.chars().last() {
        Some(c) if c.is_alphanumeric() || matches!(c, ')' | '"' | '\'' | '*' | '_' | '`') => {
            format!("{s}.")
        }
        _ => s.to_string(),
    }
}

/// Inline markdown and HTML: keep the words, drop the markup.
fn markdown_inline(s: &str) -> String {
    let s = re!(r"!\[([^\]]*)\]\([^)]*\)").replace_all(s, "$1");
    let s = re!(r"\[\^[^\]]*\]").replace_all(&s, "");
    let s = re!(r"\[([^\]]+)\]\([^)]*\)").replace_all(&s, "$1");
    let s = re!(r"<((?:https?://|mailto:)[^<>\s]+)>").replace_all(&s, "$1");
    let s = re!(r"(?i)<br\s*/?>").replace_all(&s, ", ");
    let s = re!(r"</?[A-Za-z][A-Za-z0-9-]*(?:\s[^<>]*)?/?>").replace_all(&s, "");
    let s = re!(r"`([^`]*)`").replace_all(&s, "$1");
    let s = re!(r"~~([^~]+)~~").replace_all(&s, "$1");
    // Emphasis: asterisks or underscores hugging a word, never a lone `*`
    // between spaces (multiplication) or an underscore inside snake_case.
    let s = re!(r"(^|[\s(\[\x22'])(\*{1,3}|_{1,3})([^\s*_])").replace_all(&s, "$1$3");
    let s = re!(r"(^|[\s(\[\x22'])(\*{1,3}|_{1,3})([^\s*_])").replace_all(&s, "$1$3");
    let s = re!(r"([^\s*_])(\*{1,3}|_{1,3})($|[\s).,;:!?\]\x22'])").replace_all(&s, "$1$3");
    let s = re!(r"([^\s*_])(\*{1,3}|_{1,3})($|[\s).,;:!?\]\x22'])").replace_all(&s, "$1$3");
    s.into_owned()
}

// --- emoji -------------------------------------------------------------------

fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F000..=0x1FAFF   // pictographs, emoticons, flags, skin tones
        | 0x2600..=0x27BF   // misc symbols and dingbats: ☕ ✅ ✓ ❤ ⚠
        | 0x2B00..=0x2BFF   // ⭐ and friends
        | 0xFE00..=0xFE0F   // variation selectors
        | 0x200D            // zero-width joiner
        | 0x20E3            // keycap
        | 0xE0020..=0xE007F // tag characters
    )
}

fn strip_emoji(s: &str) -> String {
    if !s.chars().any(is_emoji) {
        return s.to_string();
    }
    s.chars()
        .map(|c| if is_emoji(c) { ' ' } else { c })
        .collect()
}

// --- URLs and emails ---------------------------------------------------------

const TLDS: &str = "com|org|net|io|dev|co|uk|rs|ai|app|gov|edu|me|info|sh|so|gg|tv|xyz|us|de|fr|ca|au|nz|ie|in|eu|ly|to|cc|fm|page|site|tech|cloud";

fn urls(s: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    let url = URL.get_or_init(|| {
        Regex::new(&format!(
            r"(?i)\b(?:https?://|www\.)[^\s<>()\x22]+|\b[a-z0-9][a-z0-9-]*(?:\.[a-z0-9-]+)*\.(?:{TLDS})(?::\d+)?(?:/[^\s<>()\x22]*)?(?:[?#][^\s<>()\x22]*)?\b/?"
        ))
        .expect("valid regex")
    });
    let s = re!(r"\b([A-Za-z0-9._%+-]+)@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)\b")
        .replace_all(s, |c: &Captures| {
            format!("{} at {}", words_of(&c[1], " dot "), host(&c[2]))
        });
    url.replace_all(&s, |c: &Captures| {
        let raw = &c[0];
        let m = c.get(0).expect("whole match");
        let bare = !raw.contains("://") && !raw.to_ascii_lowercase().starts_with("www.");
        // Part of a path, or a file name like "build.sh": leave it to paths.
        let after_slash = s[..m.start()].ends_with(['/', '\\', '=']);
        let file_like = bare && !raw.contains('/') && is_file_name(raw);
        if after_slash || file_like {
            return raw.to_string();
        }
        // A URL doesn't end in sentence punctuation.
        let trimmed = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'']);
        let tail = &raw[trimmed.len()..];
        format!("{}{tail}", speak_url(trimmed))
    })
    .into_owned()
}

fn speak_url(url: &str) -> String {
    let rest = re!(r"(?i)^(?:https?://)?(?:www\.)?").replace(url, "");
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let mut parts = rest.split('/');
    let hostport = parts.next().unwrap_or("");
    let (h, port) = match hostport.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (hostport, None),
    };
    let mut out = host(h);
    if let Some(p) = port {
        out.push_str(" colon ");
        out.push_str(p);
    }
    for seg in parts.filter(|p| !p.is_empty()) {
        out.push_str(" slash ");
        out.push_str(&path_part(seg));
    }
    out
}

/// A host name, "dot" between its labels. Two-letter labels other than
/// "co" and "me" are said as letters: "U K", "I O".
fn host(h: &str) -> String {
    h.split('.')
        .filter(|l| !l.is_empty())
        .map(|l| {
            if l.len() == 2
                && l.chars().all(|c| c.is_ascii_alphabetic())
                && !matches!(l, "co" | "me")
            {
                letters(&l.to_ascii_uppercase())
            } else {
                words_of(l, " ")
            }
        })
        .collect::<Vec<_>>()
        .join(" dot ")
}

/// A name with `-`, `_` and `.` in it, as words.
fn words_of(s: &str, dot: &str) -> String {
    s.split('.')
        .map(|p| {
            p.split(['-', '_', '+'])
                .filter(|w| !w.is_empty())
                .map(|w| split_camel(w).join(" "))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(dot)
}

// --- paths -------------------------------------------------------------------

/// File extensions said as a word. The rest go letter by letter.
const WORD_EXTENSIONS: &[&str] = &[
    "json", "toml", "yaml", "lock", "log", "zip", "java", "go", "swift", "dart", "bat", "ini",
    "conf", "config", "wasm", "env", "tar", "gz", "rar", "jar", "lua", "mod", "sum", "bin", "app",
    "dmg", "pem", "key",
];

/// Extensions that mark a bare word as a file name.
const FILE_EXTENSIONS: &str = "json|toml|yaml|yml|lock|log|md|rs|py|js|mjs|cjs|ts|tsx|jsx|txt|tsv|csv|sh|ps1|bat|html|htm|css|scss|png|jpe?g|gif|svg|webp|pdf|docx?|xlsx?|pptx?|exe|dll|so|dylib|wav|mp3|mp4|zip|gz|tgz|tar|onnx|c|h|cpp|hpp|cc|go|java|kt|rb|php|swift|cs|sql|xml|ini|cfg|conf|env|wasm|lua|dart|vue|svelte";

fn paths(s: &str) -> String {
    // Windows paths: C:\Users\Will\notes.txt
    let s = re!(r"\b([A-Za-z]):\\((?:[\w.$-]+\\?)*)").replace_all(s, |c: &Captures| {
        let mut out = format!("{} drive", c[1].to_uppercase());
        for seg in c[2].split('\\').filter(|p| !p.is_empty()) {
            out.push_str(" slash ");
            out.push_str(&path_part(seg));
        }
        out
    });
    // Unix-style paths with at least one slash.
    let s = re!(r"(^|[\s(\x22'`=<])((?:~|\.{1,2})?/?(?:[\w.@-]+/)+[\w.@-]*)").replace_all(
        &s,
        |c: &Captures| {
            let path = &c[2];
            let trimmed = path.trim_end_matches(['.', ',', ';', ':']);
            if !looks_like_path(trimmed) {
                return c[0].to_string();
            }
            format!("{}{}{}", &c[1], speak_path(trimmed), &path[trimmed.len()..])
        },
    );
    // Bare file names (README.md, .gitignore, Cargo.toml).
    static FILE: OnceLock<Regex> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        Regex::new(&format!(
            r"(^|[\s(\x22'`])((?:\.?[\w-]+)(?:\.[\w-]+)*\.(?:{FILE_EXTENSIONS}))\b"
        ))
        .expect("valid regex")
    });
    let s = file.replace_all(&s, |c: &Captures| format!("{}{}", &c[1], path_part(&c[2])));
    let s = re!(r"(^|[\s(\x22'`])\.([a-z][\w-]+)\b").replace_all(&s, |c: &Captures| {
        format!("{}dot {}", &c[1], path_part(&c[2]))
    });
    s.into_owned()
}

/// Whether `name` ends in a file extension such as ".rs" or ".sh".
fn is_file_name(name: &str) -> bool {
    static FILE: OnceLock<Regex> = OnceLock::new();
    FILE.get_or_init(|| Regex::new(&format!(r"(?i)\.(?:{FILE_EXTENSIONS})$")).expect("valid regex"))
        .is_match(name.trim_end_matches(['.', ',', ';', ':', '!', '?']))
}

/// Whether a slashed token is a path rather than "and/or" or "24/7".
fn looks_like_path(p: &str) -> bool {
    if !p.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    // "w/" and "w/o" are abbreviations.
    if segs.iter().all(|s| s.len() < 2) {
        return false;
    }
    // Two plain words ("and/or", "client/server") are left to the engine,
    // which says "slash" itself.
    p.starts_with(['/', '~', '.'])
        || p.ends_with('/')
        || segs.len() >= 3
        || (segs.len() == 2 && segs.iter().any(|s| s.contains(['.', '_', '-'])))
}

fn speak_path(p: &str) -> String {
    let (lead, rest) = if let Some(r) = p.strip_prefix("~/") {
        ("home slash ", r)
    } else if let Some(r) = p.strip_prefix("./") {
        ("", r)
    } else if let Some(r) = p.strip_prefix("../") {
        ("dot dot slash ", r)
    } else if let Some(r) = p.strip_prefix('/') {
        ("slash ", r)
    } else {
        ("", p)
    };
    let parts: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(path_part)
        .collect();
    format!("{lead}{}", parts.join(" slash "))
}

/// One path component: dotfiles, extensions, and words split apart.
fn path_part(seg: &str) -> String {
    let (dotfile, seg) = match seg.strip_prefix('.') {
        Some(rest) if !rest.is_empty() => (true, rest),
        _ => (false, seg),
    };
    let pieces: Vec<&str> = seg.split('.').collect();
    let mut out = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len() && pieces.len() > 1;
        let lower = piece.to_ascii_lowercase();
        let spoken = if last && WORD_EXTENSIONS.contains(&lower.as_str()) {
            lower
        } else if last && piece.chars().all(|c| c.is_ascii_alphanumeric()) && piece.len() <= 4 {
            letters(&piece.to_ascii_uppercase())
        } else {
            piece
                .split(['-', '_'])
                .filter(|w| !w.is_empty())
                .flat_map(split_camel)
                .map(|w| dev_word(&w))
                .collect::<Vec<_>>()
                .join(" ")
        };
        out.push(spoken);
    }
    let joined = out.join(" dot ");
    if dotfile {
        format!("dot {joined}")
    } else {
        joined
    }
}

/// Short forms in paths and code that people say in full.
fn dev_word(w: &str) -> String {
    let lower = w.to_ascii_lowercase();
    let full = match lower.as_str() {
        "src" => "source",
        "usr" => "user",
        "tmp" => "temp",
        "pkg" => "package",
        "cfg" => "config",
        "msg" => "message",
        "gitignore" => "git ignore",
        "dockerignore" => "docker ignore",
        "dockerfile" => "docker file",
        "makefile" => "make file",
        "readme" => "readme",
        "ctrl" => "control",
        "cmd" => "command",
        "esc" => "escape",
        _ => return w.to_string(),
    };
    full.to_string()
}

// --- abbreviations -----------------------------------------------------------

fn abbreviations(s: &str) -> String {
    // "etc." ends a sentence as often as not: keep the stop when it does.
    let s = re!(r"\betc\.($|\s+[A-Z])").replace_all(s, "et cetera.$1");
    let s = re!(r"\betc\b\.?").replace_all(&s, "et cetera");
    let s = re!(r"(?i)\be\.g\.,?").replace_all(&s, "for example");
    let s = re!(r"(?i)\bi\.e\.,?").replace_all(&s, "that is");
    let s = re!(r"(?i)\ba\.?k\.?a\.?\b").replace_all(&s, "also known as");
    let s = re!(r"\bw/o\b").replace_all(&s, "without");
    let s = re!(r"\bw/(\s)").replace_all(&s, "with$1");
    let s = re!(r"\bvs\.?(\s)").replace_all(&s, "versus$1");
    let s = re!(r"\b(No|no)\.\s?(\d)").replace_all(&s, "number $2");
    let s = re!(r"\bPh\.D\b\.?").replace_all(&s, "PhD");
    let s = re!(r"\b(Mr|Mrs|Ms)\.").replace_all(&s, |c: &Captures| {
        match &c[1] {
            "Mr" => "Mister",
            "Mrs" => "Missus",
            _ => "Miz",
        }
        .to_string()
    });
    // Titles before a name. "St." could be a street, so it stays.
    let s = re!(r"\b(Dr|Prof)\.(\s+[A-Z])").replace_all(&s, |c: &Captures| {
        let t = if &c[1] == "Dr" { "Doctor" } else { "Professor" };
        format!("{t}{}", &c[2])
    });
    let s = re!(r"\b(?i:(approx|fig|cf|viz|incl|dept|est|max|min|govt|corp|inc|ltd|ave))\.")
        .replace_all(&s, |c: &Captures| {
            let word = match c[1].to_ascii_lowercase().as_str() {
                "approx" => "approximately",
                "fig" => "figure",
                "cf" => "compare",
                "viz" => "namely",
                "incl" => "including",
                "dept" => "department",
                "est" => "established",
                "max" => "maximum",
                "min" => "minimum",
                "govt" => "government",
                "corp" => "corporation",
                "inc" => "incorporated",
                "ltd" => "limited",
                _ => "avenue",
            };
            keep_case(&c[1], word)
        });
    let s = re!(r"\bN\.B\.").replace_all(&s, "Note");
    // Dotted initialisms: U.S., U.K., a.m.
    let s = re!(r"\b((?:[A-Za-z]\.){2,})").replace_all(&s, |c: &Captures| {
        letters(&c[1].replace('.', "").to_uppercase())
    });
    s.into_owned()
}

/// `word`, capitalised if `like` was.
fn keep_case(like: &str, word: &str) -> String {
    if like.starts_with(|c: char| c.is_uppercase()) {
        let mut chars = word.chars();
        chars
            .next()
            .map(|f| f.to_uppercase().collect::<String>() + chars.as_str())
            .unwrap_or_default()
    } else {
        word.to_string()
    }
}

// --- dates and times ---------------------------------------------------------

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_RE: &str = r"(Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|June?|July?|Aug(?:ust)?|Sept?(?:ember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)";

fn month(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    MONTHS
        .iter()
        .find(|m| lower.starts_with(&m.to_ascii_lowercase()[..3]))
        .copied()
        .unwrap_or("")
}

fn dates(s: &str) -> String {
    // ISO dates: 2024-03-05.
    let s = re!(r"\b(\d{4})-(\d{2})-(\d{2})\b").replace_all(s, |c: &Captures| {
        let (m, d) = (num(&c[2]) as usize, num(&c[3]));
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return c[0].to_string();
        }
        format!("{} {}, {}", MONTHS[m - 1], ordinal(d), year(num(&c[1])))
    });
    // Month first: March 5, 2024 / Dec. 24.
    static MDY: OnceLock<Regex> = OnceLock::new();
    let mdy = MDY.get_or_init(|| {
        Regex::new(&format!(
            r"\b{MONTH_RE}\.?\s+(\d{{1,2}})(?:st|nd|rd|th)?\b(?:(,?\s+)(\d{{4}})\b)?"
        ))
        .expect("valid regex")
    });
    let s = mdy.replace_all(&s, |c: &Captures| {
        let d = num(&c[2]);
        if !(1..=31).contains(&d) {
            return c[0].to_string();
        }
        let mut out = format!("{} {}", month(&c[1]), ordinal(d));
        if let Some(y) = c.get(4) {
            out.push_str(", ");
            out.push_str(&year(num(y.as_str())));
        }
        out
    });
    // Day first: 12 Jan 2025 / 7 Nov.
    static DMY: OnceLock<Regex> = OnceLock::new();
    let dmy = DMY.get_or_init(|| {
        Regex::new(&format!(
            r"\b(\d{{1,2}})(?:st|nd|rd|th)?\s+{MONTH_RE}(?:\.?\s+(\d{{4}})\b|\b)"
        ))
        .expect("valid regex")
    });
    let s = dmy.replace_all(&s, |c: &Captures| {
        let d = num(&c[1]);
        if !(1..=31).contains(&d) {
            return c[0].to_string();
        }
        let mut out = format!("the {} of {}", ordinal(d), month(&c[2]));
        if let Some(y) = c.get(3) {
            out.push(' ');
            out.push_str(&year(num(y.as_str())));
        }
        out
    });
    let s = re!(r"\b(Mon|Tue|Tues|Wed|Thu|Thur|Thurs|Fri|Sat|Sun)\.?(,)").replace_all(
        &s,
        |c: &Captures| {
            let day = match &c[1] {
                "Mon" => "Monday",
                "Tue" | "Tues" => "Tuesday",
                "Wed" => "Wednesday",
                "Thu" | "Thur" | "Thurs" => "Thursday",
                "Fri" => "Friday",
                "Sat" => "Saturday",
                _ => "Sunday",
            };
            format!("{day}{}", &c[2])
        },
    );
    // Time ranges: 9am-5pm.
    let s = re!(r"(?i)(\d\s*[ap]\.?m\.?)\s*[-–]\s*(\d)").replace_all(&s, "$1 to $2");
    // Clock times with a.m./p.m.
    let s =
        re!(r"(?i)\b(\d{1,2})(?::(\d{2}))?\s*([ap])\.?m\b\.?").replace_all(&s, |c: &Captures| {
            let h = num(&c[1]);
            let m = c.get(2).map_or(0, |m| num(m.as_str()));
            if !(1..=12).contains(&h) || m > 59 {
                return c[0].to_string();
            }
            format!("{} {} M", clock(h, m, false), c[3].to_uppercase())
        });
    // 24-hour or bare clock times: 14:05, 3:30.
    let s = re!(r"\b([01]?\d|2[0-3]):([0-5]\d)\b")
        .replace_all(&s, |c: &Captures| clock(num(&c[1]), num(&c[2]), true));
    // Quarters and fiscal years: Q3 2024, FY2025.
    let s =
        re!(r"\bQ([1-4])\b").replace_all(&s, |c: &Captures| format!("Q {}", cardinal(num(&c[1]))));
    let s = re!(r"\bFY\s?(\d{4}|\d{2})\b").replace_all(&s, |c: &Captures| {
        let y = num(&c[1]);
        format!("FY {}", if y < 100 { cardinal(y) } else { year(y) })
    });
    // Decades: 1980s, '90s.
    let s = re!(r"\b(1[1-9]|20)(\d)0s\b").replace_all(&s, |c: &Captures| {
        let century = num(&c[1]);
        let decade = num(&c[2]);
        let tens = if decade == 0 {
            "hundreds".to_string()
        } else {
            plural_tens(decade)
        };
        format!("{} {}", cardinal(century), tens)
    });
    let s = re!(r"(^|\s)'?(\d)0s\b").replace_all(&s, |c: &Captures| {
        format!("{}{}", &c[1], plural_tens(num(&c[2])))
    });
    // Years after a cue word: "in 1999", "since 2007", "© 2025".
    let s = re!(r"(?i)\b(in|since|by|from|until|till|of|year|circa|around|before|after|during|back in|copyright|©|Q \w+|FY)(\s+)(1[1-9]\d\d|20\d\d)\b").replace_all(&s, |c: &Captures| {
        format!("{}{}{}", &c[1], &c[2], year(num(&c[3])))
    });
    let s = re!(r"(©\s*)(1[1-9]\d\d|20\d\d)\b")
        .replace_all(&s, |c: &Captures| format!("{}{}", &c[1], year(num(&c[2]))));
    s.into_owned()
}

/// "seventies", "twenties", "tens".
fn plural_tens(d: u64) -> String {
    let t = cardinal(d * 10);
    match t.strip_suffix('y') {
        Some(stem) => format!("{stem}ies"),
        None => format!("{t}s"),
    }
}

/// A clock time: "three thirty", "fourteen oh five", "fourteen hundred".
fn clock(h: u64, m: u64, twenty_four: bool) -> String {
    let hours = cardinal(h);
    match m {
        0 if twenty_four && h > 12 => format!("{hours} hundred"),
        0 if twenty_four => format!("{hours} o'clock"),
        0 => hours,
        1..=9 => format!("{hours} oh {}", cardinal(m)),
        _ => format!("{hours} {}", cardinal(m)),
    }
}

// --- money -------------------------------------------------------------------

fn money(s: &str) -> String {
    let s = re!(r"([$£€¥])\s?(\d{1,3}(?:,\d{3})+|\d+)(?:\.(\d+))?(?:\s?(k|K|m|M|bn|b|B|tn|million|billion|thousand|trillion)\b)?").replace_all(s, |c: &Captures| {
        let (one, many, cent, cents) = match &c[1] {
            "$" => ("dollar", "dollars", "cent", "cents"),
            "£" => ("pound", "pounds", "penny", "pence"),
            "€" => ("euro", "euros", "cent", "cents"),
            _ => ("yen", "yen", "", ""),
        };
        let whole = num(&c[2].replace(',', ""));
        if let Some(scale) = c.get(4) {
            let scale = match scale.as_str() {
                "k" | "K" | "thousand" => "thousand",
                "m" | "M" | "million" => "million",
                "tn" | "trillion" => "trillion",
                _ => "billion",
            };
            let amount = match c.get(3) {
                Some(f) => decimal(&c[2], f.as_str()),
                None => cardinal(whole),
            };
            return format!("{amount} {scale} {many}");
        }
        match c.get(3) {
            Some(f) if f.as_str().len() == 2 && !cent.is_empty() => {
                let fraction = num(f.as_str());
                if whole == 0 {
                    format!("{} {}", cardinal(fraction), if fraction == 1 { cent } else { cents })
                } else if fraction == 0 {
                    format!("{} {}", cardinal(whole), if whole == 1 { one } else { many })
                } else {
                    format!("{} {} {}", cardinal(whole), if whole == 1 { one } else { many }, cardinal(fraction))
                }
            }
            Some(f) => format!("{} {many}", decimal(&c[2], f.as_str())),
            None => format!("{} {}", cardinal(whole), if whole == 1 { one } else { many }),
        }
    });
    let s = re!(r"\b(\d+)p\b").replace_all(&s, |c: &Captures| {
        let n = num(&c[1]);
        format!("{} {}", cardinal(n), if n == 1 { "penny" } else { "pence" })
    });
    let s = re!(r"\b(\d+)\s?¢")
        .replace_all(&s, |c: &Captures| format!("{} cents", cardinal(num(&c[1]))));
    s.into_owned()
}

// --- versions ----------------------------------------------------------------

fn versions(s: &str) -> String {
    // v1.2.3, v2, v0.2.0-beta.1
    let s = re!(r"(^|[\s(\[,;:])[vV](\d+(?:\.\d+)*)((?:-[A-Za-z]+(?:\.?\d+)?)?)\b")
        .replace_all(s, |c: &Captures| {
            format!("{}version {}{}", &c[1], dotted(&c[2]), pre_release(&c[3]))
        });
    // 1.2.3, 1.0.0-rc.2: three or more parts can't be a decimal.
    let s = re!(r"\b(\d+\.\d+\.\d+(?:\.\d+)*)((?:-[A-Za-z]+(?:\.?\d+)?)?)\b")
        .replace_all(&s, |c: &Captures| {
            format!("{}{}", dotted(&c[1]), pre_release(&c[2]))
        });
    s.into_owned()
}

/// "1.13.8" as "one point thirteen point eight".
fn dotted(v: &str) -> String {
    v.split('.')
        .map(cardinal_str)
        .collect::<Vec<_>>()
        .join(" point ")
}

/// "-beta.1" as " beta one", "-rc.2" as " R C two".
fn pre_release(tag: &str) -> String {
    let Some(tag) = tag.strip_prefix('-') else {
        return String::new();
    };
    let name: String = tag.chars().take_while(char::is_ascii_alphabetic).collect();
    let rest = tag[name.len()..].trim_start_matches('.');
    let name = match name.to_ascii_lowercase().as_str() {
        "rc" => "RC".to_string(),
        other => other.to_string(),
    };
    if rest.is_empty() {
        format!(" {name}")
    } else {
        format!(" {name} {}", cardinal_str(rest))
    }
}

// --- units -------------------------------------------------------------------

/// Unit symbols, case-sensitive, with their singular and plural names.
const UNITS: &[(&str, &str, &str)] = &[
    ("km", "kilometer", "kilometers"),
    ("cm", "centimeter", "centimeters"),
    ("mm", "millimeter", "millimeters"),
    ("nm", "nanometer", "nanometers"),
    ("mi", "mile", "miles"),
    ("ft", "foot", "feet"),
    ("kg", "kilogram", "kilograms"),
    ("mg", "milligram", "milligrams"),
    ("g", "gram", "grams"),
    ("lb", "pound", "pounds"),
    ("lbs", "pounds", "pounds"),
    ("oz", "ounce", "ounces"),
    ("ml", "milliliter", "milliliters"),
    ("mL", "milliliter", "milliliters"),
    ("L", "liter", "liters"),
    ("l", "liter", "liters"),
    ("ms", "millisecond", "milliseconds"),
    ("µs", "microsecond", "microseconds"),
    ("us", "microsecond", "microseconds"),
    ("ns", "nanosecond", "nanoseconds"),
    ("s", "second", "seconds"),
    ("sec", "second", "seconds"),
    ("secs", "seconds", "seconds"),
    ("min", "minute", "minutes"),
    ("mins", "minutes", "minutes"),
    ("h", "hour", "hours"),
    ("hr", "hour", "hours"),
    ("hrs", "hours", "hours"),
    ("B", "byte", "bytes"),
    ("KB", "kilobyte", "kilobytes"),
    ("kB", "kilobyte", "kilobytes"),
    ("MB", "megabyte", "megabytes"),
    ("GB", "gigabyte", "gigabytes"),
    ("TB", "terabyte", "terabytes"),
    ("PB", "petabyte", "petabytes"),
    ("KiB", "kibibyte", "kibibytes"),
    ("MiB", "mebibyte", "mebibytes"),
    ("GiB", "gibibyte", "gibibytes"),
    ("kbps", "kilobit per second", "kilobits per second"),
    ("Kbps", "kilobit per second", "kilobits per second"),
    ("Mbps", "megabit per second", "megabits per second"),
    ("Gbps", "gigabit per second", "gigabits per second"),
    ("Hz", "hertz", "hertz"),
    ("kHz", "kilohertz", "kilohertz"),
    ("MHz", "megahertz", "megahertz"),
    ("GHz", "gigahertz", "gigahertz"),
    ("mph", "mile per hour", "miles per hour"),
    ("kph", "kilometer per hour", "kilometers per hour"),
    ("km/h", "kilometer per hour", "kilometers per hour"),
    ("W", "watt", "watts"),
    ("kW", "kilowatt", "kilowatts"),
    ("MW", "megawatt", "megawatts"),
    ("kWh", "kilowatt hour", "kilowatt hours"),
    ("V", "volt", "volts"),
    ("mA", "milliamp", "milliamps"),
    ("mAh", "milliamp hour", "milliamp hours"),
    ("px", "pixel", "pixels"),
    ("dpi", "DPI", "DPI"),
    ("fps", "frame per second", "frames per second"),
    ("rpm", "RPM", "RPM"),
];

/// Units that are also common words or letters, so they need to touch the
/// number ("5s", "2L") to count.
const TIGHT_UNITS: &[&str] = &["s", "g", "h", "l", "L", "B", "W", "V", "mi", "us"];

fn units(s: &str) -> String {
    // Temperatures.
    let s = re!(r"(\d)\s?°\s?([CF])\b").replace_all(s, |c: &Captures| {
        format!(
            "{} degrees {}",
            &c[1],
            if &c[2] == "C" {
                "Celsius"
            } else {
                "Fahrenheit"
            }
        )
    });
    let s = re!(r"(\d)\s?°").replace_all(&s, "$1 degrees");
    // Durations: 2h 30m.
    let s = re!(r"\b(\d+)\s?h\s?(\d+)\s?m\b").replace_all(&s, |c: &Captures| {
        let (h, m) = (num(&c[1]), num(&c[2]));
        format!(
            "{} {} {} {}",
            cardinal(h),
            if h == 1 { "hour" } else { "hours" },
            cardinal(m),
            if m == 1 { "minute" } else { "minutes" }
        )
    });
    static UNIT: OnceLock<Regex> = OnceLock::new();
    let unit = UNIT.get_or_init(|| {
        let mut names: Vec<&str> = UNITS.iter().map(|u| u.0).collect();
        names.sort_by_key(|n| std::cmp::Reverse(n.len()));
        let alts: Vec<String> = names.iter().map(|n| regex::escape(n)).collect();
        Regex::new(&format!(
            r"\b(\d+(?:\.\d+)?)(\s?)({})(?:\b|$)",
            alts.join("|")
        ))
        .expect("valid regex")
    });
    unit.replace_all(&s, |c: &Captures| {
        let name = &c[3];
        let spaced = !c[2].is_empty();
        if spaced && TIGHT_UNITS.contains(&name) {
            return c[0].to_string();
        }
        let Some(&(_, one, many)) = UNITS.iter().find(|u| u.0 == name) else {
            return c[0].to_string();
        };
        let n = &c[1];
        let word = if n == "1" { one } else { many };
        format!("{n} {word}")
    })
    .into_owned()
}

// --- numbers -----------------------------------------------------------------

fn numbers(s: &str) -> String {
    // "#42" is "number 42".
    let s = re!(r"#(\d+)\b").replace_all(s, "number $1");
    // Ordinals: 1st, 22nd, 100th.
    let s = re!(r"\b(\d+)(?:st|nd|rd|th)\b").replace_all(&s, |c: &Captures| ordinal(num(&c[1])));
    // Fractions and pairs: 3/4, 24/7.
    let s = re!(r"(^|[^\d/])(\d+)/(\d+)\b").replace_all(&s, |c: &Captures| {
        let (n, d) = (num(&c[2]), num(&c[3]));
        format!("{}{}", &c[1], fraction(n, d))
    });
    // Ranges: 2-3 days, a 3-1 win.
    let s =
        re!(r"(^|[^\d-])(\d+(?:\.\d+)?)\s?[-–]\s?(\d+(?:\.\d+)?)\b").replace_all(&s, "$1$2 to $3");
    // Negative numbers.
    let s = re!(r"(^|[\s(])[-−](\d)").replace_all(&s, "${1}minus $2");
    // About: ~30, ≈ 30.
    let s = re!(r"[~≈]\s?(\d)").replace_all(&s, "about $1");
    // Multipliers: 10x, 3.2x.
    let s = re!(r"\b(\d+(?:\.\d+)?)[x×]\b").replace_all(&s, "$1 times");
    // Percentages.
    let s = re!(r"(\d)\s?%").replace_all(&s, "$1 percent");
    // Thousands and millions: 82M, 10k. Not "4K", a resolution.
    let s = re!(r"\b(\d+(?:\.\d+)?)([kM]|bn)\b").replace_all(&s, |c: &Captures| {
        let scale = match &c[2] {
            "k" => "thousand",
            "M" => "million",
            _ => "billion",
        };
        format!("{} {scale}", &c[1])
    });
    // Letters stuck to numbers: 24H2, k2.
    let s = re!(r"\b(\d+)([A-Z])(\d+)\b").replace_all(&s, "$1 $2 $3");
    // Digits glued to letters: "webview2", "gtk4", "3D".
    let s = re!(r"([A-Za-z])(\d)").replace_all(&s, "$1 $2");
    let s = re!(r"(\d)([A-Za-z])").replace_all(&s, "$1 $2");
    // Everything else.
    re!(r"(\d{1,3}(?:,\d{3})+|\d+)(?:\.(\d+))?")
        .replace_all(&s, |c: &Captures| match c.get(2) {
            Some(f) => decimal(&c[1], f.as_str()),
            None => cardinal_str(&c[1]),
        })
        .into_owned()
}

fn fraction(n: u64, d: u64) -> String {
    let unit = match d {
        2 => Some(("half", "halves")),
        3 => Some(("third", "thirds")),
        4 => Some(("quarter", "quarters")),
        5..=10 => None,
        _ => return format!("{} {}", cardinal(n), cardinal(d)),
    };
    if n == 0 || n >= d {
        return format!("{} {}", cardinal(n), cardinal(d));
    }
    let (one, many) = unit.map_or_else(
        || {
            let o = ordinal(d);
            (o.clone(), format!("{o}s"))
        },
        |(a, b)| (a.to_string(), b.to_string()),
    );
    format!("{} {}", cardinal(n), if n == 1 { one } else { many })
}

// --- code --------------------------------------------------------------------

fn code(s: &str) -> String {
    // Operators.
    let s = re!(r"\s*(?:!=|≠)\s*").replace_all(s, " is not equal to ");
    let s = re!(r"\s*(?:>=|≥)\s*").replace_all(&s, " is at least ");
    let s = re!(r"\s*(?:<=|≤)\s*").replace_all(&s, " is at most ");
    let s = re!(r"\s*===?\s*").replace_all(&s, " equals ");
    let s = re!(r"(\w)\s*=\s*(\w)").replace_all(&s, "$1 equals $2");
    let s = re!(r"\s+>\s+").replace_all(&s, " is greater than ");
    let s = re!(r"\s+<\s+").replace_all(&s, " is less than ");
    // Calls and paths: foo(), a::b.
    let s = re!(r"(\w)\(\)").replace_all(&s, "$1");
    let s = re!(r"(\w)::(\w)").replace_all(&s, "$1 $2");
    // Key names: Ctrl+C.
    let s = re!(r"\b(?i:(ctrl|cmd|esc))\b")
        .replace_all(&s, |c: &Captures| keep_case(&c[1], &dev_word(&c[1])));
    // Flags: --release, -v.
    let s = re!(r"(^|\s)--([a-z][\w-]*)").replace_all(&s, "${1}dash dash $2");
    // snake_case, SCREAMING_SNAKE and camelCase identifiers, as words.
    re!(r"\b[A-Za-z][A-Za-z0-9]*(?:_+[A-Za-z0-9]+)*\b")
        .replace_all(&s, |c: &Captures| {
            let word = &c[0];
            let snake = word.contains('_');
            if !snake && !is_camel(word) {
                return word.to_string();
            }
            let screaming = !word.chars().any(|c| c.is_ascii_lowercase());
            word.split('_')
                .filter(|p| !p.is_empty())
                .flat_map(split_camel)
                .map(|p| {
                    // Acronym runs stay capitalised; words go lower case.
                    if screaming
                        || !p
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                    {
                        let w = dev_word(&p.to_ascii_lowercase());
                        // No vowel: letters, as in "http" or "npm".
                        if (2..=5).contains(&w.len()) && !w.contains(['a', 'e', 'i', 'o', 'u', 'y'])
                        {
                            letters(&w)
                        } else {
                            w
                        }
                    } else {
                        p
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .into_owned()
}

/// camelCase or PascalCase with more than one part: "getUserName",
/// "HashMap", "XMLHttpRequest". Not "Hello", "API" or "PRs".
fn is_camel(w: &str) -> bool {
    let c: Vec<char> = w.chars().collect();
    c.windows(2)
        .any(|p| p[0].is_ascii_lowercase() && p[1].is_ascii_uppercase())
        || c.windows(3).enumerate().any(|(i, p)| {
            p[0].is_ascii_uppercase()
                && p[1].is_ascii_uppercase()
                && p[2].is_ascii_lowercase()
                && !(p[2] == 's' && i + 3 == c.len())
        })
}

/// Split camelCase and acronym runs: "XMLHttpRequest" → XML, Http, Request.
fn split_camel(w: &str) -> Vec<String> {
    let chars: Vec<char> = w.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let boundary = match prev {
            Some(p) if c.is_uppercase() => {
                p.is_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_uppercase()
                        && next.is_some_and(char::is_lowercase)
                        && !(c == 'S' && next.is_none()))
            }
            Some(p) if c.is_ascii_digit() => p.is_alphabetic(),
            Some(p) if c.is_alphabetic() => p.is_ascii_digit(),
            _ => false,
        };
        if boundary && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // "Mc" and "Mac" names stay whole.
    if out.len() > 1 && matches!(out[0].as_str(), "Mc") {
        let first = out.remove(0);
        out[0] = first + &out[0];
    }
    out
}

// --- symbols -----------------------------------------------------------------

fn symbols(s: &str) -> String {
    let s = re!(r"\s*(?:→|⟶|⇒|->|=>)\s*").replace_all(s, " to ");
    let s = re!(r"\s*(?:←|<-)\s*").replace_all(&s, " from ");
    let s = re!(r"\s*&\s*").replace_all(&s, " and ");
    let s = re!(r"\s*±\s*").replace_all(&s, " plus or minus ");
    let s = re!(r"\s*×\s*").replace_all(&s, " times ");
    let s = re!(r"\s*÷\s*").replace_all(&s, " divided by ");
    let s = re!(r"\+\+").replace_all(&s, " plus plus ");
    let s = re!(r"(\w)\s*\+\s*(\w)").replace_all(&s, "$1 plus $2");
    let s = re!(r"(\w)\+").replace_all(&s, "$1 plus");
    let s = re!(r"©\s*").replace_all(&s, "copyright ");
    let s = re!(r"[®™]").replace_all(&s, "");
    let s = re!(r"§\s*").replace_all(&s, "section ");
    let s = re!(r"(^|\s)@(\w)").replace_all(&s, "${1}at $2");
    let s = re!(r"(^|\s)#([A-Za-z]\w*)").replace_all(&s, "${1}hashtag $2");
    let s = re!(r"\band/or\b").replace_all(&s, "and or");
    let s = re!(r"\s*…\s*").replace_all(&s, ", ");
    let s = re!(r"\.{3,}").replace_all(&s, ", ");
    let s = re!(r"\s+[—–-]\s+|—").replace_all(&s, ", ");
    let s = re!(r"\s*[|•·◦▪‣]\s*").replace_all(&s, ", ");
    let s = re!(r"[“”„«»]").replace_all(&s, "\"");
    let s = re!(r"[‘’‚]").replace_all(&s, "'");
    s.into_owned()
}

// --- letters ---------------------------------------------------------------

/// Letters to be said one by one. Kokoro spells out a word in capitals by
/// itself ("API", "TXT"); spaced capitals read worse, because a lone "A"
/// becomes the article.
fn letters(word: &str) -> String {
    word.to_uppercase()
}

// --- tidy --------------------------------------------------------------------

fn tidy(s: &str) -> String {
    let s = re!(r"[ \t]+").replace_all(s.trim(), " ");
    let s = re!(r" ([,.;:!?])").replace_all(&s, "$1");
    let s = re!(r"([,;:])[,;:]+").replace_all(&s, "$1");
    let s = re!(r",\.").replace_all(&s, ".");
    let s = re!(r"^[,;:.]\s*").replace_all(&s, "");
    s.into_owned()
}

// --- number words ------------------------------------------------------------

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

fn num(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

/// Digits as words. Leading zeros ("007") and very long runs are read digit
/// by digit.
fn cardinal_str(digits: &str) -> String {
    let digits = digits.replace(',', "");
    if (digits.len() > 1 && digits.starts_with('0')) || digits.len() > 15 {
        return digits
            .chars()
            .filter_map(|c| c.to_digit(10))
            .map(|d| ONES[d as usize])
            .collect::<Vec<_>>()
            .join(" ");
    }
    cardinal(num(&digits))
}

/// "three point one four".
fn decimal(whole: &str, fraction: &str) -> String {
    let digits: Vec<&str> = fraction
        .chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| ONES[d as usize])
        .collect();
    format!("{} point {}", cardinal_str(whole), digits.join(" "))
}

pub fn cardinal(n: u64) -> String {
    if n < 20 {
        return ONES[n as usize].to_string();
    }
    if n < 100 {
        let (t, o) = (n / 10, n % 10);
        return if o == 0 {
            TENS[t as usize].to_string()
        } else {
            format!("{} {}", TENS[t as usize], ONES[o as usize])
        };
    }
    if n < 1000 {
        let (h, r) = (n / 100, n % 100);
        return if r == 0 {
            format!("{} hundred", ONES[h as usize])
        } else {
            format!("{} hundred {}", ONES[h as usize], cardinal(r))
        };
    }
    for (size, name) in [
        (1_000_000_000_000, "trillion"),
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ] {
        if n >= size {
            let (big, r) = (n / size, n % size);
            return if r == 0 {
                format!("{} {name}", cardinal(big))
            } else {
                format!("{} {name} {}", cardinal(big), cardinal(r))
            };
        }
    }
    unreachable!()
}

pub fn ordinal(n: u64) -> String {
    let c = cardinal(n);
    let (head, last) = match c.rsplit_once(' ') {
        Some((h, l)) => (format!("{h} "), l.to_string()),
        None => (String::new(), c.clone()),
    };
    let last = match last.as_str() {
        "one" => "first".to_string(),
        "two" => "second".to_string(),
        "three" => "third".to_string(),
        "five" => "fifth".to_string(),
        "eight" => "eighth".to_string(),
        "nine" => "ninth".to_string(),
        "twelve" => "twelfth".to_string(),
        w if w.ends_with('y') => format!("{}ieth", &w[..w.len() - 1]),
        w => format!("{w}th"),
    };
    format!("{head}{last}")
}

/// A year: "nineteen ninety nine", "two thousand seven", "twenty twenty four".
pub fn year(n: u64) -> String {
    match n {
        2000..=2009 => cardinal(n),
        1100..=2099 => {
            let (hi, lo) = (n / 100, n % 100);
            match lo {
                0 => format!("{} hundred", cardinal(hi)),
                1..=9 => format!("{} oh {}", cardinal(hi), cardinal(lo)),
                _ => format!("{} {}", cardinal(hi), cardinal(lo)),
            }
        }
        _ => cardinal(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn n(s: &str) -> String {
        normalise(s)
    }

    #[test]
    fn number_words() {
        assert_eq!(cardinal(0), "zero");
        assert_eq!(cardinal(15), "fifteen");
        assert_eq!(cardinal(40), "forty");
        assert_eq!(cardinal(1234), "one thousand two hundred thirty four");
        assert_eq!(cardinal(8_100_000_000), "eight billion one hundred million");
        assert_eq!(ordinal(1), "first");
        assert_eq!(ordinal(22), "twenty second");
        assert_eq!(ordinal(12), "twelfth");
        assert_eq!(ordinal(40), "fortieth");
        assert_eq!(year(1999), "nineteen ninety nine");
        assert_eq!(year(2007), "two thousand seven");
        assert_eq!(year(2024), "twenty twenty four");
        assert_eq!(year(1900), "nineteen hundred");
        assert_eq!(year(1905), "nineteen oh five");
    }

    #[test]
    fn plain_prose_is_left_alone() {
        let prose = "The quick brown fox jumps over the lazy dog. It was, I think, fine!";
        assert_eq!(n(prose), prose);
    }

    #[test]
    fn line_breaks_are_kept_for_the_chunker() {
        assert_eq!(n("First line\n\nSecond line"), "First line\n\nSecond line");
        assert_eq!(n("a\r\nb"), "a\nb");
    }

    #[test]
    fn markdown_keeps_the_words() {
        assert_eq!(n("# Getting started"), "Getting started.");
        assert_eq!(
            n("**Note:** this is *really* `important`."),
            "Note: this is really important."
        );
        assert_eq!(
            n("See [the guide](https://example.com/guide)."),
            "See the guide."
        );
        assert_eq!(n("![diagram](img/flow.png)"), "diagram");
        assert_eq!(n("- [x] Ship it\n- [ ] Test it"), "Ship it.\nTest it.");
        // A list item that wraps gets its stop at the end, not mid-sentence.
        assert_eq!(
            n("1. Read it a bit at a time, so\n   it starts fast\n2. Done"),
            "one. Read it a bit at a time, so\nit starts fast\ntwo. Done."
        );
        assert_eq!(
            n("| Name | Score |\n| --- | --- |\n| Ana | 9 |"),
            "Name, Score.\n\nAna, nine."
        );
        assert_eq!(n("> Quoted"), "Quoted");
        assert_eq!(n("```rust\nlet x = 1;\n```"), "\nlet x equals one;\n");
        // A dropped line still separates paragraphs.
        assert_eq!(n("Heading\n---\nText"), "Heading\n\nText");
        assert_eq!(n("Line one<br>line two"), "Line one, line two");
    }

    #[test]
    fn markdown_leaves_snake_case_and_multiplication() {
        assert_eq!(n("a * b"), "a * b");
        assert_eq!(n("max_chars"), "max chars");
    }

    #[test]
    fn emoji_are_dropped() {
        assert_eq!(n("Shipped it 🚀🚀"), "Shipped it");
        assert_eq!(n("Nice 👏🏽 work ✅"), "Nice work");
        assert_eq!(n("Warning ⚠️ full"), "Warning full");
    }

    #[test]
    fn urls_and_emails() {
        assert_eq!(n("See https://www.example.com."), "See example dot com.");
        assert_eq!(
            n("https://github.com/rust-lang/rust?tab=readme#top"),
            "github dot com slash rust lang slash rust"
        );
        assert_eq!(n("http://localhost:3000"), "localhost colon three thousand");
        assert_eq!(n("www.bbc.co.uk"), "bbc dot co dot UK");
        assert_eq!(n("will@example.com"), "will at example dot com");
        assert_eq!(n("<https://tauri.app>"), "tauri dot app");
    }

    #[test]
    fn paths_and_file_names() {
        assert_eq!(
            n("src/tts/kokoro.rs"),
            "source slash tts slash kokoro dot RS"
        );
        assert_eq!(
            n("~/.config/app.json"),
            "home slash dot config slash app dot json"
        );
        assert_eq!(
            n("C:\\Users\\Will\\a.txt"),
            "C drive slash Users slash Will slash a dot TXT"
        );
        assert_eq!(
            n("Read README.md and .gitignore"),
            "Read readme dot MD and dot git ignore"
        );
        // Not paths.
        assert_eq!(n("and/or"), "and or");
        assert_eq!(n("24/7"), "twenty four seven");
        assert_eq!(n("client/server"), "client/server");
        // File names whose extension is also a domain (".sh", ".rs").
        assert_eq!(n("Run build-engine.sh now"), "Run build engine dot SH now");
        assert_eq!(n("src-tauri/engine"), "source tauri slash engine");
    }

    #[test]
    fn abbreviations() {
        assert_eq!(n("fruit, e.g. apples"), "fruit, for example apples");
        assert_eq!(n("i.e. this"), "that is this");
        assert_eq!(n("pens, etc. are here"), "pens, et cetera are here");
        assert_eq!(n("pens, paper, etc."), "pens, paper, et cetera.");
        assert_eq!(n("Python vs. Rust"), "Python versus Rust");
        assert_eq!(n("Dr. Smith"), "Doctor Smith");
        assert_eq!(n("Main St. Then left"), "Main St. Then left");
        assert_eq!(n("Mr. and Mrs. Jones"), "Mister and Missus Jones");
        assert_eq!(n("w/ milk, w/o sugar"), "with milk, without sugar");
        assert_eq!(n("the U.S. and U.K."), "the US and UK");
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(n("on 2024-03-05"), "on March fifth, twenty twenty four");
        assert_eq!(n("March 5, 2024"), "March fifth, twenty twenty four");
        assert_eq!(
            n("12 Jan 2025"),
            "the twelfth of January twenty twenty five"
        );
        assert_eq!(n("by Fri, 7 Nov."), "by Friday, the seventh of November.");
        assert_eq!(n("at 3:30 pm"), "at three thirty P M");
        assert_eq!(n("9am-5pm"), "nine A M to five P M");
        assert_eq!(n("at 14:05"), "at fourteen oh five");
        assert_eq!(n("the 1980s"), "the nineteen eighties");
        assert_eq!(n("in 1999"), "in nineteen ninety nine");
        assert_eq!(n("Q3 2024"), "Q three twenty twenty four");
    }

    #[test]
    fn money() {
        assert_eq!(n("$5"), "five dollars");
        assert_eq!(n("$1"), "one dollar");
        assert_eq!(n("$3.50"), "three dollars fifty");
        assert_eq!(n("$0.99"), "ninety nine cents");
        assert_eq!(n("£1,200"), "one thousand two hundred pounds");
        assert_eq!(n("$2.5M"), "two point five million dollars");
        assert_eq!(n("$120k"), "one hundred twenty thousand dollars");
        assert_eq!(n("99p"), "ninety nine pence");
    }

    #[test]
    fn versions() {
        assert_eq!(n("v1.2.3"), "version one point two point three");
        assert_eq!(n("v2"), "version two");
        assert_eq!(n("large-v3-turbo"), "large-v three-turbo");
        assert_eq!(n("1.13.8"), "one point thirteen point eight");
        assert_eq!(
            n("v0.2.0-beta.1"),
            "version zero point two point zero beta one"
        );
        assert_eq!(n("1.0.0-rc.2"), "one point zero point zero RC two");
    }

    #[test]
    fn units() {
        assert_eq!(n("5km"), "five kilometers");
        assert_eq!(n("1 MB"), "one megabyte");
        assert_eq!(n("10 MB"), "ten megabytes");
        assert_eq!(n("25°C"), "twenty five degrees Celsius");
        assert_eq!(n("2h 30m"), "two hours thirty minutes");
        assert_eq!(n("at 50 km/h."), "at fifty kilometers per hour.");
        assert_eq!(n("200ms"), "two hundred milliseconds");
        // A lone letter after a space is not a unit.
        assert_eq!(n("5 s"), "five s");
    }

    #[test]
    fn numbers() {
        assert_eq!(n("1,234"), "one thousand two hundred thirty four");
        assert_eq!(n("3.14"), "three point one four");
        assert_eq!(n("-5"), "minus five");
        assert_eq!(n("1st and 22nd"), "first and twenty second");
        assert_eq!(n("3/4"), "three quarters");
        assert_eq!(n("1/2"), "one half");
        assert_eq!(n("12.5%"), "twelve point five percent");
        assert_eq!(n("10x"), "ten times");
        assert_eq!(n("#42"), "number forty two");
        assert_eq!(n("~30"), "about thirty");
        assert_eq!(n("2-3 days"), "two to three days");
        assert_eq!(n("007"), "zero zero seven");
        assert_eq!(n("Kokoro-82M"), "Kokoro-eighty two million");
        assert_eq!(n("webview2 and gtk4"), "webview two and gtk four");
        assert_eq!(n("10k users"), "ten thousand users");
        assert_eq!(n("a 4K display"), "a four K display");
    }

    #[test]
    fn code() {
        assert_eq!(n("getUserName()"), "get user name");
        assert_eq!(n("MAX_CHARS"), "max chars");
        assert_eq!(n("HashMap"), "hash map");
        assert_eq!(n("XMLHttpRequest"), "XML HTTP request");
        assert_eq!(n("reader::speak_text"), "reader speak text");
        assert_eq!(n("RUST_LOG=debug"), "rust log equals debug");
        assert_eq!(n("if a == b"), "if a equals b");
        assert_eq!(n("count > 0"), "count is greater than zero");
        assert_eq!(n("--release"), "dash dash release");
        assert_eq!(n("Ctrl+C"), "Control plus C");
        assert_eq!(n("C++ code"), "C plus plus code");
    }

    #[test]
    fn symbols() {
        assert_eq!(n("A → B"), "A to B");
        assert_eq!(n("Tom & Jerry"), "Tom and Jerry");
        assert_eq!(n("Wait… what"), "Wait, what");
        assert_eq!(n("rock — paper"), "rock, paper");
        assert_eq!(n("© 2025"), "copyright twenty twenty five");
        assert_eq!(n("± 2"), "plus or minus two");
        assert_eq!(n("@mentions"), "at mentions");
    }

    #[test]
    fn acronyms_are_left_to_the_engine() {
        // Kokoro spells out capitals itself.
        assert_eq!(n("The API and IT team"), "The API and IT team");
        assert_eq!(n("XMLHttpRequest"), "XML HTTP request");
    }

    #[test]
    fn applies_to_english_local_voices_only() {
        let mut s = Settings {
            provider: Provider::Kokoro,
            ..Settings::default()
        };
        assert!(applies(&s));
        s.kokoro_voice = "bf_emma".into();
        assert!(applies(&s));
        s.kokoro_voice = "ef_dora".into();
        assert!(!applies(&s));
        s.provider = Provider::Piper;
        assert!(applies(&s));
        s.piper_voice = "es_ES-sharvard-medium".into();
        assert!(!applies(&s));
        s.provider = Provider::Openai;
        assert!(!applies(&s));
    }

    #[test]
    fn every_rule_has_a_name() {
        let names: Vec<_> = Rule::ALL.iter().map(|r| r.name()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len());
    }

    proptest! {
        #[test]
        fn never_panics_and_keeps_lines(text in "[ -~\\n€£¥°±×→…—•✅🚀é]{0,200}") {
            let out = normalise(&text);
            let breaks = text.replace("\r\n", "\n").replace('\r', "\n").matches('\n').count();
            prop_assert_eq!(out.matches('\n').count(), breaks);
        }
    }
}
