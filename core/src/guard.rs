//! Pre-execution safety review for action batches.
//!
//! The anaisbetts lesson we said we'd steal: a `guard.py`-style safety
//! overlay that reviews what the model asked for *before* anything moves.
//! The guard is pure (no backends, no clock): it inspects a [`Batch`] and
//! returns one [`Verdict`] per action. The executor applies the verdicts;
//! denied actions become per-action failures without aborting the batch
//! (the "failed actions never abort" contract), and `NeedsConfirm` actions
//! run only with operator confirmation (see
//! [`crate::Executor::grant_confirmation`]).

use crate::action::{Action, Batch};

/// How a text fragment scan landed: hard deny or needs-confirm.
/// Used both for single `Type` actions and for concatenated runs
/// (see the batch-level scan in [`review_batch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextHit {
    Deny,
    NeedsConfirm,
}

/// Case-insensitive fragment scan of `lower` (already lowercased text)
/// against the policy's deny/confirm fragment lists. Deny wins over
/// confirm, matching [`review`]'s per-action order.
fn scan_text_fragments<'a>(policy: &'a Policy, lower: &str) -> Option<(TextHit, &'a str)> {
    if let Some(frag) = policy
        .denied_text_fragments
        .iter()
        .find(|f| lower.contains(&f.to_lowercase()))
    {
        return Some((TextHit::Deny, frag));
    }
    if let Some(frag) = policy
        .confirm_text_fragments
        .iter()
        .find(|f| lower.contains(&f.to_lowercase()))
    {
        return Some((TextHit::NeedsConfirm, frag));
    }
    None
}

/// Per-action verdict from [`review`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Execute normally.
    Allow,
    /// Do not execute. Reason is model-facing so the model can adjust.
    Deny { reason: String },
    /// Destructive enough to want a human nod. The executor runs the action
    /// only when the operator granted confirmation for its exact content
    /// (see [`crate::Executor::grant_confirmation`] and
    /// [`crate::Executor::set_confirm_hook`]); otherwise it fails in
    /// place. The reason is model-facing.
    NeedsConfirm { reason: String },
}

impl Verdict {
    /// Whether the action may execute.
    pub fn allowed(&self) -> bool {
        matches!(self, Verdict::Allow)
    }
}

/// Configurable safety policy. Everything is data, so a host can load it
/// from a config file; `Default` is permissive-but-sane.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Batches longer than this are denied wholesale (anti-runaway).
    pub max_batch_len: usize,
    /// Key combos that are never sent, normalized as sorted uppercase
    /// names, e.g. `["ALT", "CTRL", "DEL"]`. Comparison is order-insensitive.
    pub denied_keypresses: Vec<Vec<String>>,
    /// Deny TTY-switch combos (Ctrl+Alt+F1..F12) even if not listed above.
    pub deny_tty_switch: bool,
    /// Case-insensitive substrings that make a `Type` action an instant
    /// deny (e.g. fork bombs the model should never emit).
    pub denied_text_fragments: Vec<String>,
    /// Case-insensitive substrings that make a `Type` action need
    /// confirmation (destructive shell, raw device writes).
    pub confirm_text_fragments: Vec<String>,
    /// `Wait` longer than this is denied (sleep-bomb guard).
    pub max_wait_ms: u64,
    /// Read-only mode: only observation actions (currently `Screenshot`;
    /// window-listing actions join the allowlist when they exist) run.
    /// Everything else is denied in place with a model-facing reason.
    /// Useful for recon/observation sessions where nothing may be touched.
    pub read_only: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_batch_len: 64,
            denied_keypresses: vec![
                vec!["ALT".into(), "CTRL".into(), "DEL".into()],
                vec!["ALT".into(), "BACKSPACE".into(), "CTRL".into()],
            ],
            deny_tty_switch: true,
            denied_text_fragments: vec![":(){ :|:& };:".into()],
            confirm_text_fragments: vec![
                "rm -rf".into(),
                "mkfs".into(),
                "dd if=".into(),
                "> /dev/".into(),
                "shutdown".into(),
                "reboot".into(),
                "poweroff".into(),
            ],
            max_wait_ms: 60_000,
            read_only: false,
        }
    }
}

/// Normalize a keypress combo for order-insensitive comparison.
fn normalize_combo(keys: &[String]) -> Vec<String> {
    let mut v: Vec<String> = keys.iter().map(|k| k.to_ascii_uppercase()).collect();
    v.sort_unstable();
    v
}

