//! Pre-execution safety review for action batches.
//!
//! The anaisbetts lesson we said we'd steal: a `guard.py`-style safety
//! overlay that reviews what the model asked for *before* anything moves.
//! The guard is pure (no backends, no clock): it inspects a [`Batch`] and
//! returns one [`Verdict`] per action. The executor applies the verdicts;
//! denied actions become per-action failures without aborting the batch
//! (the "failed actions never abort" contract), and `NeedsConfirm` actions
//! fail until a host layer implements an interactive confirmation path.

use crate::action::{Action, Batch};

/// Per-action verdict from [`review`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Execute normally.
    Allow,
    /// Do not execute. Reason is model-facing so the model can adjust.
    Deny { reason: String },
    /// Destructive enough to want a human nod. The executor treats this as
    /// a failure until a host adds a confirmation channel; the reason is
    /// model-facing.
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
        }
    }
}

/// Normalize a keypress combo for order-insensitive comparison.
fn normalize_combo(keys: &[String]) -> Vec<String> {
    let mut v: Vec<String> = keys.iter().map(|k| k.to_ascii_uppercase()).collect();
    v.sort_unstable();
    v
}

/// Review one action against the policy.
pub fn review(policy: &Policy, action: &Action) -> Verdict {
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
            if let Some(frag) = policy
                .denied_text_fragments
                .iter()
                .find(|f| lower.contains(&f.to_lowercase()))
            {
                return Verdict::Deny {
                    reason: format!("typed text contains denied fragment {:?}", frag),
                };
            }
            if let Some(frag) = policy
                .confirm_text_fragments
                .iter()
                .find(|f| lower.contains(&f.to_lowercase()))
            {
                return Verdict::NeedsConfirm {
                    reason: format!("typed text contains destructive fragment {:?}", frag),
                };
            }
            Verdict::Allow
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
    batch.0.iter().map(|a| review(policy, a)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::MouseButton;
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
