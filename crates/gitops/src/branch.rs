//! Task branch naming.

/// Prefix every branch Kiln creates or pushes must carry.
pub const BRANCH_PREFIX: &str = "kiln/";

/// Maximum length of the part after [`BRANCH_PREFIX`] (`<key>-<slug>`).
pub const MAX_BRANCH_TAIL: usize = 50;

/// Branch name for a task: `kiln/<task-key-lower>-<slug(title)>`.
///
/// The slug is ASCII lowercase alphanumerics separated by single hyphens.
/// Common Latin accents are folded (`é` → `e`), anything else becomes a
/// separator. The `<key>-<slug>` tail is capped at [`MAX_BRANCH_TAIL`]
/// characters, cut at a word boundary where possible.
///
/// ```
/// assert_eq!(kiln_gitops::branch_name("FP-142", "Retry backoff!"), "kiln/fp-142-retry-backoff");
/// ```
pub fn branch_name(task_key: &str, title: &str) -> String {
    let key = slugify(task_key);
    let key = if key.is_empty() {
        "task".to_string()
    } else {
        truncate_words(&key, MAX_BRANCH_TAIL)
    };
    let budget = MAX_BRANCH_TAIL.saturating_sub(key.len() + 1);
    let slug = truncate_words(&slugify(title), budget);
    if slug.is_empty() {
        format!("{BRANCH_PREFIX}{key}")
    } else {
        format!("{BRANCH_PREFIX}{key}-{slug}")
    }
}

/// True for branch names Kiln is allowed to push.
pub fn is_kiln_branch(branch: &str) -> bool {
    branch
        .strip_prefix(BRANCH_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

/// Lowercase ASCII slug: `[a-z0-9]+(-[a-z0-9]+)*`.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_sep = false;
    for c in s.chars() {
        let folded = fold(c);
        let mut any = false;
        for f in folded.chars() {
            if f.is_ascii_alphanumeric() {
                if pending_sep && !out.is_empty() {
                    out.push('-');
                }
                pending_sep = false;
                out.push(f.to_ascii_lowercase());
                any = true;
            }
        }
        if !any && c != '\'' && c != '’' {
            pending_sep = true;
        }
    }
    out
}

/// Cut `slug` to at most `max` bytes, preferring to cut at a hyphen.
fn truncate_words(slug: &str, max: usize) -> String {
    if slug.len() <= max {
        return slug.to_string();
    }
    let cut = &slug[..max]; // slug is ASCII
    if slug.as_bytes()[max] == b'-' {
        return cut.trim_end_matches('-').to_string();
    }
    let cut = match cut.rfind('-') {
        Some(i) if i >= max / 2 => &cut[..i],
        _ => cut,
    };
    cut.trim_end_matches('-').to_string()
}

fn fold(c: char) -> String {
    let s = match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => "a",
        'æ' | 'Æ' => "ae",
        'ç' | 'Ç' => "c",
        'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => "i",
        'ñ' | 'Ñ' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => "o",
        'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => "u",
        'ý' | 'ÿ' | 'Ý' => "y",
        'ß' => "ss",
        _ => return c.to_string(),
    };
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        assert_eq!(
            branch_name("FP-142", "Retry backoff"),
            "kiln/fp-142-retry-backoff"
        );
        assert_eq!(
            branch_name("FP-1", "  Add `--json` flag (CLI) "),
            "kiln/fp-1-add-json-flag-cli"
        );
        assert_eq!(
            branch_name("FP-2", "Don't crash on café"),
            "kiln/fp-2-dont-crash-on-cafe"
        );
        assert_eq!(branch_name("FP-3", "日本語"), "kiln/fp-3");
        assert_eq!(branch_name("", ""), "kiln/task");
    }

    #[test]
    fn caps_length_at_word_boundary() {
        let b = branch_name(
            "FP-142",
            "Implement exponential retry backoff for the webhook delivery worker queue",
        );
        let tail = b.strip_prefix("kiln/").unwrap();
        assert!(tail.len() <= MAX_BRANCH_TAIL, "{b}");
        assert!(!tail.ends_with('-'));
        assert_eq!(b, "kiln/fp-142-implement-exponential-retry-backoff-for-the");
    }

    #[test]
    fn prefix_check() {
        assert!(is_kiln_branch("kiln/fp-1-x"));
        assert!(!is_kiln_branch("kiln/"));
        assert!(!is_kiln_branch("main"));
        assert!(!is_kiln_branch("feature/kiln/x"));
    }
}