/// The model-facing action tag, matching the serde wire names.
fn action_tag(action: &Action) -> &'static str {
    match action {
        Action::Screenshot { .. } => "screenshot",
        Action::Move { .. } => "move",
        Action::Click { .. } => "click",
        Action::DoubleClick { .. } => "double_click",
        Action::Drag { .. } => "drag",
        Action::Scroll { .. } => "scroll",
        Action::Keypress { .. } => "keypress",
        Action::Type { .. } => "type",
        Action::Wait { .. } => "wait",
        Action::ListWindows { .. } => "list_windows",
        Action::ActiveWindow => "active_window",
        Action::FocusWindow { .. } => "focus_window",
        Action::WindowBounds { .. } => "window_bounds",
    }
}

/// Review one action against the policy.
pub fn review(policy: &Policy, action: &Action) -> Verdict {
    // Read-only runs before any other check: observation only, and the
    // reason names the refused action so the model can adjust its plan.
    // The read-only allowlist is the pure queries: screenshots, window
    // listing, the active window, and window bounds. FocusWindow changes
    // window state, so it stays denied.
    let read_only_safe = matches!(
        action,
        Action::Screenshot { .. }
            | Action::ListWindows { .. }
            | Action::ActiveWindow
            | Action::WindowBounds { .. }
    );
    if policy.read_only && !read_only_safe {
        return Verdict::Deny {
            reason: format!(
                "read-only policy denies {}: only screenshot, list_windows, active_window, and window_bounds are allowed",
                action_tag(action)
            ),
        };
    }
    match action {
        Action::Keypress { keys } => {
            let combo = normalize_combo(keys);
            if policy
                .denied_keypresses
                .iter()
                .any(|d| normalize_combo(d) == combo)
            {
                return Verdict::Deny {
                    reason: format!("keypress {:?} is denied by safety policy", keys),
                };
            }
            if policy.deny_tty_switch
                && combo.contains(&"CTRL".to_string())
                && combo.contains(&"ALT".to_string())
                && combo
                    .iter()
                    .any(|k| k.len() > 1 && k.starts_with('F') && k[1..].parse::<u8>().is_ok())
            {
                return Verdict::Deny {
                    reason: "tty-switch combos (ctrl+alt+fN) are denied by safety policy".into(),
                };
            }
            Verdict::Allow
        }
        Action::Type { text } => {
            let lower = text.to_lowercase();
            match scan_text_fragments(policy, &lower) {
                Some((TextHit::Deny, frag)) => Verdict::Deny {
                    reason: format!("typed text contains denied fragment {:?}", frag),
                },
                Some((TextHit::NeedsConfirm, frag)) => Verdict::NeedsConfirm {
                    reason: format!("typed text contains destructive fragment {:?}", frag),
                },
                None => Verdict::Allow,
            }
        }
        Action::Wait { ms } => {
            if *ms > policy.max_wait_ms {
                Verdict::Deny {
                    reason: format!(
                        "wait of {}ms exceeds policy maximum of {}ms",
                        ms, policy.max_wait_ms
                    ),
                }
            } else {
                Verdict::Allow
            }
        }
        // Coordinate actions and screenshots are unrestricted: the vision
        // loop is the control plane, and the frame contract already bounds
        // where clicks can land.
        _ => Verdict::Allow,
    }
}

