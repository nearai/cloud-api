//! Markdown report used as the PR body and the workflow run summary.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::classify::{ObservationOutcome, RowRef, SyncReport};

/// GitHub rejects PR bodies over 65,536 characters; stay well under it.
pub const PR_BODY_LIMIT: usize = 60_000;

fn short(h: &str) -> &str {
    h.char_indices().nth(16).map_or(h, |(i, _)| &h[..i])
}

/// Render an upstream identifier (model, chute or instance id) as an inline code
/// span it cannot break out of: backticks become `'` and control characters
/// (including line breaks) become spaces, so it cannot add headings, links or
/// mentions to the PR body.
fn code(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| match c {
            '`' => '\'',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    format!("`{clean}`")
}

fn list(s: &mut String, title: &str, rows: &[RowRef]) {
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(s, "\n### {title} ({})\n", rows.len());
    for r in rows {
        let _ = writeln!(s, "- v{} `{}`", r.version, r.name);
    }
}

pub fn render_markdown(report: &SyncReport, date: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## Chutes measurement sync ({date})\n");
    let _ = writeln!(
        s,
        "A row is pinned only if Chutes publishes it **and** a quote that passed the Intel \
         signature chain, TCB floor, debug-bit check, report_data bindings and NVIDIA NRAS \
         showed the same five registers. Review, then merge; it takes effect with the next \
         release.\n"
    );
    let _ = writeln!(s, "### Rows to pin ({})\n", report.added.len());
    if report.added.is_empty() {
        let _ = writeln!(s, "None.");
    }
    for (row, on) in &report.added {
        let where_ = if on.is_empty() {
            "not seen live today; verified by a recent run of this job (see its artifact) \
             and still published"
                .to_string()
        } else {
            on.iter()
                .map(|o| format!("{} {}", code(&o.model), code(&o.instance_id)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(s, "- v{} `{}` — seen on {}", row.version, row.name, where_);
    }
    list(
        &mut s,
        "Published, not seen live yet (not pinned)",
        &report.published_not_observed,
    );
    list(
        &mut s,
        "Pinned, no longer published (kept)",
        &report.pinned_not_published,
    );
    list(
        &mut s,
        "Same version, different software identity (not pinned)",
        &report.identity_anomalies,
    );
    list(
        &mut s,
        "Name already pinned with a different RTMR0 (not pinned; needs a person)",
        &report.name_collisions,
    );
    list(
        &mut s,
        "All-zero runtime RTMR3 (never pinned)",
        &report.zero_rtmr3,
    );
    if !report.aliases.is_empty() {
        let _ = writeln!(s, "\n### Published under several names\n");
        for (row, others) in &report.aliases {
            let _ = writeln!(
                s,
                "- v{} `{}` is also published as {}",
                row.version,
                row.name,
                others.join(", ")
            );
        }
    }
    if !report.unpublished.is_empty() {
        let _ = writeln!(
            s,
            "\n### Verified live, not published (not pinned) ({})\n",
            report.unpublished.len()
        );
        for (r, on) in &report.unpublished {
            let models: BTreeSet<_> = on.iter().map(|o| code(&o.model)).collect();
            let _ = writeln!(
                s,
                "- mrtd `{}…` rtmr0 `{}…` rtmr1 `{}…` rtmr2 `{}…` rtmr3 `{}…` on {}",
                short(&r.mrtd),
                short(&r.rtmr0),
                short(&r.rtmr1),
                short(&r.rtmr2),
                short(&r.rtmr3),
                models.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
    }
    if !report.unverified.is_empty() {
        let _ = writeln!(
            s,
            "\n### Failed verification ({})\n",
            report.unverified.len()
        );
        for o in &report.unverified {
            if let ObservationOutcome::Failed { stage } = &o.outcome {
                let _ = writeln!(s, "- {} {}: {stage}", code(&o.model), code(&o.instance_id));
            }
        }
    }
    if !report.skipped.is_empty() {
        let _ = writeln!(s, "\n### Chutes not probed ({})\n", report.skipped.len());
        for c in &report.skipped {
            let _ = writeln!(
                s,
                "- {} ({}): {}",
                code(&c.model),
                code(&c.chute_id),
                c.reason
            );
        }
    }
    let _ = writeln!(
        s,
        "\n_Pinned register sets seen live today: {}._",
        report.already_pinned
    );

    if s.chars().count() > PR_BODY_LIMIT {
        let note = "\n\n_Report truncated; per-instance results are in observations.json in the \
                    workflow run artifact._\n";
        let keep = PR_BODY_LIMIT - note.chars().count();
        s = s.chars().take(keep).collect::<String>() + note;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Observation, ObservationOutcome, RowRef, SeenOn, SyncReport};

    #[test]
    fn report_lists_additions_with_where_they_were_seen() {
        let mut rep = SyncReport::default();
        rep.added.push((
            RowRef {
                version: "1.4.1".into(),
                name: "8xb300".into(),
            },
            vec![SeenOn {
                model: "kimi".into(),
                instance_id: "i1".into(),
            }],
        ));
        let md = render_markdown(&rep, "2026-10-06");
        assert!(md.contains("v1.4.1 `8xb300`"));
        assert!(md.contains("`kimi` `i1`"));
        assert!(md.contains("2026-10-06"));
    }

    #[test]
    fn short_never_splits_a_character() {
        assert_eq!(short("abc"), "abc");
        // Byte 16 falls inside a two-byte character.
        let s = format!("a{}", "é".repeat(20));
        assert!(s.starts_with(short(&s)));
    }

    #[test]
    fn carried_rows_say_they_were_not_seen_today() {
        let mut rep = SyncReport::default();
        rep.added.push((
            RowRef {
                version: "1.4.1".into(),
                name: "8xb300".into(),
            },
            vec![],
        ));
        let md = render_markdown(&rep, "2026-10-06");
        assert!(md.contains("not seen live today"));
    }

    #[test]
    fn upstream_ids_cannot_break_out_of_their_code_span() {
        let hostile = "m\n# Rows to pin (0)\n@team `x` [link](http://e)";
        let mut rep = SyncReport::default();
        rep.added.push((
            RowRef {
                version: "1.4.1".into(),
                name: "8xb300".into(),
            },
            vec![SeenOn {
                model: hostile.into(),
                instance_id: hostile.into(),
            }],
        ));
        rep.unverified.push(Observation {
            model: hostile.into(),
            chute_id: hostile.into(),
            instance_id: hostile.into(),
            outcome: ObservationOutcome::Failed {
                stage: "quote".into(),
            },
        });
        rep.skipped.push(crate::classify::SkippedChute {
            model: hostile.into(),
            chute_id: hostile.into(),
            reason: "HTTP 403".into(),
        });
        let md = render_markdown(&rep, "2026-10-06");
        // No injected line breaks, so no forged heading lines.
        let headings = md
            .lines()
            .filter(|l| l.trim_start().starts_with('#') && l.contains("Rows to pin"))
            .count();
        assert_eq!(headings, 1);
        for line in md.lines() {
            // Every '@' sits inside a code span: an even number of backticks
            // precedes it on its line.
            if let Some(at) = line.find('@') {
                assert_eq!(line[..at].matches('`').count() % 2, 1, "{line}");
            }
        }
    }

    #[test]
    fn report_is_truncated_for_pr_body() {
        let mut rep = SyncReport::default();
        for i in 0..5000 {
            rep.unverified.push(Observation {
                model: format!("m{i}"),
                chute_id: "c".into(),
                instance_id: format!("inst-{i}"),
                outcome: ObservationOutcome::Failed {
                    stage: "quote".into(),
                },
            });
        }
        let md = render_markdown(&rep, "2026-10-06");
        assert!(md.chars().count() <= PR_BODY_LIMIT);
        assert!(md.contains("truncated"));
        assert!(
            md.contains("observations.json"),
            "point at the untruncated data"
        );
    }
}
