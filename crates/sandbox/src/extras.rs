//! Per-project extras images (DECISIONS.md #2): render
//! `images/worker/extras.Dockerfile.tmpl` from `qa.yaml: extras` and name the
//! result by a hash of the base image id and the package lists, so unchanged
//! extras reuse the image already built.

use kiln_types::qa::QaExtras;

/// The template shipped in `images/worker/`.
pub const EXTRAS_TEMPLATE: &str = include_str!("../../../images/worker/extras.Dockerfile.tmpl");

/// Repository extras images are tagged under.
pub const EXTRAS_REPO: &str = "kiln/worker-extras";

/// Render the extras Dockerfile. `{{#k}}…{{/k}}` blocks whose list is empty
/// are dropped; `{{k}}` becomes the shell-quoted, space-separated list.
pub fn render_extras(template: &str, extras: &QaExtras) -> String {
    let mut out = template.to_string();
    for (key, list) in [
        ("apt", &extras.apt),
        ("npm", &extras.npm),
        ("cargo", &extras.cargo),
        ("pip", &extras.pip),
    ] {
        let (open, close) = (format!("{{{{#{key}}}}}"), format!("{{{{/{key}}}}}"));
        while let Some(start) = out.find(&open) {
            let Some(end) = out[start..].find(&close).map(|i| start + i) else {
                break;
            };
            let body = if list.is_empty() {
                String::new()
            } else {
                out[start + open.len()..end].to_string()
            };
            out.replace_range(start..end + close.len(), &body);
        }
        let joined = list
            .iter()
            .map(|p| shell_quote(p))
            .collect::<Vec<_>>()
            .join(" ");
        out = out.replace(&format!("{{{{{key}}}}}"), &joined);
    }
    out
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `kiln/worker-extras:<project>-<hash>`; the hash covers `base_id` (the base
/// image's id, so a rebuilt base image yields a new tag) and the lists.
pub fn extras_tag(project_key: &str, base_id: &str, extras: &QaExtras) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |s: &str| {
        for b in s.bytes().chain([0u8]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(base_id);
    for (k, list) in [
        ("apt", &extras.apt),
        ("npm", &extras.npm),
        ("cargo", &extras.cargo),
        ("pip", &extras.pip),
    ] {
        feed(k);
        list.iter().for_each(|p| feed(p));
    }
    let key: String = project_key
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{EXTRAS_REPO}:{key}-{h:016x}")
}

/// A ustar archive holding one regular file: the build context for an
/// image built from a single Dockerfile.
pub(crate) fn tar_one(name: &str, data: &[u8]) -> Vec<u8> {
    let mut hdr = [0u8; 512];
    hdr[..name.len()].copy_from_slice(name.as_bytes());
    let octal = |buf: &mut [u8], v: u64| {
        let s = format!("{v:0w$o}\0", w = buf.len() - 1);
        buf.copy_from_slice(s.as_bytes());
    };
    octal(&mut hdr[100..108], 0o644);
    octal(&mut hdr[108..116], 0);
    octal(&mut hdr[116..124], 0);
    octal(&mut hdr[124..136], data.len() as u64);
    octal(&mut hdr[136..148], 0);
    hdr[156] = b'0';
    hdr[257..263].copy_from_slice(b"ustar\0");
    hdr[263..265].copy_from_slice(b"00");
    hdr[148..156].fill(b' ');
    let sum: u64 = hdr.iter().map(|&b| u64::from(b)).sum();
    let s = format!("{sum:06o}\0 ");
    hdr[148..156].copy_from_slice(s.as_bytes());
    let mut out = hdr.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out.extend_from_slice(&[0u8; 1024]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extras() -> QaExtras {
        QaExtras {
            apt: vec!["postgresql-client".into()],
            npm: vec!["pnpm@9".into(), "it's".into()],
            ..Default::default()
        }
    }

    #[test]
    fn renders_only_requested_blocks() {
        let df = render_extras(EXTRAS_TEMPLATE, &extras());
        assert!(df.contains("apt-get install -y --no-install-recommends 'postgresql-client'"));
        assert!(df.contains(r"npm install -g --no-audit --no-fund 'pnpm@9' 'it'\''s'"));
        assert!(!df.contains("rustup"), "{df}");
        assert!(!df.contains("pip install"), "{df}");
        let code = df
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>();
        assert!(code.iter().all(|l| !l.contains("{{")), "{df}");
        assert!(df.contains("FROM ${BASE_IMAGE}"));
    }

    #[test]
    fn tag_tracks_base_and_lists() {
        let a = extras_tag("FP", "sha256:1", &extras());
        assert!(a.starts_with("kiln/worker-extras:fp-"), "{a}");
        assert_eq!(a, extras_tag("FP", "sha256:1", &extras()));
        assert_ne!(a, extras_tag("FP", "sha256:2", &extras()));
        let mut e = extras();
        e.pip.push("pytest".into());
        assert_ne!(a, extras_tag("FP", "sha256:1", &e));
    }

    #[test]
    fn tar_is_block_aligned_with_valid_header() {
        let t = tar_one("Dockerfile", b"FROM alpine\n");
        assert_eq!(t.len(), 512 + 512 + 1024);
        assert_eq!(&t[..10], b"Dockerfile");
        assert_eq!(&t[512..524], b"FROM alpine\n");
        assert_eq!(&t[257..262], b"ustar");
    }
}
