use std::time::{Duration, Instant};

use super::terminal::TerminalCursorState;

pub(crate) const CURSOR_POSITION_SETTLE: Duration = Duration::from_millis(20);
const CURSOR_POSITION_MAX_HOLD: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub(crate) struct DecscusrTracker {
    state: DecscusrParseState,
    cursor_shape_overridden: bool,
}

#[derive(Debug, Default)]
enum DecscusrParseState {
    #[default]
    Ground,
    Escape,
    Csi {
        first_param: Option<u16>,
        collecting_first_param: bool,
        has_space_intermediate: bool,
    },
}

impl DecscusrTracker {
    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.observe_byte(byte);
        }
    }

    fn observe_byte(&mut self, byte: u8) {
        match &mut self.state {
            DecscusrParseState::Ground => {
                if byte == 0x1b {
                    self.state = DecscusrParseState::Escape;
                }
            }
            DecscusrParseState::Escape => {
                self.state = if byte == b'[' {
                    DecscusrParseState::Csi {
                        first_param: None,
                        collecting_first_param: true,
                        has_space_intermediate: false,
                    }
                } else if byte == 0x1b {
                    DecscusrParseState::Escape
                } else {
                    DecscusrParseState::Ground
                };
            }
            DecscusrParseState::Csi {
                first_param,
                collecting_first_param,
                has_space_intermediate,
            } => {
                if byte == 0x1b {
                    self.state = DecscusrParseState::Escape;
                } else if byte.is_ascii_digit() && *collecting_first_param {
                    let digit = u16::from(byte - b'0');
                    *first_param = Some(first_param.unwrap_or(0).saturating_mul(10) + digit);
                } else if byte == b';' || byte == b':' {
                    *collecting_first_param = false;
                } else if byte == b' ' {
                    *has_space_intermediate = true;
                    *collecting_first_param = false;
                } else if (0x40..=0x7e).contains(&byte) {
                    if byte == b'q' && *has_space_intermediate {
                        let param = first_param.unwrap_or(0);
                        if param <= 6 {
                            self.cursor_shape_overridden = param != 0;
                        }
                    }
                    self.state = DecscusrParseState::Ground;
                } else if !(0x20..=0x3f).contains(&byte) {
                    self.state = DecscusrParseState::Ground;
                }
            }
        }
    }

    pub(crate) fn cursor_shape_overridden(&self) -> bool {
        self.cursor_shape_overridden
    }
}

#[derive(Debug, Default)]
pub(crate) struct CursorPositionSettleState {
    settled: Option<TerminalCursorState>,
    candidate: Option<TerminalCursorState>,
    pending_since: Option<Instant>,
    last_observed: Option<Instant>,
}

impl CursorPositionSettleState {
    pub(crate) fn observe(&mut self, current: Option<TerminalCursorState>, now: Instant) {
        let previous_observation = self.last_observed.replace(now);
        let Some(current) = current else {
            self.settled = None;
            self.candidate = None;
            self.pending_since = None;
            return;
        };
        if !current.visible {
            self.settled = Some(current);
            self.candidate = None;
            self.pending_since = None;
            return;
        }

        // A quiet gap means the previous write burst finished, so its last
        // position was the one the child meant. Commit it and start fresh.
        // Without this the max hold below eventually fires on the *first*
        // position of the next burst, which is exactly the mid-redraw position
        // we are trying not to expose, and settling on it makes every later
        // redraw publish it with no hold at all.
        if let (Some(candidate), Some(previous)) = (self.candidate, previous_observation) {
            if now.duration_since(previous) >= CURSOR_POSITION_SETTLE {
                self.settled = Some(candidate);
                self.candidate = None;
                self.pending_since = None;
            }
        }

        let Some(settled) = self.settled else {
            self.settled = Some(current);
            self.candidate = None;
            self.pending_since = None;
            return;
        };
        if same_cursor_position(settled, current) && settled.visible {
            self.settled = Some(current);
            self.candidate = None;
            self.pending_since = None;
            return;
        }

        let Some(candidate) = self.candidate else {
            self.candidate = Some(current);
            self.pending_since = Some(now);
            return;
        };

        let pending_since = self.pending_since.unwrap_or(now);
        if now.duration_since(pending_since) >= CURSOR_POSITION_MAX_HOLD {
            self.settled = Some(current);
            self.candidate = None;
            self.pending_since = None;
        } else if same_cursor_position(candidate, current) {
            if now.duration_since(pending_since) >= CURSOR_POSITION_SETTLE {
                self.settled = Some(current);
                self.candidate = None;
                self.pending_since = None;
            } else {
                self.candidate = Some(current);
            }
        } else {
            self.candidate = Some(current);
        }
    }

    pub(crate) fn reported_cursor(
        &self,
        current: Option<TerminalCursorState>,
        now: Instant,
    ) -> Option<TerminalCursorState> {
        let current = current?;
        let Some(candidate) = self.candidate else {
            return Some(current);
        };
        let pending_since = self.pending_since.unwrap_or(now);
        if now.duration_since(pending_since) >= CURSOR_POSITION_SETTLE {
            return Some(TerminalCursorState {
                visible: current.visible && candidate.visible,
                shape: current.shape,
                ..candidate
            });
        }
        self.settled
            .map(|settled| TerminalCursorState {
                visible: current.visible && settled.visible,
                shape: current.shape,
                ..settled
            })
            .or(Some(TerminalCursorState {
                visible: false,
                shape: current.shape,
                ..candidate
            }))
    }

