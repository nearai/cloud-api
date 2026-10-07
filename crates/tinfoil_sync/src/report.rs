//! The PR body: which rows were added and why nothing else was.

use crate::classify::{Added, Source};

/// Keep only characters that can appear in slugs, repos, tags and hex, so
/// upstream-supplied text cannot inject markdown or mentions into the PR body.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .take(200)
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '-' | '/' | '@' | ':' | ' ' | '=')
            {
                c
            } else {
                '?'
            }
        })
        .collect()
}

pub fn render_markdown(added: &[Added], notes: &[String], date: &str) -> String {
    let mut out = format!("# Tinfoil measurement sync, {date}\n\n");
    if added.is_empty() {
        out.push_str(
            "No new rows: every live measurement that Sigstore confirms is already pinned.\n",
        );
    } else {
        out.push_str(
            "Each row below was seen in a live Tinfoil attestation or proxy document and \
             equals the measurement in the release's Sigstore attestation (Fulcio chain, \
             Rekor inclusion, signer `tinfoil-release-publish.yml` in the same repo at the \
             same tag). Router rows additionally passed full SEV-SNP verification \
             (VCEK chain, TCB floor, debug bit, TLS key binding). Model rows are checked \
             against the Sigstore predicate; the model enclaves' own attestations are not \
             verified by this job.\n\n",
        );
        out.push_str("| Kind | Slug | Repo | Tag | Measurement | Source | Observed |\n");
        out.push_str("|---|---|---|---|---|---|---|\n");
        for a in added {
            let source = match a.source {
                Source::Live => "live",
                Source::Earlier => "earlier run",
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | `{}` | {} | {} |\n",
                if a.slug.is_none() { "router" } else { "model" },
                a.slug.as_deref().map(sanitize).unwrap_or_default(),
                sanitize(&a.repo),
                sanitize(&a.tag),
                a.measurement
                    .iter()
                    .map(|m| sanitize(m))
                    .collect::<Vec<_>>()
                    .join(","),
                source,
                sanitize(&a.observed_at),
            ));
        }
    }
    if !notes.is_empty() {
        out.push_str("\n## Not pinned\n\n");
        for n in notes {
            out.push_str(&format!("- {}\n", sanitize(n)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn added(slug: Option<&str>, tag: &str) -> Added {
        Added {
            slug: slug.map(String::from),
            repo: "tinfoilsh/x".into(),
            tag: tag.into(),
            measurement: vec!["ab".into(), "cd".into()],
            observed_at: "2026-10-07T06:30:00Z".into(),
            source: Source::Live,
        }
    }

    #[test]
    fn lists_added_rows_with_repo_tag_measurement_and_time() {
        let md = render_markdown(
            &[added(None, "v1"), added(Some("m"), "v2")],
            &[],
            "2026-10-07",
        );
        assert!(
            md.contains("| router |  | tinfoilsh/x | v1 | `ab,cd` | live | 2026-10-07T06:30:00Z |")
        );
        assert!(md.contains("| model | m | tinfoilsh/x | v2 |"));
        assert!(!md.contains("Not pinned"));
    }

    #[test]
    fn empty_run_says_so_and_notes_are_sanitised() {
        let md = render_markdown(&[], &["x: [evil](http://e) `a`".into()], "2026-10-07");
        assert!(md.contains("No new rows"));
        assert!(md.contains("- x: ?evil??http://e? ?a?"));
    }
}