/// Review a whole batch: one verdict per action, parallel to the batch.
/// A batch over `max_batch_len` denies every action.
///
/// Beyond the per-action scan, consecutive `Type` actions are concatenated
/// and scanned as one string. The lesson comes from the SwarmTraces
/// incident writeup (2026-09-26/27): agent sandboxes constrained to
/// GET-only HTTP were escaped by spreading payloads as fragments across
/// ~1M link-shortener URLs — a long enough chain of "reads" collectively
/// encodes what no single read contains. The same trick works against a
/// per-action text scan: `type "rm "` + `type "-rf /tmp/cache"` slips past
/// per-action fragment matching while producing the same keystrokes. The
/// concatenation scan closes that gap for the obvious case.
///
/// Known limits (deliberately documented, not patched): a fragment split
/// across a batch boundary, or interleaved with `Keypress` actions, still
/// evades the scan — typing is a sequence, and any text pre-screen is
/// porous against an adversary that controls the sequencing. The guard is
/// a mistake-catcher and a speed-bump, not a security boundary; the real
/// defenses against a hostile model are the operator-level controls
/// (read-only mode, the confirmation channel, the rate limiter).
pub fn review_batch(policy: &Policy, batch: &Batch) -> Vec<Verdict> {
    if batch.0.len() > policy.max_batch_len {
        return batch
            .0
            .iter()
            .map(|_| Verdict::Deny {
                reason: format!(
                    "batch length {} exceeds policy maximum {}",
                    batch.0.len(),
                    policy.max_batch_len
                ),
            })
            .collect();
    }
    let mut verdicts: Vec<Verdict> = batch.0.iter().map(|a| review(policy, a)).collect();
    // Concatenated-type scan: find maximal runs of consecutive Type
    // actions (length >= 2), scan their joined text, and escalate runs
    // whose concatenation contains a fragment no single piece did. Only
    // Allow verdicts are overridden — an action already denied or
    // confirm-gated keeps its more specific per-action reason. In
    // read-only mode the type actions are already denied, so this pass
    // can only ever fire outside it.
    let mut run_start: Option<usize> = None;
    let flush = |verdicts: &mut Vec<Verdict>, start: usize, end: usize| {
        if end - start < 2 {
            return;
        }
        let joined: String = batch.0[start..end]
            .iter()
            .filter_map(|a| match a {
                Action::Type { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // Skip the scan if any member already caught the fragment: the
        // per-action reasons are more precise than a run-level one.
        let any_flagged = verdicts[start..end].iter().any(|v| !v.allowed());
        if any_flagged {
            return;
        }
        if let Some((hit, frag)) = scan_text_fragments(policy, &joined.to_lowercase()) {
            let n = end - start;
            for v in verdicts[start..end].iter_mut() {
                *v = match hit {
                    TextHit::Deny => Verdict::Deny {
                        reason: format!(
                            "concatenated typed text (split across {} type actions) contains denied fragment {:?}",
                            n, frag
                        ),
                    },
                    TextHit::NeedsConfirm => Verdict::NeedsConfirm {
                        reason: format!(
                            "concatenated typed text (split across {} type actions) contains destructive fragment {:?}",
                            n, frag
                        ),
                    },
                };
            }
        }
    };
    for (i, action) in batch.0.iter().enumerate() {
        if matches!(action, Action::Type { .. }) {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(start) = run_start.take() {
            flush(&mut verdicts, start, i);
        }
    }
    if let Some(start) = run_start.take() {
        flush(&mut verdicts, start, batch.0.len());
    }
    verdicts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::MouseButton;
    use crate::backend::WindowId;
    use crate::frame::FrameId;

    fn keypress(keys: &[&str]) -> Action {
        Action::Keypress {
            keys: keys.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn default_policy_allows_benign_actions() {
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Screenshot { note: None },
            Action::Click {
                frame: FrameId(0),
                button: MouseButton::Left,
                x: 10,
                y: 10,
            },
            keypress(&["ctrl", "l"]),
            Action::Type {
                text: "hello world".into(),
            },
            Action::Wait { ms: 500 },
        ]);
        assert!(review_batch(&p, &batch).iter().all(|v| v.allowed()));
    }

    #[test]
    fn denies_ctrl_alt_del_regardless_of_order_or_case() {
        let p = Policy::default();
        assert_eq!(
            review(&p, &keypress(&["Del", "Ctrl", "Alt"])),
            Verdict::Deny {
                reason: "keypress [\"Del\", \"Ctrl\", \"Alt\"] is denied by safety policy".into()
            }
        );
        assert!(matches!(
            review(&p, &keypress(&["CTRL", "ALT", "DEL"])),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn denies_tty_switch_dynamically() {
        let p = Policy::default();
        assert!(matches!(
            review(&p, &keypress(&["ctrl", "alt", "f3"])),
            Verdict::Deny { .. }
        ));
        // A lone F3 or alt+F3 is fine.
        assert!(review(&p, &keypress(&["f3"])).allowed());
        assert!(review(&p, &keypress(&["alt", "f4"])).allowed());
    }

    #[test]
    fn deny_tty_switch_can_be_disabled() {
        let mut p = Policy::default();
        p.deny_tty_switch = false;
        assert!(review(&p, &keypress(&["ctrl", "alt", "f2"])).allowed());
    }

    #[test]
    fn denies_fork_bomb_text() {
        let p = Policy::default();
        assert!(matches!(
            review(
                &p,
                &Action::Type {
                    text: "x=':(){ :|:& };:'".into()
                }
            ),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn destructive_shell_needs_confirm_case_insensitive() {
        let p = Policy::default();
        assert!(matches!(
            review(
                &p,
                &Action::Type {
                    text: "sudo RM -RF /tmp/cache".into()
                }
            ),
            Verdict::NeedsConfirm { .. }
        ));
        assert!(matches!(
            review(
                &p,
                &Action::Type {
                    text: "dd if=/dev/zero of=/dev/sda".into()
                }
            ),
            Verdict::NeedsConfirm { .. }
        ));
    }

    #[test]
    fn wait_beyond_max_is_denied() {
        let p = Policy::default();
        assert!(matches!(
            review(&p, &Action::Wait { ms: 3_600_000 }),
            Verdict::Deny { .. }
        ));
        assert!(review(&p, &Action::Wait { ms: 60_000 }).allowed());
    }

    #[test]
    fn oversized_batch_denies_every_action() {
        let mut p = Policy::default();
        p.max_batch_len = 2;
        let batch = Batch(vec![
            Action::Wait { ms: 1 },
            Action::Wait { ms: 1 },
            Action::Wait { ms: 1 },
        ]);
        let verdicts = review_batch(&p, &batch);
        assert_eq!(verdicts.len(), 3);
        assert!(verdicts.iter().all(|v| matches!(v, Verdict::Deny { .. })));
    }

    #[test]
    fn custom_policy_denies_custom_combo() {
        let mut p = Policy::default();
        p.denied_keypresses
            .push(vec!["SUPER".into(), "L".into()]);
        assert!(matches!(
            review(&p, &keypress(&["super", "l"])),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn read_only_allows_only_observation() {
        let p = Policy {
            read_only: true,
            ..Policy::default()
        };
        // Observation primitives: screenshots and window queries are allowed.
        assert!(review(&p, &Action::Screenshot { note: None }).allowed());
        for a in [
            Action::ListWindows {
                title_substr: None,
                app_id: None,
            },
            Action::ActiveWindow,
            Action::WindowBounds { id: WindowId(1) },
        ] {
            assert!(review(&p, &a).allowed(), "{a:?} should be read-only-safe");
        }
        // Everything else is denied, with a reason naming the action so
        // the model can replan. focus_window changes window state: denied.
        let others = Batch(vec![
            Action::Move { frame: FrameId(1), x: 0, y: 0 },
            Action::Click {
                frame: FrameId(1),
                button: MouseButton::Left,
                x: 0,
                y: 0,
            },
            Action::DoubleClick {
                frame: FrameId(1),
                button: MouseButton::Right,
                x: 0,
                y: 0,
            },
            Action::Drag {
                frame: FrameId(1),
                path: vec![],
            },
            Action::Scroll {
                frame: FrameId(1),
                x: 0,
                y: 0,
                dx: 0.0,
                dy: 1.0,
            },
            keypress(&["ctrl", "l"]),
            Action::Type { text: "hello".into() },
            Action::Wait { ms: 100 },
            Action::FocusWindow { id: WindowId(1) },
        ]);
        let verdicts = review_batch(&p, &others);
        assert_eq!(verdicts.len(), 9);
        assert!(verdicts.iter().all(|v| matches!(v, Verdict::Deny { .. })));
        for (a, v) in others.0.iter().zip(verdicts) {
            if let Verdict::Deny { reason } = v {
                assert!(
                    reason.contains(action_tag(a)),
                    "reason {reason:?} should name the refused action"
                );
            }
        }
        // A batch made only of screenshots sails through.
        let shots = Batch(vec![
            Action::Screenshot { note: Some("recon".into()) },
            Action::Screenshot { note: None },
        ]);
        assert!(review_batch(&p, &shots).iter().all(|v| v.allowed()));
    }

    #[test]
    fn read_only_still_bounds_batch_length() {
        let p = Policy {
            read_only: true,
            max_batch_len: 1,
            ..Policy::default()
        };
        let batch = Batch(vec![
            Action::Screenshot { note: None },
            Action::Screenshot { note: None },
        ]);
        assert!(review_batch(&p, &batch)
            .iter()
            .all(|v| matches!(v, Verdict::Deny { .. })));
    }

    #[test]
    fn concatenated_type_run_trips_confirm_fragment() {
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Type {
                text: "rm ".into(),
            },
            Action::Type {
                text: "-rf /tmp/cache".into(),
            },
        ]);
        let verdicts = review_batch(&p, &batch);
        assert_eq!(verdicts.len(), 2);
        for v in &verdicts {
            assert!(
                matches!(v, Verdict::NeedsConfirm { reason } if reason.contains("split across 2 type actions") && reason.contains("rm -rf")),
                "unexpected verdict: {v:?}"
            );
        }
    }

    #[test]
    fn concatenated_type_run_trips_deny_fragment() {
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Type {
                text: ":(){ ".into(),
            },
            Action::Type {
                text: ":|:& ".into(),
            },
            Action::Type {
                text: "};:".into(),
            },
        ]);
        let verdicts = review_batch(&p, &batch);
        assert_eq!(verdicts.len(), 3);
        for v in &verdicts {
            assert!(
                matches!(v, Verdict::Deny { reason } if reason.contains("split across 3 type actions")),
                "unexpected verdict: {v:?}"
            );
        }
    }

    #[test]
    fn benign_split_types_stay_allowed() {
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Type {
                text: "hello ".into(),
            },
            Action::Type {
                text: "world".into(),
            },
        ]);
        assert!(review_batch(&p, &batch).iter().all(|v| v.allowed()));
    }

    #[test]
    fn single_type_fragment_keeps_precise_per_action_reason() {
        let p = Policy::default();
        let batch = Batch(vec![Action::Type {
            text: "rm -rf /".into(),
        }]);
        let verdicts = review_batch(&p, &batch);
        assert!(matches!(
            &verdicts[0],
            Verdict::NeedsConfirm { reason } if reason == "typed text contains destructive fragment \"rm -rf\""
        ));
    }

    #[test]
    fn non_consecutive_types_are_not_concatenated() {
        // Documented limit: a fragment split across a non-Type action still
        // evades the scan. Pinned so the gap stays visible.
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Type {
                text: "rm ".into(),
            },
            Action::Wait { ms: 100 },
            Action::Type {
                text: "-rf /tmp/cache".into(),
            },
        ]);
        assert!(review_batch(&p, &batch).iter().all(|v| v.allowed()));
    }

    #[test]
    fn run_concatenation_stops_at_batch_boundary() {
        // Documented limit: the scan only sees one batch.
        let p = Policy::default();
        let b1 = Batch(vec![Action::Type {
            text: "rm ".into(),
        }]);
        let b2 = Batch(vec![Action::Type {
            text: "-rf /tmp/cache".into(),
        }]);
        assert!(review_batch(&p, &b1).iter().all(|v| v.allowed()));
        assert!(review_batch(&p, &b2).iter().all(|v| v.allowed()));
    }

    #[test]
    fn run_escalation_skips_runs_with_flagged_members() {
        // One piece already trips the per-action scan: keep its precise
        // reason, don't rewrite the run.
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Type {
                text: "rm ".into(),
            },
            Action::Type {
                text: "rm -rf /".into(),
            },
        ]);
        let verdicts = review_batch(&p, &batch);
        assert_eq!(verdicts[0], Verdict::Allow);
        assert!(matches!(
            &verdicts[1],
            Verdict::NeedsConfirm { reason } if reason == "typed text contains destructive fragment \"rm -rf\""
        ));
    }

    #[test]
    fn type_run_scan_does_not_fire_under_read_only() {
        // Read-only already denies typing per-action; the run scan must
        // not reclassify those denies into confirm gates.
        let mut p = Policy::default();
        p.read_only = true;
        let batch = Batch(vec![
            Action::Type {
                text: "rm ".into(),
            },
            Action::Type {
                text: "-rf /tmp/cache".into(),
            },
        ]);
        let verdicts = review_batch(&p, &batch);
        for v in &verdicts {
            assert!(
                matches!(v, Verdict::Deny { reason } if reason.contains("read-only")),
                "unexpected verdict: {v:?}"
            );
        }
    }

    #[test]
    fn multiple_runs_are_scanned_independently() {
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Type {
                text: "rm ".into(),
            },
            Action::Type {
                text: "-rf /".into(),
            },
            Action::Screenshot { note: None },
            Action::Type {
                text: "hello ".into(),
            },
            Action::Type {
                text: "world".into(),
            },
        ]);
        let verdicts = review_batch(&p, &batch);
        assert!(matches!(&verdicts[0], Verdict::NeedsConfirm { .. }));
        assert!(matches!(&verdicts[1], Verdict::NeedsConfirm { .. }));
        assert_eq!(verdicts[2], Verdict::Allow);
        assert_eq!(verdicts[3], Verdict::Allow);
        assert_eq!(verdicts[4], Verdict::Allow);
    }

    #[test]
    fn coordinate_actions_are_never_guarded() {
        let p = Policy::default();
        let batch = Batch(vec![
            Action::Move {
                frame: FrameId(1),
                x: 0,
                y: 0,
            },
            Action::Drag {
                frame: FrameId(1),
                path: vec![],
            },
            Action::Scroll {
                frame: FrameId(1),
                x: 5,
                y: 5,
                dx: 0.0,
                dy: -600.0,
            },
            Action::DoubleClick {
                frame: FrameId(1),
                button: MouseButton::Right,
                x: 1,
                y: 1,
            },
        ]);
        assert!(review_batch(&p, &batch).iter().all(|v| v.allowed()));
    }
}