    pub(crate) fn pending(&self) -> bool {
        self.candidate.is_some()
    }
}

fn same_cursor_position(left: TerminalCursorState, right: TerminalCursorState) -> bool {
    left.x == right.x && left.y == right.y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(x: u16, y: u16, visible: bool, shape: u8) -> TerminalCursorState {
        TerminalCursorState {
            x,
            y,
            visible,
            shape,
        }
    }

    #[test]
    fn cursor_settle_holds_position_change_until_quiet_window() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(20, 5, true, 0)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(Some(cursor(20, 5, true, 0)), now + Duration::from_millis(2))
            .unwrap();

        assert_eq!((reported.x, reported.y), (1, 0));
    }

    #[test]
    fn cursor_settle_adopts_position_change_after_quiet_window() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(2, 0, true, 0)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(
                Some(cursor(2, 0, true, 0)),
                now + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
            )
            .unwrap();

        assert_eq!((reported.x, reported.y), (2, 0));
    }

    #[test]
    fn cursor_settle_caps_continuous_position_changes_from_first_pending_time() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);

        // Churn arriving faster than the settle window never lets the burst
        // end, so only the max hold can release the held position.
        let mut at = now;
        let mut last = 1u16;
        while at.duration_since(now) < CURSOR_POSITION_MAX_HOLD + Duration::from_millis(10) {
            at += Duration::from_millis(10);
            last += 1;
            settle.observe(Some(cursor(last, 0, true, 0)), at);
        }

        assert!(!settle.pending());
        assert_eq!(
            settle.reported_cursor(
                Some(cursor(last, 0, true, 0)),
                at + Duration::from_millis(1)
            ),
            Some(cursor(last, 0, true, 0))
        );
    }

    #[test]
    fn cursor_settle_commits_candidate_when_the_write_burst_ends() {
        // Two redraws separated by an idle gap. The second redraw must not be
        // able to settle on its own mid-redraw position.
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        let shell = cursor(2, 5, true, 0);
        let caret = cursor(7, 34, true, 0);
        let footer = cursor(33, 36, true, 0);

        settle.observe(Some(shell), now);
        settle.observe(Some(footer), now + Duration::from_millis(10));
        settle.observe(Some(caret), now + Duration::from_millis(18));

        settle.observe(Some(footer), now + Duration::from_millis(218));
        let reported = settle
            .reported_cursor(Some(footer), now + Duration::from_millis(220))
            .expect("cursor");

        assert_eq!((reported.x, reported.y), (caret.x, caret.y));
    }

    #[test]
    fn cursor_settle_never_reports_a_mid_redraw_paint_position() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(7, 19, true, 0)), now);

        // A redraw leaves the cursor on the last repainted row before the child
        // places it back on its input line.
        settle.observe(Some(cursor(5, 23, true, 0)), now + Duration::from_millis(1));
        assert_eq!(
            settle.reported_cursor(Some(cursor(5, 23, true, 0)), now + Duration::from_millis(2)),
            Some(cursor(7, 19, true, 0))
        );

        settle.observe(Some(cursor(8, 19, true, 0)), now + Duration::from_millis(3));
        assert_eq!(
            settle.reported_cursor(Some(cursor(8, 19, true, 0)), now + Duration::from_millis(4)),
            Some(cursor(7, 19, true, 0))
        );
        assert_eq!(
            settle.reported_cursor(
                Some(cursor(8, 19, true, 0)),
                now + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
            ),
            Some(cursor(8, 19, true, 0))
        );
    }

    #[test]
    fn cursor_settle_keeps_render_read_pure() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(2, 0, true, 0)), now + Duration::from_millis(1));

        assert!(settle.pending());
        let _ = settle.reported_cursor(
            Some(cursor(2, 0, true, 0)),
            now + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
        );

        assert!(settle.pending());
    }

    #[test]
    fn cursor_settle_passes_shape_through_while_position_is_held() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 2)), now);
        settle.observe(Some(cursor(2, 0, true, 6)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(Some(cursor(2, 0, true, 6)), now + Duration::from_millis(2))
            .unwrap();

        assert_eq!((reported.x, reported.y, reported.shape), (1, 0, 6));
    }

    #[test]
    fn cursor_settle_passes_shape_through_after_quiet_window() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 2)), now);
        settle.observe(Some(cursor(2, 0, true, 2)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(
                Some(cursor(2, 0, true, 6)),
                now + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
            )
            .unwrap();

        assert_eq!((reported.x, reported.y, reported.shape), (2, 0, 6));
    }

    #[test]
    fn cursor_settle_hides_immediately_and_waits_to_reveal() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(1, 0, false, 0)), now + Duration::from_millis(1));

        assert_eq!(
            settle.reported_cursor(Some(cursor(1, 0, false, 0)), now + Duration::from_millis(2)),
            Some(cursor(1, 0, false, 0))
        );

        settle.observe(Some(cursor(1, 0, true, 0)), now + Duration::from_millis(3));
        assert_eq!(
            settle.reported_cursor(Some(cursor(1, 0, true, 0)), now + Duration::from_millis(4)),
            Some(cursor(1, 0, false, 0))
        );
    }
}
